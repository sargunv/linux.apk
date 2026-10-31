//! The live Android session the harness drives: core's launcher driver, plus
//! what only the harness needs (output as text, Android restarts, the bridge
//! log).

use std::cell::Cell;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use innkeeper_core::launcher;

/// A session it started itself is stopped again when this is dropped, so a
/// run leaves nothing running that it started.
pub struct Session {
    core: launcher::Session,
    started: Cell<bool>,
    /// The dev driver (tools/dev/session.sh) that `core` runs.
    driver: PathBuf,
    /// The host bridge's log, in the session's state directory.
    bridge_log: PathBuf,
    /// InnkeeperProbes.apk (out/test/probes), for its input harness.
    pub probes: PathBuf,
}

impl Deref for Session {
    type Target = launcher::Session;
    fn deref(&self) -> &launcher::Session {
        &self.core
    }
}

/// A command's result: whether it exited 0, and its stdout followed by its
/// stderr (pm and am print their failures there).
pub struct Output {
    pub ok: bool,
    pub text: String,
}

impl Session {
    pub fn new(driver: PathBuf, probes: PathBuf) -> Result<Self> {
        let o = Command::new(&driver).arg("paths").output()?;
        let paths: serde_json::Value = serde_json::from_slice(&o.stdout)
            .with_context(|| format!("{} paths", driver.display()))?;
        let state = paths["state"].as_str().context("no state dir")?;
        Ok(Self {
            core: launcher::Session::at(driver.clone()),
            bridge_log: Path::new(state).join("bridge.log"),
            driver,
            started: Cell::new(false),
            probes,
        })
    }

    /// Runs `cmd` inside the session as root, killed after `secs`.
    pub fn run(&self, secs: u64, cmd: &[&str]) -> Output {
        match self.exec(secs, cmd) {
            Ok(o) => {
                let mut text = o.text();
                for l in o.stderr.lines() {
                    text.push_str(l);
                    text.push('\n');
                }
                Output { ok: o.ok(), text }
            }
            Err(e) => Output {
                ok: false,
                text: format!("{e:#}"),
            },
        }
    }

    /// Shell snippet inside the session.
    pub fn sh(&self, secs: u64, script: &str) -> Output {
        self.run(secs, &["sh", "-c", script])
    }

    /// Copies a file out of the session.
    pub fn pull(&self, src: &str, dst: &Path) -> Result<()> {
        let o = self.exec(60, &["cat", src])?;
        if !o.ok() {
            bail!("pull {src} failed: {}", o.stderr.trim());
        }
        fs::write(dst, o.stdout)?;
        Ok(())
    }

    /// The bridge log's current size, so a step can look only at what comes
    /// after.
    pub fn bridge_log_len(&self) -> u64 {
        fs::metadata(&self.bridge_log).map_or(0, |m| m.len())
    }

    /// The bridge log from byte `from` on (at most its last MiB).
    pub fn bridge_log(&self, from: u64) -> String {
        let Ok(mut f) = File::open(&self.bridge_log) else {
            return String::new();
        };
        let len = f.metadata().map_or(0, |m| m.len());
        let start = from.min(len).max(len.saturating_sub(1 << 20));
        let mut b = Vec::new();
        if f.seek(SeekFrom::Start(start)).is_ok() {
            let _ = f.read_to_end(&mut b);
        }
        String::from_utf8_lossy(&b).into_owned()
    }

    /// Makes sure a session is running and booted. Starts one (headless unless
    /// `window`) if none is running.
    pub fn ensure(&self, window: bool) -> Result<()> {
        let status = |m: &str| eprintln!("compat: {m}");
        if !self.running() {
            self.started.set(true);
            self.start(!window, &status)?;
        }
        let t = Instant::now();
        self.wait_booted(&status)?;
        if t.elapsed() > Duration::from_secs(1) {
            eprintln!("compat: session booted ({:.0?} wait)", t.elapsed());
        }
        self.prepare_android();
        Ok(())
    }

    /// Per-boot setup: a bigger log buffer, screen awake and unlocked.
    fn prepare_android(&self) {
        self.run(30, &["logcat", "-G", "16M"]);
        self.run(15, &["input", "keyevent", "WAKEUP"]);
        self.run(15, &["wm", "dismiss-keyguard"]);
    }

    /// Pid of Android's system_server, if running.
    pub fn system_server_pid(&self) -> Option<String> {
        let o = self.run(15, &["pidof", "system_server"]);
        let p = o.text.trim().to_string();
        (o.ok && !p.is_empty()).then_some(p)
    }

    /// The last native crash in the crash buffer: process and abort message.
    pub fn last_fatal(&self) -> String {
        let log = self
            .run(20, &["logcat", "-d", "-b", "crash", "-t", "300"])
            .text;
        let proc = log
            .lines()
            .rfind(|l| l.contains(">>> "))
            .and_then(|l| l.split(">>> ").nth(1))
            .map(|p| p.trim_end_matches(" <<<").trim().to_string());
        let msg = log
            .lines()
            .rfind(|l| l.contains("Abort message: "))
            .and_then(|l| l.split("Abort message: ").nth(1))
            .map(|m| m.trim().trim_matches('\'').to_string());
        match (proc, msg) {
            (Some(p), Some(m)) => format!("{p}: {m}"),
            (Some(p), None) => format!("{p} crashed"),
            (None, _) => "system_server restarted".into(),
        }
    }

    /// Waits until a restarted Android userspace serves activities again.
    pub fn wait_system(&self, limit: Duration) -> Result<()> {
        let t = Instant::now();
        let mut last = None;
        while t.elapsed() < limit {
            let pid = self.system_server_pid();
            let home = self.run(
                20,
                &[
                    "cmd",
                    "package",
                    "resolve-activity",
                    "--brief",
                    "-a",
                    "android.intent.action.MAIN",
                    "-c",
                    "android.intent.category.HOME",
                ],
            );
            if pid.is_some() && pid == last && home.text.contains('/') {
                sleep(Duration::from_secs(10));
                eprintln!("compat: Android is back ({:.0?})", t.elapsed());
                return Ok(());
            }
            last = pid;
            sleep(Duration::from_secs(5));
        }
        bail!("Android did not come back in {limit:?}")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.started.get() {
            eprintln!("compat: stopping the session this run started");
            if let Err(e) = self.stop() {
                eprintln!("compat: {e:#}");
            }
            let _ = Command::new(&self.driver).arg("reset").status();
        }
    }
}
