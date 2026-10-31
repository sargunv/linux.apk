//! The apps service (id 5): apps installed or removed inside Android (an app
//! store, PackageInstaller, `pm install`) get or lose their desktop entry and
//! icons. The Innkeeper System app reports package events; the entries are
//! written with `innkeeper-core`, the same code as `innkeeper install`.
//!
//! | Type     | Request | Body                                                                                                      | Reply                                                                                                  |
//! | -------- | ------- | --------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
//! | `0x0501` | HAVE    | name package, name version                                                                                | `u8`: 0 missing or outdated (send ADDED), 1 current, 2 not ours (leave it)                             |
//! | `0x0502` | ADDED   | name package, name version, name label, `i32 category` (-1 to 64), `u8 game`, `u8 new_window`, `u32 len`; fd: sealed memfd | The blob is icons: repeated `u32 size`, `u32 len`, PNG (square, ≤ 512, at most 8, ≤ 4 MiB all)         |
//! | `0x0503` | REMOVED | name package                                                                                              |                                                                                                        |
//!
//! At each connection the system app sends HAVE for every launchable user
//! app, so installs made while the bridge was away catch up; then it follows
//! package broadcasts.
//!
//! Entries this service writes say `X-Innkeeper-Origin=android`. An entry
//! written by `innkeeper install` (which has more metadata, such as URL
//! schemes) is never replaced; an entry for another session profile is never
//! touched; an uninstall inside Android removes this profile's entry,
//! whoever wrote it. Without `--mirror-apps` (only the main session passes
//! it) every request succeeds and does nothing, so test sessions don't fill
//! the user's app menu.

use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use innkeeper_core::desktop::{self, AppEntry};
use innkeeper_core::{paths, validate, xdg};

use crate::ipc::blob;
use crate::ipc::server::{Peer, Reply, Service};
use crate::ipc::wire::errno::*;
use crate::ipc::wire::{DecodeError, Reader, Writer};

pub mod file;
pub mod session;

pub mod ty {
    pub const HAVE: u16 = 0x0501;
    pub const ADDED: u16 = 0x0502;
    pub const REMOVED: u16 = 0x0503;
}

/// Largest icon blob (all sizes together).
pub const MAX_ICONS: u32 = 4 << 20;
/// Icons per app.
const MAX_ICON_COUNT: usize = 8;
const MAX_ICON_SIZE: u32 = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Is the desktop entry for `package` at `version` current?
    Have {
        package: String,
        version: String,
    },
    /// `package` was installed or updated; its icons come in a blob.
    Added {
        package: String,
        version: String,
        label: String,
        /// `ApplicationInfo.category` (-1 for none).
        category: i32,
        game: bool,
        new_window: bool,
        icons_len: u32,
    },
    Removed {
        package: String,
    },
}

impl Request {
    pub fn takes_fd(&self) -> bool {
        matches!(self, Request::Added { .. })
    }
}

/// HAVE's answer.
pub mod have {
    /// No entry, or ours for another version: send ADDED.
    pub const MISSING: u8 = 0;
    pub const CURRENT: u8 = 1;
    /// Someone else's entry (`innkeeper install`, another profile), or
    /// mirroring is off: leave it.
    pub const NOT_OURS: u8 = 2;
}

fn package(r: &mut Reader<'_>) -> Result<String, DecodeError> {
    let p = r.name()?;
    validate::package(&p).map_err(|_| DecodeError::Invalid("package name"))?;
    Ok(p)
}

pub fn decode(t: u16, body: &[u8]) -> Result<Request, DecodeError> {
    let mut r = Reader::new(body);
    let req = match t {
        ty::HAVE => Request::Have {
            package: package(&mut r)?,
            version: r.name()?,
        },
        ty::ADDED => {
            let package = package(&mut r)?;
            let version = r.name()?;
            let label = r.name()?;
            let category = r.i32()?;
            if !(-1..=64).contains(&category) {
                return Err(DecodeError::Invalid("category"));
            }
            Request::Added {
                package,
                version,
                label,
                category,
                game: r.bool()?,
                new_window: r.bool()?,
                icons_len: r.u32()?,
            }
        }
        ty::REMOVED => Request::Removed {
            package: package(&mut r)?,
        },
        t => return Err(DecodeError::UnknownType(t)),
    };
    r.end()?;
    Ok(req)
}

pub fn encode(tag: u32, req: &Request) -> Vec<u8> {
    let mut w;
    match req {
        Request::Have { package, version } => {
            w = Writer::message(ty::HAVE, tag);
            w.name(package).name(version);
        }
        Request::Added {
            package,
            version,
            label,
            category,
            game,
            new_window,
            icons_len,
        } => {
            w = Writer::message(ty::ADDED, tag);
            w.name(package)
                .name(version)
                .name(label)
                .i32(*category)
                .u8(*game as u8)
                .u8(*new_window as u8)
                .u32(*icons_len);
        }
        Request::Removed { package } => {
            w = Writer::message(ty::REMOVED, tag);
            w.name(package);
        }
    }
    w.into_vec()
}

/// Icons in a blob: repeated `u32 size`, `u32 len`, `len` bytes of PNG.
/// Only square hicolor sizes up to 512 whose PNG header matches are kept.
pub fn parse_icons(b: &[u8]) -> Result<Vec<(u32, Vec<u8>)>, i32> {
    let mut r = Reader::new(b);
    let mut out = Vec::new();
    while !r.is_empty() {
        let size = r.u32().map_err(|_| -EINVAL)?;
        let len = r.u32().map_err(|_| -EINVAL)? as usize;
        let png = r.take(len).map_err(|_| -EINVAL)?;
        if out.len() >= MAX_ICON_COUNT || size == 0 || size > MAX_ICON_SIZE {
            return Err(-EINVAL);
        }
        if xdg::png_size(png) != Some((size, size)) {
            return Err(-EINVAL);
        }
        out.push((size, png.to_vec()));
    }
    Ok(out)
}

/// Settings shared by every connection.
pub struct Config {
    /// Off: every request is a no-op (other profiles' sessions, tests).
    pub enabled: bool,
    pub mirror_apps: AtomicBool,
    pub dirs: xdg::Dirs,
    /// The session profile entries are made for (see `paths::profile`).
    pub profile: String,
    /// `Exec` prefix: `innkeeper run` (with the profile), the package and
    /// `%U` are appended.
    pub run: Vec<String>,
}

impl Config {
    pub fn from_env(enabled: bool) -> Config {
        let profile = paths::profile().unwrap_or_default();
        let mut run = paths::command(&[xdg::INNKEEPER.into()], &profile);
        run.push("run".into());
        Config {
            enabled,
            mirror_apps: AtomicBool::new(
                innkeeper_core::preferences::Preferences::load()
                    .map(|p| p.mirror_apps)
                    .unwrap_or(true),
            ),
            dirs: xdg::Dirs::from_env(),
            profile,
            run,
        }
    }

    /// The existing entry for `pkg`: (ours, its version), or None.
    fn existing(&self, pkg: &str) -> Option<(bool, String)> {
        let text = std::fs::read_to_string(self.dirs.app_desktop(pkg)).ok()?;
        let exec = desktop::read_key(&text, "Exec").unwrap_or_default();
        let ours = desktop::read_key(&text, "X-Innkeeper-Origin").as_deref() == Some("android")
            && xdg::exec_profile(&exec) == self.profile;
        let version = desktop::read_key(&text, "X-Innkeeper-Version").unwrap_or_default();
        Some((ours, version))
    }

    fn have(&self, pkg: &str, version: &str) -> u8 {
        if !self.enabled || !self.mirror_apps.load(Ordering::Relaxed) {
            return have::NOT_OURS;
        }
        match self.existing(pkg) {
            None => have::MISSING,
            Some((false, _)) => have::NOT_OURS,
            Some((true, v)) if v == version => have::CURRENT,
            Some((true, _)) => have::MISSING,
        }
    }
}

pub struct AppsService {
    cfg: Arc<Config>,
    label: String,
    session: Option<(session::Relay, crate::ipc::server::Events, u64)>,
}

impl AppsService {
    pub fn new(cfg: Arc<Config>, peer: &Peer) -> AppsService {
        AppsService {
            cfg,
            label: peer.label.clone(),
            session: None,
        }
    }

    pub fn with_session(
        mut self,
        relay: session::Relay,
        events: crate::ipc::server::Events,
    ) -> Self {
        self.session = Some((relay, events, 0));
        self
    }

    fn added(
        &self,
        pkg: String,
        version: String,
        label: String,
        categories: Vec<String>,
        new_window: bool,
        icons: Vec<(u32, Vec<u8>)>,
    ) -> Reply {
        if self.cfg.have(&pkg, &version) != have::MISSING {
            return Reply::ok();
        }
        let name = validate::label(&label).unwrap_or_else(|| pkg.clone());
        let mut entry = AppEntry {
            package: pkg.clone(),
            name,
            names: vec![],
            run: self.cfg.run.clone(),
            icon: None,
            version,
            categories,
            schemes: vec![],
            web_hosts: vec![],
            origin: Some("android".into()),
            splash: None,
            new_window,
        };
        // The record is written before the portal's dialog shows, so a HAVE
        // meanwhile finds it current.
        let text = match xdg::write_record(&self.cfg.dirs, &mut entry, &icons) {
            Ok(t) => t,
            Err(e) => {
                crate::warn!("apps: {pkg}: {e:#}");
                return Reply::err(-EIO);
            }
        };
        let cfg = self.cfg.clone();
        let label = self.label.clone();
        let add = move || match xdg::add_to_menu(&cfg.dirs, &pkg, &text, false) {
            Ok(m) => crate::info!("apps: {label}: added {pkg} ({m})"),
            Err(e) => crate::warn!("apps: {pkg}: {e:#}"),
        };
        if let Err(e) = std::thread::Builder::new()
            .name("apps-portal".into())
            .spawn(add)
        {
            crate::warn!("apps: spawn: {e}");
            return Reply::err(-EIO);
        }
        Reply::ok()
    }

    fn removed(&self, pkg: &str) -> Reply {
        if !self.cfg.enabled {
            return Reply::ok();
        }
        // The app is gone from this profile: drop its entry, whoever wrote
        // it, unless it belongs to another profile.
        let Some(text) = std::fs::read_to_string(self.cfg.dirs.app_desktop(pkg)).ok() else {
            return Reply::ok();
        };
        let exec = desktop::read_key(&text, "Exec").unwrap_or_default();
        if xdg::exec_profile(&exec) != self.cfg.profile {
            return Reply::ok();
        }
        xdg::remove_app(&self.cfg.dirs, pkg);
        crate::info!("apps: {}: removed {pkg}", self.label);
        Reply::ok()
    }
}

impl Drop for AppsService {
    fn drop(&mut self) {
        if let Some((relay, _, id)) = &self.session {
            relay.detach(*id);
        }
    }
}

impl Service for AppsService {
    fn connected(&mut self) {
        if let Some((relay, events, id)) = &mut self.session {
            *id = relay.attach(events.clone());
        }
    }

    fn handle(&mut self, t: u16, body: &[u8], mut fds: Vec<OwnedFd>) -> Reply {
        if t == session::BOOT_COMPLETED {
            if !body.is_empty() || !fds.is_empty() {
                return Reply::err(-EPROTO);
            }
            return match &self.session {
                Some((relay, _, id)) => {
                    relay.boot_completed(*id);
                    Reply::ok()
                }
                None => Reply::err(-EPERM),
            };
        }
        if t == session::RESULT {
            return match &self.session {
                Some((relay, _, id)) => match relay.complete(*id, body, fds) {
                    Ok(()) => Reply::ok(),
                    Err(e) => Reply::err(e),
                },
                None => Reply::err(-EPERM),
            };
        }

        let req = match decode(t, body) {
            Ok(r) => r,
            Err(e) => return Reply::err(e.status()),
        };
        if fds.len() != req.takes_fd() as usize {
            return Reply::err(-EPROTO);
        }
        match req {
            Request::Have { package, version } => {
                let mut w = Writer::default();
                w.u8(self.cfg.have(&package, &version));
                Reply::with_body(w.into_vec())
            }
            Request::Added {
                package,
                version,
                label,
                category,
                game,
                new_window,
                icons_len,
            } => {
                let fd = fds.pop().expect("checked above");
                let b = match blob::read(&fd, icons_len, MAX_ICONS as usize) {
                    Ok(b) => b,
                    Err(e) => return Reply::err(e),
                };
                let icons = match parse_icons(&b) {
                    Ok(i) => i,
                    Err(e) => return Reply::err(e),
                };
                self.added(
                    package,
                    version,
                    label,
                    desktop::categories_for((category >= 0).then_some(category), game),
                    new_window,
                    icons,
                )
            }
            Request::Removed { package } => self.removed(&package),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::wire::{HDR, decode_header};

    fn roundtrip(b: &[u8]) -> Result<Request, DecodeError> {
        let h = decode_header(b)?;
        decode(h.ty, &b[HDR..])
    }

    fn png(size: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0]);
        v
    }

    fn icons(list: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut w = Writer::default();
        for (s, p) in list {
            w.u32(*s).u32(p.len() as u32);
            w.0.extend_from_slice(p);
        }
        w.into_vec()
    }

    #[test]
    fn roundtrips() {
        for r in [
            Request::Have {
                package: "org.example.app".into(),
                version: "1.2".into(),
            },
            Request::Added {
                package: "org.example.app".into(),
                version: "1.2 (beta)".into(),
                label: "Example ✓".into(),
                category: 4,
                game: false,
                new_window: true,
                icons_len: 1000,
            },
            Request::Removed {
                package: "org.example.app".into(),
            },
        ] {
            assert_eq!(roundtrip(&encode(1, &r)), Ok(r));
        }
        for bad in ["", "noDot", "../x.y", "a.b/c", "1a.b"] {
            let r = Request::Removed {
                package: bad.into(),
            };
            assert!(roundtrip(&encode(1, &r)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn icons_are_checked() {
        let ok = icons(&[(48, png(48)), (256, png(256))]);
        assert_eq!(parse_icons(&ok).unwrap().len(), 2);
        assert!(parse_icons(&icons(&[(48, png(64))])).is_err()); // size mismatch
        assert!(parse_icons(&icons(&[(1024, png(1024))])).is_err());
        assert!(parse_icons(&icons(&[(48, b"not a png".to_vec())])).is_err());
        assert!(parse_icons(&ok[..ok.len() - 1]).is_err());
        let many: Vec<_> = (0..9).map(|_| (48, png(48))).collect();
        assert!(parse_icons(&icons(&many)).is_err());
    }

    #[test]
    fn mirror_writes_and_removes_only_its_own_entries() {
        let tmp = crate::test_dir("apps-mirror");
        let cfg = Arc::new(Config {
            enabled: true,
            mirror_apps: AtomicBool::new(true),
            dirs: xdg::Dirs {
                data: tmp.join("data"),
                config: tmp.join("config"),
                cache: tmp.join("cache"),
                portal: None,
            },
            profile: "t".into(),
            run: [
                paths::command(&["innkeeper".into()], "t"),
                vec!["run".into()],
            ]
            .concat(),
        });
        let exec = |pkg: &str| {
            let mut run = cfg.run.clone();
            run.push(pkg.into());
            desktop::exec_line(&run, Some("%U")).unwrap()
        };
        let peer = Peer::test();
        let s = AppsService::new(cfg.clone(), &peer);
        assert_eq!(cfg.have("org.ex.a", "1"), have::MISSING);
        let r = s.added(
            "org.ex.a".into(),
            "1".into(),
            "A".into(),
            desktop::categories_for(None, false),
            true,
            vec![(48, png(48))],
        );
        assert_eq!(r.status, 0);
        let text = std::fs::read_to_string(cfg.dirs.app_desktop("org.ex.a")).unwrap();
        assert!(text.contains("X-Innkeeper-Origin=android"), "{text}");
        assert!(text.contains("[Desktop Action NewWindow]"), "{text}");
        assert!(
            text.contains(&format!("Exec={}\n", exec("org.ex.a"))),
            "{text}"
        );
        assert!(
            cfg.dirs
                .icon_path(48, "dev.sargunv.innkeeper.org.ex.a")
                .is_file()
        );
        assert_eq!(cfg.have("org.ex.a", "1"), have::CURRENT);
        assert_eq!(cfg.have("org.ex.a", "2"), have::MISSING);
        let no_version = text.replace("X-Innkeeper-Version=1\n", "");
        std::fs::write(cfg.dirs.app_desktop("org.ex.a"), no_version).unwrap();
        assert_eq!(cfg.have("org.ex.a", ""), have::CURRENT);
        // An entry from `innkeeper install` is left alone, and so is another
        // profile's.
        let cli = format!("[Desktop Entry]\nName=B\nExec={}\n", exec("org.ex.b"));
        std::fs::write(cfg.dirs.app_desktop("org.ex.b"), cli).unwrap();
        assert_eq!(cfg.have("org.ex.b", "1"), have::NOT_OURS);
        let other =
            "[Desktop Entry]\nName=C\nX-Innkeeper-Origin=android\nExec=innkeeper run org.ex.c %U\n";
        std::fs::write(cfg.dirs.app_desktop("org.ex.c"), other).unwrap();
        assert_eq!(cfg.have("org.ex.c", "1"), have::NOT_OURS);
        s.removed("org.ex.c");
        assert!(cfg.dirs.app_desktop("org.ex.c").is_file());
        // Uninstalled here: the entry goes, whoever wrote it.
        s.removed("org.ex.a");
        s.removed("org.ex.b");
        assert!(!cfg.dirs.app_desktop("org.ex.a").exists());
        assert!(
            !cfg.dirs
                .icon_path(48, "dev.sargunv.innkeeper.org.ex.a")
                .exists()
        );
        assert!(!cfg.dirs.app_desktop("org.ex.b").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
