//! Innkeeper compat harness. See tests/compat/README.md.
//!
//!   mise run test:compat -- [run] [--window] [--only PKG,...] [--accept]
//!   mise run test:compat -- fetch
//!   mise run test:compat -- report
//!
//! `run` fetches the corpus, makes sure a session is up (starting a headless
//! one if needed, and stopping it at the end), runs every app, writes
//! out/test/compat/<profile>/. `--accept` also records the results in
//! tests/compat/results.json. `report` renders the baseline under out/.

mod app;
mod budget;
mod corpus;
mod frame;
mod report;
mod session;

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::Duration;
use std::{env, fs};

use anyhow::{Context, Result, bail};

use app::{AppResult, Run, Status};
use budget::Budgets;
use report::Results;
use session::Session;

struct Opts {
    cmd: String,
    window: bool,
    only: Vec<String>,
    accept: bool,
}

fn parse_args() -> Result<Opts> {
    let mut o = Opts {
        cmd: "run".into(),
        window: false,
        only: vec![],
        accept: false,
    };
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "run" | "fetch" | "report" => o.cmd = a,
            "--window" => o.window = true,
            "--accept" => o.accept = true,
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
    let baseline_path = repo.join("tests/compat/results.json");
    let baseline = Results::load(&baseline_path).unwrap_or_default();
    let budgets = Budgets::load(&repo.join("tests/compat/budgets.toml"))?;

    if o.cmd == "report" {
        report::write_markdown(
            &repo.join("out/test/compat/COMPATIBILITY.md"),
            &baseline,
            &corpus,
            &budgets,
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    let apps: Vec<_> = corpus
        .app
        .iter()
        .filter(|a| o.only.is_empty() || o.only.contains(&a.package))
        .collect();
    if apps.is_empty() {
        bail!("no apps selected");
    }
    let out = repo
        .join("out/test/compat")
        .join(innkeeper_core::paths::profile()?);
    let apk_dir = env::var_os("INNKEEPER_CORPUS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| repo.join("out/test/compat/corpus"));
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
    let mut measured = s.start_measured(o.window)?;
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

    // A median over part of the corpus isn't comparable.
    if o.only.is_empty() {
        measured.app_start_ms = budget::median(
            results
                .iter()
                .filter(|r| r.result == Status::Works)
                .filter_map(|r| r.launch_ms)
                .collect(),
        );
    }
    let mut new = Results {
        date: today(),
        build: s.getprop("ro.build.fingerprint"),
        headless: s.headless(),
        session_mem_mb: s.memory_current().map(|b| b >> 20),
        measured,
        apps: results.clone(),
    };
    // Publish actual observations before the baseline merge masks harness errors.
    new.save(&out.join("fresh-results.json"))?;
    new.measured = measured.or(baseline.measured);
    new.apps.clear();
    // Merge: keep baseline entries for apps not run, or that hit a harness error.
    for a in &corpus.app {
        let fresh = results.iter().find(|r| r.package == a.package);
        let old = baseline.apps.iter().find(|r| r.package == a.package);
        match (fresh, old) {
            (Some(r), Some(old)) if r.result == Status::Error => new.apps.push(old.clone()),
            (Some(r), _) => new.apps.push(r.clone()),
            (None, Some(old)) => new.apps.push(old.clone()),
            (None, None) => {}
        }
    }
    new.save(&out.join("results.json"))?;
    report::write_markdown(&out.join("COMPATIBILITY.md"), &new, &corpus, &budgets)?;

    let mut regressions = report::regressions(&baseline, &results);
    for l in budgets.lines(&measured) {
        let Some(m) = l.measured else {
            eprintln!("compat: {}: not measured", l.name);
            continue;
        };
        let what = format!(
            "{}: {m} {} (budget {} {}, limit {})",
            l.name, l.unit, l.budget, l.unit, l.limit
        );
        if l.over() {
            regressions.push(format!("over budget: {what}"));
        } else {
            eprintln!("compat: {what}");
        }
    }
    for r in &regressions {
        eprintln!("compat: REGRESSION {r}");
    }
    println!("{}", report::summary(&results));
    if o.accept {
        new.save(&baseline_path)?;
        eprintln!("compat: wrote tests/compat/results.json");
    } else {
        eprintln!(
            "compat: baseline kept; results in {} (--accept to record)",
            out.display()
        );
    }
    Ok(if regressions.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn today() -> String {
    Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}
