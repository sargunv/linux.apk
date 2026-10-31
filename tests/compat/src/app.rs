//! One app through the harness: install, launch, first frame, interactions,
//! logs, uninstall.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::corpus::App;
use crate::frame;
use crate::session::Session;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Works,
    Partly,
    Broken,
    /// The harness or session failed, not the app; not a result.
    Error,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Works => "Works",
            Status::Partly => "Partly works",
            Status::Broken => "Doesn't work",
            Status::Error => "Not tested",
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct AppResult {
    pub package: String,
    pub name: String,
    pub group: String,
    pub version_name: String,
    pub version_code: u64,
    pub result: Status,
    pub reason: String,
    /// Time to the first frame on a fresh install (ActivityTaskManager's
    /// "Displayed").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_ms: Option<u64>,
    /// Total PSS of the app process after the interactions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pss_mb: Option<u64>,
    pub checks: Vec<Check>,
}

const TMP_APK: &str = "/data/local/tmp/compat.apk";
const UI_XML: &str = "/data/local/tmp/compat-ui.xml";
/// How long one `uiautomator dump` may take (it waits for the UI to go idle).
const UI_DUMP_SECS: u64 = 150;
/// Writes a touch gesture as an input stream (runs inside the session).
const GESTURE_SH: &str = include_str!("../android/gesture.sh");
/// The probes APK, copied in for its input harness (app_process loads it from
/// there; it isn't installed).
const PROBES_APK: &str = "/data/local/tmp/innkeeper-probes.apk";

pub struct Run<'a> {
    s: &'a Session,
    app: &'a App,
    dir: PathBuf,
    checks: Vec<Check>,
    launch_ms: Option<u64>,
    pss_mb: Option<u64>,
    /// The app's window, once it is launched.
    win: Option<Win>,
    last_shot: std::cell::RefCell<Option<PathBuf>>,
}

/// An app window: its Android display, SurfaceFlinger's id for screencap and
/// its size in pixels.
struct Win {
    display: i32,
    sf_id: String,
    w: i32,
    h: i32,
}

/// The harness's coordinates are for a 1280x800 display.
const REF_W: i32 = 1280;
const REF_H: i32 = 800;

/// `launched display=N package=P size=WxH ...` from the window manager.
fn parse_launched(text: &str) -> Option<(i32, i32, i32)> {
    let data = text.split("data=\"").nth(1)?;
    let field = |k: &str| {
        data.split_whitespace()
            .find_map(|f| f.strip_prefix(k))
            .map(|v| v.trim_end_matches('"').to_string())
    };
    let d = field("display=")?.parse().ok()?;
    let (w, h) = field("size=")?
        .split_once('x')
        .map(|(a, b)| (a.to_string(), b.to_string()))?;
    Some((d, w.parse().ok()?, h.parse().ok()?))
}

/// `<display id="N">` … `</display>` of a `uiautomator dump --windows`.
fn display_section(xml: &str, display: i32) -> Option<String> {
    let start = xml.find(&format!("<display id=\"{display}\""))?;
    let rest = &xml[start..];
    let end = rest
        .find("</display>")
        .map_or(rest.len(), |e| e + "</display>".len());
    Some(rest[..end].to_string())
}

/// A window's `size=WxH` in the window manager's LIST reply.
fn parse_list_size(text: &str, display: i32) -> Option<(i32, i32)> {
    let key = format!("display={display} ");
    let line = text.lines().find(|l| l.contains(&key))?;
    let size = line
        .split_whitespace()
        .find_map(|f| f.strip_prefix("size="))?;
    let (w, h) = size.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// Keep the launch window while it exists; follow an unambiguous replacement
/// after a startup trampoline finishes its task (e.g. Thunderbird onboarding).
fn app_display(text: &str, pkg: &str, original: i32) -> Option<i32> {
    let key = format!("package={pkg} ");
    let displays: Vec<i32> = text
        .lines()
        .filter_map(|line| {
            if !line.contains(&key) || !line.contains("window=yes") {
                return None;
            }
            line.split_once("display=")?
                .1
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .collect();
    if displays.contains(&original) {
        Some(original)
    } else if let [display] = displays.as_slice() {
        Some(*display)
    } else {
        None
    }
}

/// SurfaceFlinger's display id for virtual display `name`
/// (`dumpsys SurfaceFlinger --display-id`).
fn parse_sf_id(text: &str, name: &str) -> Option<String> {
    let key = format!("displayName=\"{name}\"");
    text.lines()
        .find(|l| l.contains(&key))?
        .strip_prefix("Display ")?
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// `mCurrentFocus` of `display` in `dumpsys window displays`.
fn parse_focus(text: &str, display: i32) -> Option<String> {
    let mut cur = -1;
    for l in text.lines() {
        let t = l.trim();
        if let Some(r) = t.strip_prefix("Display: mDisplayId=") {
            cur = r.split_whitespace().next()?.parse().unwrap_or(-1);
        } else if cur == display
            && let Some(f) = t.strip_prefix("mCurrentFocus=")
        {
            return Some(f.to_string());
        }
    }
    None
}

/// Why a step ended the run early.
struct Stop(Status, String);

impl<'a> Run<'a> {
    pub fn new(s: &'a Session, app: &'a App, out: &Path) -> Self {
        let dir = out.join(&app.package);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        Self {
            s,
            app,
            dir,
            checks: vec![],
            launch_ms: None,
            pss_mb: None,
            win: None,
            last_shot: Default::default(),
        }
    }

    /// `input` with `-d` for the app's display, coordinates scaled to it.
    fn input(&self, args: &[&str]) {
        let mut v: Vec<String> = vec!["input".into()];
        if let Some(w) = &self.win {
            v.push("-d".into());
            v.push(w.display.to_string());
        }
        let scale = |i: usize, a: &str| -> String {
            match (&self.win, a.parse::<i32>()) {
                (Some(w), Ok(n)) if matches!(args[0], "tap" | "swipe") && i > 0 && i <= 4 => {
                    let (full, reff) = if i % 2 == 1 {
                        (w.w, REF_W)
                    } else {
                        (w.h, REF_H)
                    };
                    (n * full / reff).to_string()
                }
                _ => a.to_string(),
            }
        };
        v.extend(args.iter().enumerate().map(|(i, a)| scale(i, a)));
        let argv: Vec<&str> = v.iter().map(String::as_str).collect();
        self.s.run(15, &argv);
    }

    /// Opens the app in a window of its own through the window manager and
    /// waits for its first frame ("Displayed" in the log).
    fn launch_in_window(&mut self, component: &str) -> Result<(), Stop> {
        let pkg = self.app.package.clone();
        let o = self.s.run(
            60,
            &[
                "am",
                "broadcast",
                "-n",
                "dev.sargunv.innkeeper.system/.wm.ControlReceiver",
                "-a",
                "dev.sargunv.innkeeper.LAUNCH",
                "--es",
                "package",
                &pkg,
                "--es",
                "component",
                component,
                "--ei",
                "width",
                &REF_W.to_string(),
                "--ei",
                "height",
                &REF_H.to_string(),
            ],
        );
        self.save("am-start.txt", &o.text);
        let Some((display, w, h)) = parse_launched(&o.text) else {
            let why = o
                .text
                .split("data=\"")
                .nth(1)
                .unwrap_or("no window manager reply")
                .trim()
                .trim_end_matches('"')
                .to_string();
            self.check("launch", false, why.clone());
            return Err(Stop(Status::Broken, format!("launch: {why}")));
        };
        let t = Instant::now();
        let mut displayed = None;
        while t.elapsed() < Duration::from_secs(60) {
            let log = self
                .s
                .run(
                    20,
                    &[
                        "logcat",
                        "-d",
                        "-b",
                        "system,main",
                        "-s",
                        "ActivityTaskManager:I",
                    ],
                )
                .text;
            // The app's own first frame, or a system activity it opened
            // straight away (a permission or battery-optimisation prompt).
            if let Some(l) = log
                .lines()
                .find(|l| l.contains("Displayed ") && l.contains(&pkg))
                .or_else(|| {
                    log.lines().find(|l| {
                        l.contains("Displayed com.android.settings/")
                            || l.contains("Displayed com.android.permissioncontroller/")
                    })
                })
            {
                displayed = Some(l.to_string());
                break;
            }
            sleep(Duration::from_millis(500));
        }
        let Some(line) = displayed else {
            self.check("launch", false, "no first frame in 60 s");
            return Err(Stop(Status::Broken, "launch: timeout".into()));
        };
        // "... Displayed pkg/.Act for user 0: +1s234ms"
        self.launch_ms = line.rsplit_once('+').and_then(|(_, d)| {
            let d = d.trim().trim_end_matches("ms");
            let (s, ms) = d.split_once('s').unwrap_or(("0", d));
            Some(s.parse::<u64>().ok()? * 1000 + ms.parse::<u64>().ok()?)
        });
        let sf = self
            .s
            .run(20, &["dumpsys", "SurfaceFlinger", "--display-id"])
            .text;
        let sf_id = parse_sf_id(&sf, &format!("innkeeper:{pkg}")).unwrap_or_default();
        self.check(
            "launch",
            true,
            format!(
                "{} ms, window display {display} ({w}x{h})",
                self.launch_ms.unwrap_or(0)
            ),
        );
        self.win = Some(Win {
            display,
            sf_id,
            w,
            h,
        });
        Ok(())
    }

    fn check(&mut self, name: &str, ok: bool, detail: impl Into<String>) -> bool {
        let detail = detail.into();
        eprintln!(
            "  {} {name}{}",
            if ok { "ok  " } else { "FAIL" },
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        );
        self.checks.push(Check {
            name: name.into(),
            ok,
            detail,
        });
        ok
    }

    fn save(&self, name: &str, text: &str) {
        let _ = fs::write(self.dir.join(name), text);
    }

    fn save_append(&self, name: &str, text: &str) {
        use std::io::Write;
        if let Ok(mut f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join(name))
        {
            let _ = f.write_all(text.as_bytes());
        }
    }

    pub fn run(mut self, apk: &Path) -> AppResult {
        let pkg = self.app.package.clone();
        let outcome = self.steps(apk);
        if outcome.is_ok() {
            self.signatures();
        }
        self.collect();
        self.s.run(120, &["pm", "uninstall", &pkg]);
        self.s.run(15, &["rm", "-f", TMP_APK, UI_XML]);
        let (result, reason) = match outcome {
            Err(Stop(st, why)) => (st, why),
            Ok(()) => {
                let failed: Vec<_> = self
                    .checks
                    .iter()
                    .filter(|c| !c.ok)
                    .map(|c| {
                        if c.detail.is_empty() {
                            c.name.clone()
                        } else {
                            format!("{} ({})", c.name, c.detail)
                        }
                    })
                    .collect();
                if failed.is_empty() {
                    (
                        Status::Works,
                        "launches, renders, survives tap/scroll/back".into(),
                    )
                } else {
                    (
                        Status::Partly,
                        format!("first frame ok; {}", failed.join("; ")),
                    )
                }
            }
        };
        eprintln!("  => {}: {reason}", result.label());
        let r = AppResult {
            package: pkg,
            name: self.app.name.clone(),
            group: self.app.group.clone(),
            version_name: self.app.version_name.clone(),
            version_code: self.app.version_code,
            result,
            reason,
            launch_ms: self.launch_ms,
            pss_mb: self.pss_mb,
            checks: self.checks,
        };
        if let Ok(j) = serde_json::to_string_pretty(&r) {
            let _ = fs::write(self.dir.join("result.json"), j + "\n");
        }
        r
    }

    fn steps(&mut self, apk: &Path) -> Result<(), Stop> {
        let pkg = self.app.package.clone();
        let s = self.s;
        s.run(120, &["pm", "uninstall", &pkg]);

        // Install.
        if let Err(e) = s.push_fixed(apk, TMP_APK) {
            return Err(Stop(Status::Error, format!("{e:#}")));
        }
        let o = s.run(
            300,
            &[
                "pm",
                "install",
                "-r",
                "-g",
                "--bypass-low-target-sdk-block",
                TMP_APK,
            ],
        );
        self.save("install.txt", &o.text);
        if !o.text.contains("Success") {
            let why = o
                .text
                .lines()
                .find(|l| l.contains("Failure") || l.contains("Exception"))
                .unwrap_or("pm install failed")
                .trim();
            // "Failure [INSTALL_FAILED_X: INSTALL_FAILED_X: detail]" -> "INSTALL_FAILED_X: detail"
            let why = why
                .trim_start_matches("Failure [")
                .trim_end_matches(']')
                .split(": ")
                .fold(Vec::<&str>::new(), |mut v, p| {
                    if v.last() != Some(&p) {
                        v.push(p);
                    }
                    v
                })
                .join(": ");
            self.check("install", false, why.clone());
            return Err(Stop(Status::Broken, format!("install: {why}")));
        }
        self.check("install", true, "");

        // Launcher activity; TV apps may only have a leanback one.
        let mut component = None;
        // Right after an install (or an Android restart) the package may not
        // resolve yet: retry a few times.
        let cats = [
            "android.intent.category.LAUNCHER",
            "android.intent.category.LEANBACK_LAUNCHER",
        ];
        for (attempt, cat) in (0..4).flat_map(|a| cats.iter().map(move |c| (a, *c))) {
            if component.is_some() {
                break;
            }
            if attempt > 0 && cat == cats[0] {
                sleep(Duration::from_secs(3));
            }
            let o = s.run(
                30,
                &[
                    "cmd",
                    "package",
                    "resolve-activity",
                    "--brief",
                    "-a",
                    "android.intent.action.MAIN",
                    "-c",
                    cat,
                    &pkg,
                ],
            );
            if let Some(c) = o
                .text
                .lines()
                .map(str::trim)
                .rfind(|l| l.starts_with(&format!("{pkg}/")))
            {
                if cat.contains("LEANBACK") {
                    self.check("leanback-only launcher", true, c);
                }
                component = Some(c.to_string());
                break;
            }
        }
        let Some(component) = component else {
            self.check("launcher activity", false, "");
            return Err(Stop(Status::Broken, "no launcher activity".into()));
        };

        // Launch from a clean slate.
        s.run(30, &["logcat", "-b", "main,system,crash", "-c"]);
        self.launch_in_window(&component)?;
        self.after_launch()
    }

    fn after_launch(&mut self) -> Result<(), Stop> {
        let pkg = self.app.package.clone();
        // Settle, watching for an early death.
        let settle = Duration::from_secs(self.app.settle.unwrap_or(8));
        let t = Instant::now();
        while t.elapsed() < settle {
            sleep(Duration::from_secs(2));
            if let Some(why) = self.dead() {
                self.check("alive after launch", false, why.clone());
                return Err(Stop(Status::Broken, why));
            }
        }
        self.check("alive after launch", true, "");

        self.refresh_window();
        // Platform compatibility warnings ("built for an older version of
        // Android") sit over a working app; note them and dismiss them. The
        // window manager shows the first in the app's window
        // (AppWarningActivity).
        let mut focus = self.focus();
        // The same goes for the system's "let app always run in the
        // background?" prompt that some apps raise at start.
        for (dialog, button) in [
            ("DeprecatedTargetSdkVersionDialog", "OK"),
            ("DeprecatedAbiDialog", "OK"),
            ("AppWarningActivity", "OK"),
            ("RequestIgnoreBatteryOptimizations", "Allow"),
        ] {
            if focus.contains(dialog) {
                let _ = self.screencap(&self.dir.join("warning.png"));
                self.check(
                    &format!("{dialog} shown"),
                    true,
                    format!("answered {button}"),
                );
                let _ = self.script(&[format!("tap:{button}")]);
                sleep(Duration::from_secs(1));
                focus = self.focus();
            }
        }

        // First frame.
        let focused = focus.contains(&pkg);
        self.check("app window focused", focused, focus.clone());
        let png = self.dir.join("first.png");
        match self.screencap(&png) {
            Ok(st) if st.blank() && self.secure_window() => {
                // FLAG_SECURE: SurfaceFlinger leaves the app out of
                // screenshots (the display itself still shows it). Judge the
                // first screen by its UI tree instead.
                let nodes = self.ui_nodes().unwrap_or_default();
                let n = nodes.len();
                let texts = nodes
                    .iter()
                    .filter(|n| !n.text.is_empty() || !n.desc.is_empty())
                    .count();
                self.check(
                    "first frame",
                    texts > 0,
                    format!("FLAG_SECURE window, screenshot blocked; UI tree: {n} nodes, {texts} labelled"),
                );
                if texts == 0 {
                    return Err(Stop(
                        Status::Broken,
                        "secure window with an empty UI tree".into(),
                    ));
                }
            }
            Ok(st) if st.blank() => {
                self.check("first frame", false, format!("blank: {}", st.describe()));
                return Err(Stop(
                    Status::Broken,
                    format!("blank frame ({})", st.describe()),
                ));
            }
            Ok(st) => {
                self.check("first frame", true, st.describe());
            }
            Err(e) => {
                self.check("first frame", false, format!("{e:#}"));
                return Err(Stop(Status::Error, format!("screencap: {e:#}")));
            }
        }
        if !focused {
            let why = if focus.contains("Not Responding") {
                "ANR dialog"
            } else if focus.contains("Application Error") {
                "crash dialog"
            } else {
                "app window not in front"
            };
            return Err(Stop(Status::Broken, format!("{why} ({focus})")));
        }

        self.refresh_window();
        // Per-app script, from a fresh first screen.
        let script = self.app.script.clone();
        if !script.is_empty() {
            let res = self.script(&script);
            self.check("script", res.is_ok(), res.err().unwrap_or_default());
            if let Some(why) = self.dead() {
                self.check("alive after script", false, why);
                return Ok(());
            }
        }

        // Generic interactions.
        for (name, cmd) in [
            ("tap centre", vec!["tap", "640", "420"]),
            ("scroll", vec!["swipe", "640", "600", "640", "250", "300"]),
            ("back", vec!["keyevent", "BACK"]),
        ] {
            if name == "back" {
                self.pss_mb = self.pss();
                let _ = self.screencap(&self.dir.join("after.png"));
            }
            self.input(&cmd);
            sleep(Duration::from_millis(2500));
            if let Some(why) = self.dead() {
                self.check(&format!("alive after {name}"), false, why);
                return Ok(());
            }
        }
        self.check("alive after tap/scroll/back", true, "");
        Ok(())
    }

    /// A crash, ANR or process death since launch, if any.
    fn dead(&self) -> Option<String> {
        let pkg = &self.app.package;
        let crash = self
            .s
            .run(20, &["logcat", "-d", "-b", "crash", "-t", "400"])
            .text;
        if crash.contains(pkg.as_str()) {
            let first = crash
                .lines()
                .find(|l| l.contains("Exception") || l.contains("Error") || l.contains("signal"))
                .map(|l| l.split_once(": ").map_or(l, |x| x.1).trim().to_string())
                .unwrap_or_default();
            return Some(format!("crash: {first}"));
        }
        let am = self
            .s
            .run(
                20,
                &[
                    "logcat",
                    "-d",
                    "-b",
                    "system,main",
                    "-t",
                    "4000",
                    "-s",
                    "ActivityManager",
                ],
            )
            .text;
        if let Some(l) = am
            .lines()
            .find(|l| l.contains("ANR in") && l.contains(pkg.as_str()))
        {
            return Some(format!(
                "ANR: {}",
                l.split_once("ANR in").map_or(l, |x| x.1).trim()
            ));
        }
        if !self.s.run(15, &["pidof", pkg]).ok {
            let why = am
                .lines()
                .rfind(|l| {
                    l.contains(pkg.as_str())
                        && (l.contains("has died") || l.contains("Killing") || l.contains("Force"))
                })
                .map(|l| l.split_once(": ").map_or(l, |x| x.1).trim().to_string())
                .unwrap_or_else(|| "no crash logged".into());
            return Some(format!("process gone: {why}"));
        }
        None
    }

    /// Whether the app's focused window has FLAG_SECURE.
    fn secure_window(&self) -> bool {
        let o = self.s.run(30, &["dumpsys", "window", "windows"]);
        let mut in_app = false;
        for l in o.text.lines() {
            let t = l.trim();
            if t.starts_with("Window #") {
                in_app = t.contains(&format!(" {}/", self.app.package));
            } else if in_app && t.starts_with("fl=") && t.split_whitespace().any(|f| f == "SECURE")
            {
                return true;
            }
        }
        false
    }

    fn focus(&self) -> String {
        let o = self.s.run(30, &["dumpsys", "window", "displays"]);
        let display = self.win.as_ref().map_or(0, |w| w.display);
        parse_focus(&o.text, display).unwrap_or_else(|| "?".into())
    }

    fn screencap(&self, dst: &Path) -> anyhow::Result<frame::FrameStats> {
        let tmp = "/data/local/tmp/compat-screen.png";
        let o = match &self.win {
            Some(w) if !w.sf_id.is_empty() => {
                self.s.run(30, &["screencap", "-d", &w.sf_id, "-p", tmp])
            }
            _ => self.s.run(30, &["screencap", "-p", tmp]),
        };
        if !o.ok {
            anyhow::bail!("screencap failed: {}", o.text.trim());
        }
        self.s.pull(tmp, dst)?;
        self.s.run(10, &["rm", "-f", tmp]);
        frame::analyze(dst)
    }

    fn pss(&self) -> Option<u64> {
        let o = self
            .s
            .run(30, &["dumpsys", "meminfo", "-s", &self.app.package]);
        let l = o
            .text
            .lines()
            .find(|l| l.trim_start().starts_with("TOTAL PSS:"))?;
        let kb: u64 = l.split_whitespace().nth(2)?.parse().ok()?;
        Some(kb / 1024)
    }

    fn script(&self, steps: &[String]) -> Result<(), String> {
        let log_from = self.s.bridge_log_len();
        for step in steps {
            let (op, arg) = step.split_once(':').ok_or(format!("bad step {step}"))?;
            match op {
                "audio" => {
                    let sink = std::env::var("INNKEEPER_AUDIO_SINK")
                        .map_err(|_| "audio: no INNKEEPER_AUDIO_SINK".to_string())?;
                    let path = self.dir.join("audio.raw");
                    let file = fs::File::create(&path).map_err(|e| e.to_string())?;
                    // Bound the recorder and its artifact to three seconds of PCM.
                    let status = Command::new("timeout")
                        .args([
                            "--kill-after=1",
                            "3",
                            "parec",
                            "-d",
                            &format!("{sink}.monitor"),
                        ])
                        .args(["--format=s16le", "--rate=48000", "--channels=2", "--raw"])
                        .stdout(Stdio::from(file))
                        .status()
                        .map_err(|e| format!("audio capture: {e}"))?;
                    if status.code() != Some(124) {
                        return Err(format!("audio capture: {status}"));
                    }
                    let analysis = Command::new("/usr/bin/python3")
                        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../e2e/audio/freq.py"))
                        .arg(&path)
                        .output()
                        .map_err(|e| format!("audio analysis: {e}"))?;
                    let detail = String::from_utf8_lossy(&analysis.stdout);
                    self.save_append("audio.txt", &detail);
                    if !analysis.status.success() {
                        return Err(format!("audio: silent output: {detail}"));
                    }
                }
                "wait" => sleep(Duration::from_secs_f64(arg.parse().unwrap_or(1.0))),
                // Text the host bridge logged since the script started, e.g. a
                // link a test session only logs instead of opening
                // (host/bridge/src/system/openuri.rs).
                "bridgelog" => {
                    let log = self.s.bridge_log(log_from);
                    if !log.contains(arg) {
                        return Err(format!("{step}: not in the bridge log"));
                    }
                }
                // A shell command in the session; fails on a non-zero exit.
                "sh" => {
                    let o = self.s.sh(60, arg);
                    self.save_append("script-sh.txt", &format!("$ {arg}\n{}\n", o.text));
                    if !o.ok {
                        return Err(format!("{step}: failed: {}", o.text.trim()));
                    }
                }
                "intent" => {
                    self.s.run(
                        30,
                        &[
                            "am",
                            "start",
                            "-a",
                            "android.intent.action.VIEW",
                            "-d",
                            arg,
                            &self.app.package,
                        ],
                    );
                    sleep(Duration::from_secs(1));
                }
                // pinch|pan|doubletap X Y, as touch through Innkeeper System's
                // translator and injector (the probes' input harness;
                // multi-touch, which `input` can't do).
                "gesture" => {
                    self.s
                        .push_fixed(&self.s.probes, PROBES_APK)
                        .map_err(|e| format!("{step}: {e:#}"))?;
                    // Scaled to the app window's pixels, on its display.
                    let mut a: Vec<String> = arg.split_whitespace().map(str::to_string).collect();
                    if let (Some(w), 3) = (&self.win, a.len()) {
                        let x: i32 = a[1].parse().unwrap_or(0);
                        let y: i32 = a[2].parse().unwrap_or(0);
                        a[1] = (x * w.w / REF_W).to_string();
                        a[2] = (y * w.h / REF_H).to_string();
                        a.extend([w.display.to_string(), w.w.to_string(), w.h.to_string()]);
                    }
                    let mut argv = vec![
                        "sh",
                        "-c",
                        "apk=$1 script=$2; shift 2; sh -c \"$script\" gesture.sh \"$@\" | \
                         CLASSPATH=$apk app_process / dev.sargunv.innkeeper.probes.InputStdinHarness",
                        "sh",
                        PROBES_APK,
                        GESTURE_SH,
                    ];
                    argv.extend(a.iter().map(String::as_str));
                    let o = self.s.run(60, &argv);
                    if !o.text.contains("inject=true") {
                        return Err(format!("{step}: not injected: {}", o.text.trim()));
                    }
                    sleep(Duration::from_millis(1500));
                }
                // Screenshot that must not be blank; "changed" must also
                // differ from the previous script screenshot.
                "shot" | "changed" => {
                    let png = self.dir.join(format!("script-{arg}.png"));
                    let st = self.screencap(&png).map_err(|e| format!("{step}: {e:#}"))?;
                    if st.blank() {
                        return Err(format!("{step}: blank ({})", st.describe()));
                    }
                    let mut last = self.last_shot.borrow_mut();
                    if op == "changed"
                        && let Some(prev) = last.as_ref()
                    {
                        let d = frame::diff(prev, &png).map_err(|e| format!("{step}: {e:#}"))?;
                        if d < 0.01 {
                            return Err(format!(
                                "{step}: frame unchanged ({:.2}% differs)",
                                d * 100.0
                            ));
                        }
                    }
                    *last = Some(png);
                }
                "key" => {
                    self.input(&["keyevent", arg]);
                    sleep(Duration::from_millis(700));
                }
                // "try" taps like "tap" but skips a missing node (optional
                // onboarding pages, tours, rating prompts).
                "tap" | "try" => {
                    let nodes = self.ui_nodes()?;
                    let Some(n) = nodes.iter().find(|n| n.text == arg || n.desc == arg) else {
                        if op == "try" {
                            continue;
                        }
                        return Err(format!("{step}: no such node"));
                    };
                    // Node bounds are already in the app display's pixels.
                    let (x, y) = n.centre;
                    let mut v = vec!["input".to_string()];
                    if let Some(w) = &self.win {
                        v.extend(["-d".to_string(), w.display.to_string()]);
                    }
                    v.extend(["tap".to_string(), x.to_string(), y.to_string()]);
                    let argv: Vec<&str> = v.iter().map(String::as_str).collect();
                    self.s.run(15, &argv);
                    sleep(Duration::from_millis(700));
                }
                "absent" => {
                    let nodes = self.ui_nodes()?;
                    if nodes
                        .iter()
                        .any(|n| n.text.contains(arg) || n.desc.contains(arg))
                    {
                        return Err(format!("{step}: on screen"));
                    }
                }
                "expect" => {
                    let nodes = self.ui_nodes()?;
                    if !nodes
                        .iter()
                        .any(|n| n.text.contains(arg) || n.desc.contains(arg))
                    {
                        return Err(format!("{step}: not on screen"));
                    }
                }
                _ => return Err(format!("unknown step {step}")),
            }
        }
        Ok(())
    }

    fn ui_nodes(&self) -> Result<Vec<Node>, String> {
        // The dump waits for the UI to go idle, which busy apps rarely do on
        // the first try. A window's display needs `--windows`: the plain dump
        // clips every node to display 0's size.
        let flag = if self.win.is_some() { "--windows " } else { "" };
        let mut last = String::new();
        // Under CPU load (other sessions running) a dump can take well over a
        // minute; give it 150 s, and four tries with a pause between them.
        for attempt in 0..4 {
            if attempt > 0 {
                sleep(Duration::from_secs(3));
            }
            let t = Instant::now();
            let o = self.s.sh(
                UI_DUMP_SECS,
                &format!(
                    "rm -f {UI_XML}; uiautomator dump {flag}{UI_XML} >/dev/null && cat {UI_XML}"
                ),
            );
            let took = t.elapsed().as_secs_f64();
            self.save_append(
                "ui-dump-times.txt",
                &format!("attempt {attempt}: {took:.1} s, ok={}\n", o.ok),
            );
            if o.text.contains("<hierarchy") || o.text.contains("<displays") {
                let xml = match &self.win {
                    Some(w) => display_section(&o.text, w.display).unwrap_or_default(),
                    None => o.text,
                };
                self.save("ui.xml", &xml);
                return Ok(parse_nodes(&xml));
            }
            last = format!("{} (after {took:.0} s)", o.text.trim());
        }
        Err(format!("uiautomator dump failed: {last}"))
    }

    /// Follow a replacement startup task, then re-read the window size after
    /// the compositor reports its real scale.
    fn refresh_window(&mut self) {
        let Some(display) = self.win.as_ref().map(|w| w.display) else {
            return;
        };
        let o = self.s.run(
            30,
            &[
                "am",
                "broadcast",
                "-n",
                "dev.sargunv.innkeeper.system/.wm.ControlReceiver",
                "-a",
                "dev.sargunv.innkeeper.LIST",
            ],
        );
        let Some(current) = app_display(&o.text, &self.app.package, display) else {
            return;
        };
        if current != display {
            let sf = self
                .s
                .run(20, &["dumpsys", "SurfaceFlinger", "--display-id"]);
            if let Some(win) = self.win.as_mut() {
                win.display = current;
                win.sf_id = parse_sf_id(&sf.text, &format!("innkeeper:{}", self.app.package))
                    .unwrap_or_default();
            }
        }
        if let Some((w, h)) = parse_list_size(&o.text, current)
            && let Some(win) = self.win.as_mut()
        {
            win.w = w;
            win.h = h;
        }
    }

    /// Known failure classes in the log since launch, each a failed check.
    fn signatures(&mut self) {
        const SIGS: &[(&str, &[&str], &str)] = &[
            (
                "media codecs",
                &["Codec reported err"],
                "MediaCodec failed to start (no sound/media)",
            ),
            (
                "WebView",
                &[
                    "MissingWebViewPackageException",
                    "Failed to load WebView provider",
                ],
                "WebView provider missing",
            ),
            (
                "GL/Vulkan errors",
                &[
                    "GL_INVALID_",
                    "GL_OUT_OF_MEMORY",
                    "EGL_BAD_",
                    "[OpenGL]",
                    "eglCreateContext failed",
                    "VK_ERROR_",
                ],
                "GL/EGL/Vulkan errors logged",
            ),
            (
                "Google Play services",
                &[
                    "GooglePlayServicesUtil",
                    "Google Play services is missing",
                    "Google Play services not available",
                ],
                "needs Google Play services",
            ),
        ];
        let pattern = SIGS
            .iter()
            .flat_map(|(_, pats, _)| pats.iter().copied())
            .collect::<Vec<_>>()
            .join("' -e '");
        let log = self
            .s
            .sh(
                60,
                &format!("logcat -d -b main,system -t 20000 | grep -F -e '{pattern}' | head -100"),
            )
            .text;
        for (name, pats, why) in SIGS {
            if pats.iter().any(|p| log.contains(p)) {
                self.check(name, false, *why);
            }
        }
    }

    /// Bounded logs and state for triage.
    fn collect(&self) {
        let s = self.s;
        let logs = [
            (
                "logcat.txt",
                vec!["logcat", "-d", "-t", "3000", "-b", "main,system,crash"],
            ),
            (
                "crash.txt",
                vec!["logcat", "-d", "-t", "500", "-b", "crash"],
            ),
        ];
        for (f, cmd) in logs {
            self.save(f, &s.run(30, &cmd).text);
        }
        self.save("focus.txt", &self.focus());
    }
}

pub struct Node {
    pub text: String,
    pub desc: String,
    pub centre: (i32, i32),
}

fn attr<'a>(node: &'a str, name: &str) -> &'a str {
    let key = format!(" {name}=\"");
    node.find(&key)
        .map(|i| &node[i + key.len()..])
        .and_then(|r| r.find('"').map(|j| &r[..j]))
        .unwrap_or("")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Text, content-desc and bounds of each node in a `uiautomator dump`.
pub fn parse_nodes(xml: &str) -> Vec<Node> {
    xml.split("<node ")
        .skip(1)
        .filter_map(|n| {
            let n = &format!(" {n}");
            let b = attr(n, "bounds");
            let v: Vec<i32> = b
                .split(|c: char| !c.is_ascii_digit() && c != '-')
                .filter(|x| !x.is_empty())
                .filter_map(|x| x.parse().ok())
                .collect();
            if v.len() != 4 {
                return None;
            }
            Some(Node {
                text: unescape(attr(n, "text")),
                desc: unescape(attr(n, "content-desc")),
                centre: ((v[0] + v[2]) / 2, (v[1] + v[3]) / 2),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_window_manager_output() {
        assert_eq!(
            parse_launched(
                "Broadcast completed: result=-1, data=\"launched display=4 package=a.b size=2176x1360 dpi=272\""
            ),
            Some((4, 2176, 1360))
        );
        assert_eq!(parse_launched("Broadcast completed: result=0"), None);
        let sf = "Display 0 (HWC display 0): no display identification data\nDisplay 9223372039441632759 (Virtual display): displayName=\"innkeeper:a.b\" uniqueId=\"virtual:x\"";
        assert_eq!(
            parse_sf_id(sf, "innkeeper:a.b").as_deref(),
            Some("9223372039441632759")
        );
        let w = "  Display: mDisplayId=0 (organized)\n  mCurrentFocus=null\n  Display: mDisplayId=4 (organized)\n  mCurrentFocus=Window{1 a.b/a.b.Main}";
        assert_eq!(parse_focus(w, 4).as_deref(), Some("Window{1 a.b/a.b.Main}"));
        assert_eq!(parse_focus(w, 0).as_deref(), Some("null"));
    }

    #[test]
    fn follows_the_only_replacement_task_window() {
        let replacement = "display=5 package=mail.app size=1280x800 tasks=12 window=yes\n";
        assert_eq!(app_display(replacement, "mail.app", 4), Some(5));
        assert_eq!(app_display(replacement, "other.app", 4), None);
        let both =
            format!("display=4 package=mail.app size=1280x800 tasks=11 window=yes\n{replacement}");
        assert_eq!(app_display(&both, "mail.app", 4), Some(4));
        assert_eq!(app_display(&both, "mail.app", 3), None);
        assert_eq!(
            app_display(
                "display=5 package=mail.app size=1280x800 window=no",
                "mail.app",
                4
            ),
            None
        );
    }

    #[test]
    fn picks_the_window_display() {
        let xml = r#"<displays><display id="0"><node text="Home" bounds="[0,0][1,1]"/></display><display id="4"><node text="Skip" bounds="[10,1200][110,1300]"/></display></displays>"#;
        let n = parse_nodes(&display_section(xml, 4).unwrap());
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].text, "Skip");
        assert_eq!(
            parse_list_size(
                "Broadcast completed: result=-1, data=\"display=3 package=a size=1280x800 dpi=160\ndisplay=4 package=b size=2176x1360 dpi=272 tasks=9",
                4
            ),
            Some((2176, 1360))
        );
    }

    #[test]
    fn parses_nodes() {
        let xml = r#"<hierarchy><node index="0" text="7" resource-id="" content-desc="" bounds="[487,381][633,477]"><node text="" content-desc="a &amp; b" bounds="[0,0][10,20]"/></node></hierarchy>"#;
        let n = parse_nodes(xml);
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].text, "7");
        assert_eq!(n[0].centre, (560, 429));
        assert_eq!(n[1].desc, "a & b");
    }
}
