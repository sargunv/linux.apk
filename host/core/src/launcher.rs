//! Driving the Android session through `innkeeper-session` (the prefix's, or
//! any launcher for test harnesses). Its state (running, the image) comes
//! from the launcher's library, or, for a launcher outside a prefix (the dev
//! driver, tools/dev/session.sh), from its `status` and `image`; starting, stopping and commands go through the launcher
//! itself. Commands run inside the session
//! as argv through `innkeeper-session exec`, which hands them to the exec
//! relay as argv: no string is ever interpreted by a shell, host or Android.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use innkeeper_session::Paths;

use crate::paths::Prefix;
use crate::proc;

pub struct Session {
    launcher: PathBuf,
    /// The profile it runs, if not the environment's.
    profile: Option<String>,
}

/// Lines of session stderr worth keeping (the session's linker warns there).
fn clean_stderr(s: &str) -> String {
    s.lines()
        .filter(|l| !l.contains("linker"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What [`Session::status`] reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    pub running: bool,
    pub booted: bool,
}

impl Session {
    /// The session of this program's prefix and profile.
    pub fn find() -> Result<Self> {
        Ok(Self::at(Prefix::locate()?.launcher()))
    }

    /// The session `launcher` runs (for harnesses outside a prefix).
    pub fn at(launcher: PathBuf) -> Self {
        Self {
            launcher,
            profile: None,
        }
    }

    /// The session of profile `name` of this program's prefix.
    pub fn of_profile(name: &str) -> Result<Self> {
        innkeeper_session::profile::check_name(name)?;
        Ok(Self {
            profile: Some(name.into()),
            ..Self::find()?
        })
    }

    /// The launcher, run for this session's profile.
    fn command(&self) -> Command {
        let mut c = Command::new(&self.launcher);
        if let Some(p) = self.profile.clone().or(crate::paths::profile().ok()) {
            c.env("INNKEEPER_PROFILE", p);
        }
        c
    }

    /// The launcher library's view of this launcher's session.
    pub fn paths(&self) -> Option<Paths> {
        let prefix = self.launcher.parent()?.parent()?.parent()?;
        match &self.profile {
            Some(p) => Paths::for_profile(prefix, p).ok(),
            None => Paths::new(prefix).ok(),
        }
    }

    fn state(&self) -> Option<innkeeper_session::Status> {
        match self.paths() {
            Some(paths) => Some(innkeeper_session::status(&paths)),
            None => Some(innkeeper_session::Status::parse(&self.query("status")?)),
        }
    }

    pub fn running(&self) -> bool {
        self.pid().is_some()
    }

    /// Pid of the session's Android init (it changes when the session
    /// restarts).
    pub fn pid(&self) -> Option<u32> {
        self.state()?.pid.map(|p| p as u32)
    }

    /// The output of a launcher command that only reads state, if it
    /// succeeds.
    fn query(&self, command: &str) -> Option<String> {
        let o = self.command().arg(command).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    /// Runs argv inside the session as root; returns (success, stdout, stderr).
    pub fn exec_with(&self, secs: u64, argv: &[&str], stdin: Stdio) -> Result<proc::Output> {
        let mut c = self.command();
        c.arg("exec").args(argv);
        let mut o = proc::run(&mut c, stdin, Duration::from_secs(secs))?;
        o.stderr = clean_stderr(&o.stderr);
        Ok(o)
    }

    pub fn exec(&self, secs: u64, argv: &[&str]) -> Result<proc::Output> {
        self.exec_with(secs, argv, Stdio::null())
    }

    /// Runs argv, failing with its output unless it exits 0.
    pub fn exec_ok(&self, secs: u64, argv: &[&str]) -> Result<String> {
        let o = self.exec(secs, argv)?;
        let text = o.text();
        if !o.ok() {
            bail!(
                "{} failed: {} {}",
                argv.join(" "),
                text.trim(),
                o.stderr.trim()
            );
        }
        Ok(text)
    }

    /// Writes a host file to a fixed path inside the session (`dst` must be a
    /// constant, not derived from input).
    pub fn push_fixed(&self, src: &Path, dst: &'static str) -> Result<()> {
        let f = File::open(src).with_context(|| format!("open {}", src.display()))?;
        // `$1` is the destination; the script itself is a constant.
        let o = self.exec_with(
            120,
            &[
                "sh",
                "-c",
                "cat > \"$1.tmp\" && chmod 0444 \"$1.tmp\" && mv -f \"$1.tmp\" \"$1\"",
                "sh",
                dst,
            ],
            f.into(),
        )?;
        if !o.ok() {
            bail!("copy to {dst} failed: {}", o.stderr.trim());
        }
        Ok(())
    }

    pub fn getprop(&self, name: &str) -> String {
        self.exec(15, &["getprop", name])
            .map(|o| o.text().trim().to_string())
            .unwrap_or_default()
    }

    /// Stops the session (no-op if it isn't running).
    pub fn stop(&self) -> Result<()> {
        let o = proc::run(
            self.command().arg("stop"),
            Stdio::null(),
            Duration::from_secs(120),
        )?;
        if !o.ok() {
            bail!("session stop failed: {}", o.stderr.trim());
        }
        Ok(())
    }

    /// Running and boot state, for status displays.
    pub fn status(&self) -> Status {
        let Some(s) = self.state() else {
            return Status::default();
        };
        if s.pid.is_none() {
            return Status::default();
        }
        Status {
            running: true,
            booted: self.booted(),
        }
    }

    pub fn booted(&self) -> bool {
        self.getprop("sys.boot_completed") == "1"
    }

    /// Whether the image is fetched, extracted and flattened (so starting
    /// doesn't download it first).
    pub fn image_ready(&self) -> bool {
        let dirs = match self.paths() {
            Some(paths) => paths.image().ok().map(|i| (i.img, i.apexdir)),
            None => self.query("image").and_then(|text| {
                let dir = |key: &str| {
                    text.lines()
                        .find_map(|l| l.strip_prefix(key))
                        .map(PathBuf::from)
                };
                Some((dir("img=")?, dir("apex=")?))
            }),
        };
        dirs.is_some_and(|(img, apex)| {
            img.join("system/system").is_dir() && apex.join("apex-info-list.xml").is_file()
        })
    }

    /// Fetches and unpacks the image (about 1.2 GB to download). Its
    /// progress goes to stderr.
    pub fn prepare(&self) -> Result<()> {
        self.run_prepare()
            .context(crate::ErrorKind::ImageNotPrepared)
    }

    fn run_prepare(&self) -> Result<()> {
        let st = self
            .command()
            .arg("prepare")
            .stdin(Stdio::null())
            .stdout(io::stderr())
            .status()?;
        if !st.success() {
            bail!(
                "preparing the Android image failed ({} prepare)",
                self.launcher.display()
            );
        }
        Ok(())
    }

    /// Starts the session (headless: with no windows, for tests), preparing
    /// the image first if this is the first start. Progress goes to `status`.
    pub fn start(&self, headless: bool, status: &dyn Fn(&str)) -> Result<()> {
        if !self.image_ready() {
            status(PREPARING);
            self.run_prepare()
                .context(crate::ErrorKind::ImageNotPrepared)?;
        }
        status("starting the Android session");
        let mut c = self.command();
        c.arg("start").stdin(Stdio::null()).stdout(Stdio::null());
        if headless {
            c.arg("--headless");
        }
        if !c
            .status()
            .context(crate::ErrorKind::SessionStartFailed)?
            .success()
        {
            return Err(anyhow::anyhow!(
                "session start failed ({} start)",
                self.launcher.display()
            )
            .context(crate::ErrorKind::SessionStartFailed));
        }
        Ok(())
    }

    /// Waits for boot and user 0's credential-encrypted apps. Progress goes to `status`.
    pub fn wait_booted(&self, status: &dyn Fn(&str)) -> Result<()> {
        let t = Instant::now();
        let limit = Duration::from_secs(300);
        let mut said = false;
        // Boot completion can precede user unlock. Until then PackageManager
        // filters out direct-boot-unaware launcher activities, even for root.
        while !self.booted()
            || !self
                .exec(15, &["am", "get-started-user-state", "0"])
                .is_ok_and(|o| o.ok() && o.text().trim() == "RUNNING_UNLOCKED")
        {
            if !self.running() {
                return Err(anyhow::anyhow!(
                    "the session stopped while booting (see `innkeeper-session log`)"
                )
                .context(crate::ErrorKind::SessionStartFailed));
            }
            if t.elapsed() > limit {
                return Err(
                    anyhow::anyhow!("the session did not finish booting in {limit:?}")
                        .context(crate::ErrorKind::SessionStartFailed),
                );
            }
            if !said && t.elapsed() > Duration::from_secs(3) {
                status("waiting for Android to boot");
                said = true;
            }
            sleep(Duration::from_secs(1));
        }
        if said {
            // Freshly booted: wake the display and dismiss the keyguard.
            let _ = self.exec(15, &["input", "keyevent", "WAKEUP"]);
            let _ = self.exec(15, &["wm", "dismiss-keyguard"]);
        }
        Ok(())
    }

    /// Makes sure a session is running and booted, starting one (with
    /// windows) if needed. Progress goes to `status`.
    pub fn ensure(&self, status: &dyn Fn(&str)) -> Result<()> {
        if !self.running() {
            self.start(false, status)?;
        }
        self.wait_booted(status)
    }
}

/// The status line for a first start's image download.
pub const PREPARING: &str = "preparing Android (first run: downloading about 1.2 GB)";

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn cold_open_waits_for_user_unlock() {
        let dir = crate::test_dir("cold-open-unlock");
        let launcher = dir.join("session");
        std::fs::write(
            &launcher,
            r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
case "$*" in
  status) echo 'running: host pid 1' ;;
  'exec getprop sys.boot_completed') echo 1 ;;
  'exec am get-started-user-state 0')
    n=$(cat attempts 2>/dev/null || echo 0)
    n=$((n + 1)); echo "$n" >attempts
    case "$n" in
      1) exit 1 ;;
      2) echo RUNNING_LOCKED ;;
      3) echo RUNNING_UNLOCKING ;;
      *) touch unlocked; echo RUNNING_UNLOCKED ;;
    esac ;;
  'exec cmd package resolve-activity --brief --user current -a android.intent.action.MAIN -c android.intent.category.LAUNCHER dev.sargunv.innkeeper.proof')
    echo resolve >>lookups
    if test -f unlocked; then echo dev.sargunv.innkeeper.proof/.MainActivity
    else echo 'No activity found'; fi ;;
  *) exit 1 ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let session = Session::at(launcher);
        session.ensure(&|_| {}).unwrap();
        assert_eq!(
            crate::android::launcher_component(&session, "dev.sargunv.innkeeper.proof").unwrap(),
            "dev.sargunv.innkeeper.proof/.MainActivity"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("lookups")).unwrap(),
            "resolve\n"
        );
    }
}
