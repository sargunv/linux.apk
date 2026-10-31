//! innkeeper-broker: serves the session's seccomp user notifications
//! (supervisor/README.md).
//!
//!   * [`filter`]: the seccomp filter, installed on Android's init.
//!   * [`notify`]: the serve loop, which dispatches every notification.
//!   * [`binder`]: the driver's state machine (`drivers/android/binder.c`) and
//!     its UAPI. It makes no syscalls: process memory, receive buffers, fds
//!     and answering the caller go through the [`binder::driver::Userspace`]
//!     trait, so it is unit tested on the host against [`fake`].
//!   * `creds`: virtual credentials for Android's uids.
//!   * `fs`: what the Flatpak sandbox's missing mounts would do.
//!   * `bpf`: eBPF objects and the rootless calls netd needs.
//!   * [`input`]: session-local evdev devices the bridge feeds.
//!   * [`ohos`] and [`ashmem`]: OpenHarmony's kernel extensions.
//!
//! `INNKEEPER_GUEST` selects the guest ([`guest`]), `INNKEEPER_BROKER_DEBUG`
//! tracing ([`init_log`]).

pub mod ashmem;
pub mod binder;
mod bpf;
mod creds;
pub mod fake;
pub mod filter;
mod fs;
pub mod input;
pub mod notify;
pub mod ohos;

use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// The guest userspace the session runs. Android's behaviour is the default;
/// OpenHarmony (`ohos/`) adds its kernel extensions on top of the same
/// emulation, and nothing else changes for Android.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guest {
    Android,
    Ohos,
}

/// From `INNKEEPER_GUEST` (`ohos`, or anything else for Android), read once.
pub fn guest() -> Guest {
    static GUEST: std::sync::OnceLock<Guest> = std::sync::OnceLock::new();
    *GUEST.get_or_init(|| match std::env::var("INNKEEPER_GUEST").as_deref() {
        Ok("ohos") => Guest::Ohos,
        _ => Guest::Android,
    })
}

/// Verbose tracing (INNKEEPER_BROKER_DEBUG).
pub static DEBUG: AtomicBool = AtomicBool::new(false);

/// Bounded trace sink: stderr, or a file that is rotated (to `<path>.1`) every
/// 64 MiB so a long boot can never fill the disk.
struct LogSink {
    path: Option<String>,
    file: Option<std::fs::File>,
    written: u64,
}
static LOG: Mutex<LogSink> = Mutex::new(LogSink {
    path: None,
    file: None,
    written: 0,
});
const LOG_ROTATE: u64 = 64 << 20;

/// Configure tracing from INNKEEPER_BROKER_DEBUG: unset, empty or "0" = off,
/// "1" = stderr, anything else = a log file path. Tracing covers every binder
/// call and a line of syscall counters every 10 s.
pub fn init_log() {
    let Some(v) = std::env::var_os("INNKEEPER_BROKER_DEBUG") else {
        return;
    };
    let v = v.to_string_lossy().into_owned();
    if v.is_empty() || v == "0" {
        return;
    }
    DEBUG.store(true, Ordering::Relaxed);
    if v != "1" {
        LOG.lock().unwrap().path = Some(v);
    }
}

pub fn log(args: std::fmt::Arguments) {
    let mut l = LOG.lock().unwrap();
    let Some(path) = l.path.clone() else {
        eprintln!("{args}");
        return;
    };
    if l.file.is_none() || l.written > LOG_ROTATE {
        if l.file.take().is_some() {
            let _ = std::fs::rename(&path, format!("{path}.1"));
        }
        l.file = std::fs::File::create(&path).ok();
        l.written = 0;
    }
    let line = format!("{args}\n");
    l.written += line.len() as u64;
    if let Some(f) = l.file.as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}
