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

use crate::budget::Measured;

/// How long the session idles after a cold start before its memory is read:
/// past the 60 s after boot in which Android keeps the processes the boot
/// broadcasts started (init.innkeeper.rc).
const IDLE_SETTLE: Duration = Duration::from_secs(90);

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

    /// Starts the run's session (headless unless `window`) and measures it,
    /// or uses the one running (and measures nothing). A fresh /data's first
    /// boot does extra work, so the cold start timed is a second start; idle
    /// memory is read [`IDLE_SETTLE`] after it.
    pub fn start_measured(&self, window: bool) -> Result<Measured> {
        if self.running() {
            eprintln!("compat: using the running session; not measuring its start or idle memory");
            self.ensure(window)?;
            return Ok(Measured::default());
        }
        self.started.set(true);
        let first = self.boot_timed(window)?;
        eprintln!("compat: session booted in {first:.1?}; restarting it to time a cold start");
        self.stop()?;
        let cold = self.boot_timed(window)?;
        eprintln!("compat: cold session start {cold:.1?}; idle for {IDLE_SETTLE:?}");
        sleep(IDLE_SETTLE);
        let idle = self.memory_without_cache();
        if let Some(mib) = idle {
            eprintln!("compat: idle session memory {mib} MiB");
        }
        self.prepare_android();
        Ok(Measured {
            session_start_ms: Some(cold.as_millis() as u64),
            idle_memory_mib: idle,
            app_start_ms: None,
        })
    }

    /// Starts the session and waits until Android has booted, polling more
    /// often than [`launcher::Session::wait_booted`] does.
    fn boot_timed(&self, window: bool) -> Result<Duration> {
        // Admission waits for host memory are not Android startup time.
        let ready = Command::new(&self.driver).arg("wait").status()?;
        if !ready.success() {
            bail!("the session driver could not admit a boot");
        }
        let t = Instant::now();
        self.start(!window, &|m: &str| eprintln!("compat: {m}"))?;
        while !self.booted() {
            if !self.running() {
                bail!("the session stopped while booting (see `innkeeper-session log`)");
            }
            if t.elapsed() > Duration::from_secs(300) {
                bail!("the session did not finish booting in 300 s");
            }
            sleep(Duration::from_millis(100));
        }
        Ok(t.elapsed())
    }

    /// The dev driver's Flatpak scope.
    fn cgroup(&self) -> Option<PathBuf> {
        let o = Command::new(&self.driver).arg("cgroup").output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().into())
    }

    /// The session scope's memory use (`memory.current`), in bytes.
    pub fn memory_current(&self) -> Option<u64> {
        fs::read_to_string(self.cgroup()?.join("memory.current"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The session cgroup's memory without page cache (`memory.current`
    /// less the file pages that aren't shmem), in MiB. Page cache is charged
    /// to whichever cgroup read a file first, so it depends on what else has
    /// read the image.
    pub fn memory_without_cache(&self) -> Option<u64> {
        let current = self.memory_current()?;
        let stat = fs::read_to_string(self.cgroup()?.join("memory.stat")).ok()?;
        let field = |k: &str| {
            stat.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix(' ')?.parse::<u64>().ok())
        };
        Some((current - field("file")? + field("shmem")?) >> 20)
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

    /// Whether the composer runs without host windows.
    pub fn headless(&self) -> bool {
        self.getprop("vendor.innkeeper.hwc.backend") == "headless"
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
