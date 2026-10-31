//! Innkeeper compat harness. See tests/compat/README.md.
//!
//!   mise run test:compat -- [run] [--window] --only PKG,...
//!   mise run test:compat -- fetch [--only PKG,...]
//!
//! `run` fetches the selected apps, makes sure a session is up (starting a
//! headless one if needed, and stopping it at the end), runs each app and
//! writes out/test/compat/<profile>/results.json. It fails unless every app
//! works.

mod app;
mod corpus;
mod frame;
mod session;

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result, bail};

use app::{AppResult, Run, Status};
use session::Session;

struct Opts {
    cmd: String,
    window: bool,
    only: Vec<String>,
}

fn parse_args() -> Result<Opts> {
    let mut o = Opts {
        cmd: "run".into(),
        window: false,
        only: vec![],
    };
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "run" | "fetch" => o.cmd = a,
            "--window" => o.window = true,
            "--only" => {
                let v = args.next().context("--only needs a value")?;
                o.only.extend(v.split(',').map(str::to_string));
            }
            _ => bail!("unknown argument {a} (see tests/compat/src/main.rs)"),
        }
    }
    Ok(o)
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("compat: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Android commands run through `innkeeper-session exec` inherit our
/// niceness, and Android threads that set their priority back to 0 then fail:
/// uiautomator dies at once with "SecurityException: Cannot set priority to
/// 0" ("Killed"). zsh runs background jobs at nice 5 (BG_NICE). So a niced
/// harness runs itself again at nice 0 through the user manager, which can
/// raise priority back where we can't.
fn run_at_nice_zero() -> Result<()> {
    let stat = fs::read_to_string("/proc/self/stat")?;
    // The fields after the command name (which may hold spaces), from
    // field 3 (state) on: nice is field 19.
    let nice: i32 = stat
        .rsplit_once(')')
        .and_then(|(_, f)| f.split_whitespace().nth(16))
        .context("read /proc/self/stat")?
        .parse()?;
    if nice <= 0 || env::var_os("INNKEEPER_RENICED").is_some() {
        return Ok(());
    }
    eprintln!("compat: running at nice {nice}; restarting at nice 0");
    let mut c = Command::new("systemd-run");
    c.args(["--user", "--quiet", "--pipe", "--wait", "--collect"])
        .args(["-p", "Nice=0", "--same-dir"]);
    for (k, v) in env::vars_os() {
        let name = k.to_string_lossy();
        if name.starts_with("INNKEEPER_")
            || name.starts_with("XDG_")
            || [
                "WAYLAND_DISPLAY",
                "PULSE_SERVER",
                "DBUS_SESSION_BUS_ADDRESS",
                "HOME",
                "PATH",
            ]
            .contains(&&*name)
        {
            let mut set = OsString::from("--setenv=");
            set.push(&k);
            set.push("=");
            set.push(&v);
            c.arg(set);
        }
    }
    c.arg("--setenv=INNKEEPER_RENICED=1")
        .arg("--")
        .arg(env::current_exe()?)
        .args(env::args_os().skip(1));
    Err(c.exec()).context("systemd-run")
}

fn real_main() -> Result<ExitCode> {
    let o = parse_args()?;
    if o.cmd == "run" {
        run_at_nice_zero()?;
    }
    // A dev tool: it runs against the checkout it was built in, and its staged
    // session.
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let launcher = repo.join("tools/dev/session.sh");
    let corpus = corpus::load(&repo.join("tests/compat/corpus.toml"))?;

    let apps: Vec<_> = corpus
        .app
        .iter()
        .filter(|a| o.only.is_empty() || o.only.contains(&a.package))
        .collect();
    if o.only.is_empty() {
        bail!("pick apps with --only PKG,...: the full corpus is not a supported run");
    }
    if apps.is_empty() {
        bail!("no apps selected");
    }
    let out = repo
        .join("out/test/compat")
        .join(innkeeper_core::paths::profile()?);
    let apk_dir = repo.join("out/test/compat/corpus");
    let mut apks = vec![];
    for a in &apps {
        apks.push(corpus::fetch(a, &apk_dir)?);
    }
    if o.cmd == "fetch" {
        return Ok(ExitCode::SUCCESS);
    }

    if !launcher.is_file() {
        bail!(
            "no staged session at {} (run `mise run build`)",
            launcher.display()
        );
    }
    let probes = repo.join("out/test/probes/InnkeeperProbes.apk");
    if !probes.is_file() {
        bail!(
            "no probes APK at {} (run `mise run build`)",
            probes.display()
        );
    }
    let s = Session::new(launcher, probes)?;
    s.ensure(o.window)?;
    fs::create_dir_all(&out)?;
    let mut results: Vec<AppResult> = vec![];
    for (a, apk) in apps.iter().zip(&apks) {
        let mut tries = 0;
        loop {
            tries += 1;
            eprintln!("compat: {} {} ({})", a.name, a.version_name, a.package);
            if let Err(e) = s.ensure(o.window) {
                eprintln!("compat: session: {e:#}");
            }
            let pid = s.pid();
            let system = s.system_server_pid();
            let mut r = Run::new(&s, a, &out).run(apk);
            // Android's own restart (SurfaceFlinger or system_server died) in
            // the same session: the app triggered it, so it counts against the
            // app, and the next app waits until the system is back.
            if s.pid() == pid && system.is_some() && s.system_server_pid() != system {
                let why = s.last_fatal();
                eprintln!("compat: Android restarted during the run: {why}");
                r.result = Status::Broken;
                r.reason = format!("Android restarted during the run ({why})");
                r.checks.push(app::Check {
                    name: "no system restart".into(),
                    ok: false,
                    detail: why,
                });
                if let Err(e) = s.wait_system(Duration::from_secs(300)) {
                    eprintln!("compat: {e:#}");
                }
            }
            // The session restarted or died under us (something else stopped
            // the profile, or a host-side crash): not the app's fault, so retry
            // once on a fresh session.
            if pid.is_none() || s.pid() != pid || !s.booted() {
                r.result = Status::Error;
                r.reason = "session restarted or died during the run".into();
                if tries < 2 {
                    eprintln!("compat: session changed during the run, retrying");
                    continue;
                }
            }
            results.push(r);
            break;
        }
    }

    let path = out.join("results.json");
    fs::write(&path, serde_json::to_string_pretty(&results)? + "\n")?;
    let works = results.iter().filter(|r| r.result == Status::Works).count();
    println!(
        "{works} of {} apps work; results in {}",
        results.len(),
        path.display()
    );
    Ok(if works == results.len() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
