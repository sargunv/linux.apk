//! `innkeeper-bridge --run-dir DIR --state-dir DIR [--allow-uid UID]...
//!                  [--system-app-dir DIR] [--test-session]
//!                  [--sink NAME] [-v]`
//!
//! Listens on `<run-dir>/bridge/bridge.sock` (the launcher binds
//! `<run-dir>/bridge` into the Android session), keeps MPRIS artwork in
//! `<run-dir>/art` and writes the system app's activity reports to
//! `<run-dir>/lifecycle` for the launcher. Document grants persist in
//! `<state-dir>/documents.grants`.
//!
//! - `--allow-uid`: host uids that may connect (default: our own).
//! - `--system-app-dir`: the Innkeeper System app's data directory as seen
//!   from the host; its owner may use the system, documents, apps,
//!   hoststate, input and a11y services. Nobody else may use input, a11y or
//!   apps.
//! - `--test-session` (also `INNKEEPER_TEST=1`): isolates the clipboard,
//!   portals, OpenURI, notifications, MPRIS, AT-SPI and location from the
//!   desktop, and gives apps installed inside Android no desktop entries.
//! - `--sink`: the PipeWire sink for audio (default: the desktop's default).
//! - `-v`: per-stream statistics.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use std::sync::atomic::AtomicUsize;

use innkeeper_bridge::audio::{AudioService, Backend, Limits};
use innkeeper_bridge::display::{DisplayService, Limits as DisplayLimits};
use innkeeper_bridge::input::{Hub, InputService};
use innkeeper_bridge::ipc::proto::service;
use innkeeper_bridge::ipc::server::{self, Config, Registry, UidCheck};
use innkeeper_bridge::ipc::transport;
use innkeeper_bridge::system::{Shared, SystemService};

fn main() -> Result<()> {
    let mut run_dir: Option<PathBuf> = None;
    let mut state_dir: Option<PathBuf> = None;
    let mut allow = Vec::new();
    let mut sink = None;
    let mut system_app_dir: Option<PathBuf> = None;
    let mut test_session = std::env::var("INNKEEPER_TEST").is_ok_and(|v| v == "1");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--run-dir" => run_dir = Some(args.next().context("--run-dir needs a path")?.into()),
            "--state-dir" => {
                state_dir = Some(args.next().context("--state-dir needs a path")?.into())
            }
            "--allow-uid" => allow.push(
                args.next()
                    .context("--allow-uid needs a uid")?
                    .parse()
                    .context("bad uid")?,
            ),
            "--sink" => sink = Some(args.next().context("--sink needs a name")?),
            "--test-session" => test_session = true,
            "--system-app-dir" => {
                system_app_dir = Some(args.next().context("--system-app-dir needs a path")?.into())
            }
            "-v" | "--verbose" => innkeeper_bridge::log::set_verbose(true),
            _ => bail!("unknown argument {a:?} (see the source header for usage)"),
        }
    }
    let desktop = !test_session;
    let run_dir = run_dir.context("--run-dir is required")?;
    let state_dir = state_dir.context("--state-dir is required")?;
    let socket = run_dir.join("bridge/bridge.sock");
    // A report from before a restart is stale; the system app sends a new one.
    let _ = std::fs::remove_file(run_dir.join("lifecycle"));
    if allow.is_empty() {
        allow.push(rustix::process::getuid().as_raw());
    }
    let backend = Backend::Pulse {
        sink,
        source: std::env::var("INNKEEPER_AUDIO_SOURCE").ok(),
    };
    innkeeper_bridge::info!("audio: {backend:?}");
    let mut services = Registry::default();
    let streams = Arc::new(AtomicUsize::new(0));
    // The composer's displays in host windows (needs $WAYLAND_DISPLAY), and
    // the system app's side of them: window requests, the seat, the IME.
    let hub = Hub::new();
    *hub.evdev.lock().unwrap() = Some(innkeeper_bridge::input::evdev::Evdev::new(
        run_dir.join("bridge/evdev.sock"),
    )?);
    innkeeper_bridge::input::gamepad::start(run_dir.join("bridge/evdev.sock"));
    // Not in the socket directory, which is shared with Android.
    let shared = Shared::new(&run_dir, desktop, hub.clone());
    // The system app names the playing app; each audio connection applies
    // that name to the playback streams it has open and the next one it opens.
    let audio_shared = shared.clone();
    services.register(service::AUDIO, "audio", move |peer, _events| {
        let opened = Arc::default();
        let app = audio_shared.register_playback(&opened);
        Ok(Box::new(AudioService::with_playback(
            backend.clone(),
            Limits::default(),
            streams.clone(),
            peer,
            app,
            opened,
        )))
    });
    let display_shared = shared.clone();
    let display_hub = hub.clone();
    let session_hub = hub.clone();
    let a11y_hub = hub.clone();
    services.register(service::DISPLAY, "display", move |peer, events| {
        Ok(Box::new(DisplayService::new(
            DisplayLimits::default(),
            display_hub.clone(),
            peer,
            events,
            display_shared.clone(),
        )?))
    });
    services.register(service::INPUT, "input", move |peer, events| {
        Ok(Box::new(InputService::new(hub.clone(), peer, events)))
    });
    let for_preferences = shared.clone();
    let for_signals = shared.clone();
    services.register(service::SYSTEM, "system", move |peer, events| {
        Ok(Box::new(SystemService::new(shared.clone(), peer, events)))
    });
    // On SIGTERM (session stop) or SIGINT, close our desktop notifications
    // before exiting; they would otherwise outlive the session.
    let mut signals = signal_hook::iterator::Signals::new([
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
    ])?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if let Some(sig) = signals.forever().next() {
                innkeeper_bridge::info!("signal {sig}: shutting down");
                for_signals.shutdown();
                std::process::exit(0);
            }
        })?;
    // Android apps' accessibility trees on AT-SPI, while the desktop's
    // assistive technologies are on (never in a test session on the user's
    // own bus).
    let a11y = innkeeper_bridge::a11y::Shared::new(desktop);
    let focus = a11y.clone();
    a11y_hub.watch_focus(Box::new(move |w| focus.keyboard_focus(w)));
    services.register(service::A11Y, "a11y", move |peer, events| {
        Ok(Box::new(innkeeper_bridge::a11y::A11yService::new(
            a11y.clone(),
            peer,
            events,
        )))
    });
    // The host's battery, power saver, network and location. Location asks
    // the user, so a test session on the real desktop gets none.
    let host_state = innkeeper_bridge::hoststate::HostState::new(desktop);
    services.register(service::HOSTSTATE, "hoststate", move |peer, events| {
        Ok(Box::new(
            innkeeper_bridge::hoststate::HostStateService::new(host_state.clone(), peer, events),
        ))
    });
    // Host files the user picks for Android.
    let store = innkeeper_bridge::documents::Store::new(Some(state_dir.join("documents.grants")));
    services.register(service::DOCUMENTS, "documents", move |peer, events| {
        Ok(Box::new(
            innkeeper_bridge::documents::DocumentsService::new(
                store.clone(),
                peer,
                events,
                desktop,
            ),
        ))
    });
    // Apps installed inside Android get desktop entries, except in a test
    // session.
    let session_api = innkeeper_bridge::apps::session::Relay::default();
    let apps = Arc::new(innkeeper_bridge::apps::Config::from_env(desktop));
    let _session_bus = match session_api.serve(for_preferences, apps.clone(), session_hub) {
        Ok(bus) => Some(bus),
        Err(e) => {
            innkeeper_bridge::warn!("session D-Bus: {e:#}");
            None
        }
    };
    innkeeper_bridge::info!("apps: mirroring {}", if desktop { "on" } else { "off" });
    services.register(service::APPS, "apps", move |peer, events| {
        Ok(Box::new(
            innkeeper_bridge::apps::AppsService::new(apps.clone(), peer)
                .with_session(session_api.clone(), events),
        ))
    });
    let mut service_uids: Vec<(u32, UidCheck)> = Vec::new();
    if let Some(dir) = system_app_dir {
        // The system app's uid is assigned at install time; the owner of its
        // data directory (created by installd) is the source of truth.
        let check: UidCheck = Arc::new(move |uid| {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(&dir).is_ok_and(|m| m.uid() == uid)
        });
        for id in [
            service::SYSTEM,
            service::DOCUMENTS,
            service::APPS,
            service::INPUT,
            service::A11Y,
            service::HOSTSTATE,
        ] {
            service_uids.push((id, check.clone()));
        }
    }
    let cfg = Config {
        allow_uids: allow,
        service_uids,
        private_services: vec![service::INPUT, service::A11Y, service::APPS],
        max_clients: 32,
        services,
    };
    let listener = transport::listen(&socket)?;
    innkeeper_bridge::info!(
        "listening on {} (uids {:?}, services {:?})",
        socket.display(),
        cfg.allow_uids,
        cfg.services.names()
    );
    server::serve(listener, Arc::new(cfg))
}
