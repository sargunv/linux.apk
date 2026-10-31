//! The session (docs/session.md, "Flatpak mode") runs in Innkeeper's own
//! sandbox, which allows no namespaces, mounts or systemd units, but whose
//! `/`, `/dev` and `/sys` are writable tmpfs dirs of its own.
//!
//! - The root is composed on disk: per partition, hardlinks to the (read-only)
//!   image cache plus copies of the layers above it, files read-only. It is
//!   composed once per image, stage and addons, and only the start-time layer
//!   goes on at each start.
//! - `/` gets symlinks to it, the binder nodes and socket dirs; the broker
//!   shows the Android paths behind the links (supervisor/broker/src/fs.rs).
//! - init is the broker's child (execed from pivot-exec), and the session
//!   lives as long as the process that started it, which serves the control
//!   socket ([`crate::control`]).

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::thread::sleep;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::conf::Config;
use crate::image::walk_files;
use crate::paths::Paths;
use crate::root::{self, LAYERED, PARTITIONS};
use crate::session::{self, append};
use crate::{control, init, lifecycle, sys};

/// Bumped when the composition below changes.
const COMPOSE_VERSION: &str = "1";

fn cp(args: &[&std::ffi::OsStr]) -> Result<()> {
    let st = Command::new("cp").args(args).status().context("run cp")?;
    if !st.success() {
        bail!("cp {args:?} failed");
    }
    Ok(())
}

/// A layer's files over the composed tree. `--remove-destination` unlinks a
/// hardlinked file before replacing it, so the image cache is never written.
fn over(layer: &Path, dst: &Path) -> Result<()> {
    if !layer.is_dir() {
        return Ok(());
    }
    let mut src = layer.as_os_str().to_owned();
    src.push("/.");
    let mut to = dst.as_os_str().to_owned();
    to.push("/");
    cp(&[
        "-a".as_ref(),
        "--reflink=auto".as_ref(),
        "--remove-destination".as_ref(),
        &src,
        &to,
    ])
}

/// Read-only files, as on read-only mounts: ART refuses to load a dex file
/// its process could write.
fn files_read_only(dir: &Path) -> Result<()> {
    walk_files(dir, &mut |p, m| {
        if m & 0o222 != 0 {
            fs::set_permissions(p, fs::Permissions::from_mode(m & !0o222 & 0o7777))?;
        }
        Ok(())
    })
}

/// Deletes a tree whose dirs may be read-only.
pub fn remove_tree(dir: &Path) -> Result<()> {
    if dir.symlink_metadata().is_err() {
        return Ok(());
    }
    fn writable(d: &Path) {
        if let Ok(m) = d.symlink_metadata()
            && m.is_dir()
        {
            let _ = fs::set_permissions(d, fs::Permissions::from_mode((m.mode() & 0o7777) | 0o700));
            for e in fs::read_dir(d).into_iter().flatten().flatten() {
                writable(&e.path());
            }
        }
    }
    writable(dir);
    fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))
}

/// What the composed root is made of: the image, its derived layers, the
/// stage (its manifest has every file's hash) and the addons' files.
fn compose_key(cfg: &Config) -> Result<String> {
    let mut h = Sha256::new();
    h.update(COMPOSE_VERSION);
    h.update(cfg.image.img.as_os_str().as_encoded_bytes());
    h.update(cfg.image.apexdir.as_os_str().as_encoded_bytes());
    for l in ["common", "single"] {
        h.update(fs::read(cfg.image.derived.join(format!("{l}.source"))).unwrap_or_default());
    }
    h.update(fs::read(cfg.paths.share.join("manifest.tsv"))?);
    h.update(format!("{:?}", crate::phone::mode(&cfg.paths.data)?));
    for a in cfg.paths.addon_layers() {
        h.update(a.as_os_str().as_encoded_bytes());
        let mut files = vec![];
        walk_files(&a, &mut |p, m| {
            let md = fs::metadata(p)?;
            files.push(format!(
                "{} {m:o} {} {}",
                p.display(),
                md.len(),
                md.mtime_nsec() + md.mtime() * 1_000_000_000
            ));
            Ok(())
        })?;
        files.sort();
        h.update(files.join("\n"));
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Composes the partitions and `/apex` under the root dir, unless the last
/// composition had the same inputs.
fn compose(cfg: &Config) -> Result<()> {
    let r = &cfg.paths.root;
    let key = compose_key(cfg)?;
    let key_file = r.join(".compose-key");
    if fs::read_to_string(&key_file).is_ok_and(|k| k == key) {
        return Ok(());
    }
    remove_tree(r)?;
    fs::create_dir_all(r)?;
    // Hardlinks to the image cache, or reflink copies where they can't cross
    // mounts (the dev driver grants the image cache and the root separately).
    let probe = r.join(".link");
    let tree: &[&std::ffi::OsStr] =
        if fs::hard_link(cfg.image.apexdir.join("apex-info-list.xml"), &probe).is_ok() {
            &["-al".as_ref()]
        } else {
            &["-a".as_ref(), "--reflink=auto".as_ref()]
        };
    let _ = fs::remove_file(&probe);
    let link = |src: &Path, dst: &Path| cp(&[tree, &[src.as_os_str(), dst.as_os_str()]].concat());
    let p = &cfg.paths;
    let addons = p.addon_layers();
    // Each partition's stack, bottom first; the start-time layer goes on at each start.
    for part in PARTITIONS {
        let src = root::image_partition(&cfg.image.img, part);
        if !src.is_dir() {
            continue;
        }
        let dst = r.join(part);
        link(&src, &dst)?;
        if LAYERED.contains(part) {
            let mut layers = vec![
                cfg.image.derived.join("common").join(part),
                cfg.image.derived.join("single").join(part),
                p.android().join(part),
            ];
            layers.extend(addons.iter().map(|a| a.join(part)));
            for l in layers {
                over(&l, &dst)?;
            }
        }
    }
    if crate::phone::mode(&cfg.paths.data)? == crate::profile::Mode::Phone {
        for keep in crate::phone::KEEPS {
            let (part, rest) = keep.split_once('/').unwrap_or((keep, ""));
            let src = root::image_partition(&cfg.image.img, part).join(rest);
            if src.exists() {
                remove_tree(&r.join(keep))?;
                link(&src, &r.join(keep))?;
            }
        }
    }
    let apex = r.join("apex");
    link(&cfg.image.apexdir, &apex)?;
    over(&cfg.image.derived.join("single").join("apex"), &apex)?;
    files_read_only(r)?;
    // The uid mode's layer sets exact modes that code verifies (clatd's
    // 06755), so it goes on again.
    over(&cfg.image.derived.join("single").join("apex"), &apex)?;
    fs::write(&key_file, key)?;
    Ok(())
}

/// The start-time layer over the composed root, read-only too.
fn apply_run_layer(cfg: &Config) -> Result<()> {
    let r = &cfg.paths.root;
    let layer = cfg.run_layer();
    for part in LAYERED.iter().chain(&["apex"]) {
        over(&layer.join(part), &r.join(part))?;
    }
    walk_files(&layer, &mut |p, _| {
        let dst = r.join(p.strip_prefix(&layer)?);
        let m = fs::metadata(&dst)?.permissions().mode();
        fs::set_permissions(&dst, fs::Permissions::from_mode(m & !0o222 & 0o7777))?;
        Ok(())
    })?;
    let l = r.join("innkeeper");
    remove_tree(&l)?;
    fs::create_dir_all(&l)?;
    root::innkeeper_dir(cfg, &l)?;
    symlink("/apex", l.join("apex"))?;
    Ok(())
}

/// A symlink `link` -> `target`, replacing whatever is at `link`.
fn link(target: &Path, link: &Path) -> Result<()> {
    if let Ok(m) = link.symlink_metadata() {
        if m.is_dir() {
            fs::remove_dir(link).with_context(|| format!("replace {}", link.display()))?;
        } else {
            fs::remove_file(link)?;
        }
    }
    symlink(target, link).with_context(|| format!("link {}", link.display()))
}

/// The sandbox's `/` as Android's root.
fn sandbox_root(cfg: &Config) -> Result<()> {
    let r = &cfg.paths.root;
    let slash = Path::new("/");
    for p in PARTITIONS.iter().chain(&["apex", "innkeeper"]) {
        if r.join(p).exists() {
            link(&r.join(p), &slash.join(p))?;
        }
    }
    let data = &cfg.paths.data;
    fs::create_dir_all(data)?;
    link(data, Path::new("/data"))?;
    // vold binds /data/data onto /data/user/0, where apps' data dirs are (the
    // broker's lstat shows the symlink as that directory).
    fs::create_dir_all(data.join("user"))?;
    fs::create_dir_all(data.join("data"))?;
    if data.join("user/0").symlink_metadata().is_err() {
        symlink("/data/data", data.join("user/0"))?;
    }
    // The image's other top-level entries, where the sandbox has none (its
    // /etc and /bin stay).
    root::copy_top_level(&cfg.image.img, slash, false)?;
    // Shared storage without FUSE (vold can't mount MediaProvider's, nor bind
    // /mnt/user/0 over /storage): apps reach /data/media directly, so it is
    // on disk rather than in the sandbox's tmpfs.
    link(Path::new("/data/media"), Path::new("/storage/emulated"))?;
    for base in ["/mnt/user/0", "/mnt/pass_through/0"] {
        fs::create_dir_all(base)?;
        link(Path::new("/data/media"), &Path::new(base).join("emulated"))?;
    }
    fs::create_dir_all("/storage/self")?;
    link(
        Path::new("/storage/emulated/0"),
        Path::new("/storage/self/primary"),
    )?;
    // Android's /etc is /system/etc. The sandbox's /etc stays (the glibc side
    // needs it) and gains the entries it lacks, such as task_profiles.json:
    // dirs as symlinks, files as copies (libbase opens files O_NOFOLLOW).
    for e in fs::read_dir(r.join("system/etc"))? {
        let e = e?;
        let to = Path::new("/etc").join(e.file_name());
        if to.symlink_metadata().is_ok() {
            continue;
        }
        let t = e.file_type()?;
        if t.is_dir() {
            symlink(Path::new("/system/etc").join(e.file_name()), &to)?;
        } else if t.is_symlink() {
            symlink(fs::read_link(e.path())?, &to)?;
        } else {
            root::copy_file(&e.path(), &to)?;
        }
    }
    let dev = Path::new("/dev");
    root::binder_nodes(dev)?;
    let run = &cfg.paths.run;
    fs::set_permissions(run.join("bridge"), fs::Permissions::from_mode(0o700))?;
    fs::create_dir_all(dev.join("socket"))?;
    link(&run.join("bridge"), &dev.join("socket/innkeeper"))?;
    link(&run.join("kmsg"), &dev.join("kmsg"))?;
    // The bpf() emulator's pins (supervisor/broker/src/bpf.rs).
    fs::create_dir_all("/sys/fs/bpf")?;
    fs::set_permissions("/sys/fs/bpf", fs::Permissions::from_mode(0o1777))?;
    // No cgroup tree in the sandbox: /sys/fs/cgroup is a plain directory, so
    // process groups are bookkeeping only and kills fall back to the process
    // group. netd attaches its (emulated) eBPF programs there.
    fs::create_dir_all("/sys/fs/cgroup/system")?;
    fs::create_dir_all("/sys/fs/cgroup/apps")?;
    Ok(())
}

/// Ends the session: kills everything it started and removes its socket and
/// pid files. When this process is the sandbox's main one (its parent is the
/// sandbox's pid 1), that is everything else in the sandbox: Android
/// processes orphaned by a dying init would otherwise keep it alive.
fn teardown(p: &Paths) {
    session::ENDING.store(true, std::sync::atomic::Ordering::SeqCst);
    sys::kill_descendants();
    // The bridge is killed with Android, so its signal handler cannot reap
    // files kept alive for desktop readers. End their lifetime here.
    let cache = crate::paths::xdg_home("XDG_CACHE_HOME", ".cache")
        .join(crate::paths::per_profile("innkeeper", &p.profile))
        .join("handoff");
    let _ = fs::remove_dir_all(cache);
    if sys::ppid() == 1 {
        unsafe { libc::kill(-1, libc::SIGKILL) };
    }
    let _ = fs::remove_file(p.control());
    session::remove_pids(p);
}

/// How a session that ended by itself went idle: `Some(wakeup)` once idle.
type Idle = Arc<Mutex<Option<Option<u64>>>>;

/// Runs the session in this sandbox until init exits; with `follow`, shows
/// its log meanwhile. It ends with this process: on a signal, `stop`, or
/// init's exit. When it stops for idleness with an alarm ahead, this process
/// stays as the waiter that starts it again ([`wait_to_wake`]).
pub fn run(
    cfg: &Config,
    follow: bool,
    image_lease: crate::prune::Lease,
    trace: &session::BootTrace,
) -> Result<()> {
    let sigs = sys::block_signals(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]);
    let p = cfg.paths.clone();
    thread::spawn(move || {
        let sig = sys::wait_signal(&sigs);
        teardown(&p);
        std::process::exit(128 + sig);
    });
    let idle = Idle::default();
    let r = run_session(cfg, follow, &idle, trace);
    teardown(&cfg.paths);
    // The alarm waiter has no Android processes left. Its next start must
    // be able to prepare changed derived layers and acquire a fresh lease.
    drop(image_lease);
    let wakeup = *idle.lock().unwrap();
    match wakeup {
        Some(Some(at)) if r.is_ok() => wait_to_wake(cfg, at),
        _ => r,
    }
}

/// Stays in the sandbox, stopped, until shortly before `at_ms` (ms since the
/// epoch), then starts the session again as `start` does, in a new sandbox:
/// this one's `/` holds the last boot's runtime files. `status` reports it
/// stopped, and `stop` (or a new `start`) ends it.
fn wait_to_wake(cfg: &Config, at_ms: u64) -> Result<()> {
    let p = &cfg.paths;
    request_background(p);
    let at = at_ms.saturating_sub(lifecycle::wake_lead().as_millis() as u64);
    let sock = p.control();
    let gone = sock.clone();
    control::serve(
        &sock,
        control::Served {
            status: format!("stopped\nwakeup={at_ms}\n"),
            exec_lock: None,
            stop: Box::new(move || {
                let _ = fs::remove_file(&gone);
                std::process::exit(0);
            }),
        },
    )?;
    session::log(
        p,
        &format!(
            "waiting to start again at {}",
            lifecycle::calendar(at / 1000)
        ),
    );
    // Wall-clock time, checked often: a suspended host wakes late, not never.
    loop {
        let now = lifecycle::now_ms();
        if now >= at {
            break;
        }
        sleep(Duration::from_millis((at - now).min(30_000)));
    }
    // Not waiting any more, so `start` doesn't stop this process first.
    let _ = fs::remove_file(&sock);
    let mut c = Command::new(env::current_exe()?);
    c.arg("start");
    if cfg.headless {
        c.arg("--headless");
    }
    let st = c.stdin(Stdio::null()).status().context("run start")?;
    if !st.success() {
        bail!("start the session again: {st}");
    }
    Ok(())
}

/// Asks the Background portal to let Innkeeper run with no window open
/// (media, alarms, background work), so the desktop doesn't stop it. Best
/// effort, without waiting for the answer: `gdbus` is in the Flatpak runtime.
pub fn request_background(p: &Paths) {
    if crate::preferences::Preferences::load().is_ok_and(|p| p.keep_running) {
        return;
    }
    let opts = "{'reason': <'Android apps keep playing media, run foreground services and ring \
                alarms with no window open'>, 'autostart': <false>}";
    let r = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest=org.freedesktop.portal.Desktop",
            "--object-path=/org/freedesktop/portal/desktop",
            "--method=org.freedesktop.portal.Background.RequestBackground",
            "",
            opts,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Err(e) = r {
        session::log(p, &format!("Background portal: {e}"));
    }
}

fn run_session(cfg: &Config, follow: bool, idle: &Idle, trace: &session::BootTrace) -> Result<()> {
    let p = &cfg.paths;
    compose(cfg).context("compose the root")?;
    trace.mark(p, "root composed");
    apply_run_layer(cfg).context("add the start-time layer")?;
    trace.mark(p, "start-time layer applied");
    crate::image::begin_boot(&p.data, &cfg.image, &p.root.join("innkeeper"))?;
    trace.mark(p, "data version recorded");
    sandbox_root(cfg).context("set up the sandbox's /")?;
    trace.mark(p, "sandbox root ready");
    session::start_kmsg_reader(p)?;
    session::start_bridge(cfg)?;
    trace.mark(p, "bridge ready");
    let log = append(&p.session_log())?;
    // realpath (via /proc/self/fd) must retain the public volume path:
    // MediaProvider treats canonical /data/media paths as internal storage.
    let root_map = format!(
        "{}=/\n{}=/data\n{}=/storage/emulated",
        p.root.display(),
        p.data.display(),
        p.data.join("media").display()
    );
    let mut cmd = init::command(cfg, Path::new("/"), &[("INNKEEPER_ROOT_MAP", root_map)]);
    // pivot-exec becomes the broker, init's parent and subreaper; it dies
    // with this thread.
    unsafe {
        cmd.pre_exec(|| {
            sys::die_with_parent(libc::SIGKILL);
            Ok(())
        })
    };
    let mut pivot = cmd
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()
        .context("start pivot-exec")?;
    let outer = pivot.id() as i32;
    trace.mark(p, "pivot spawned");
    let mut init = None;
    for _ in 0..300 {
        init = sys::children(outer, &["init"]).first().copied();
        if init.is_some() || pivot.try_wait()?.is_some() {
            break;
        }
        sleep(Duration::from_millis(50));
    }
    let Some(init) = init else {
        bail!("init did not start (see {})", p.session_log().display());
    };
    session::write_pids(p, outer, init)?;
    trace.mark(p, "init found");
    let paths = p.clone();
    control::serve(
        &p.control(),
        control::Served {
            status: crate::Status { pid: Some(init) }.text(),
            exec_lock: Some(p.exec_lock()),
            stop: Box::new(move || teardown(&paths)),
        },
    )?;
    let (idle, paths) = (idle.clone(), p.clone());
    lifecycle::watch(cfg, move |wakeup| {
        *idle.lock().unwrap() = Some(wakeup);
        // As `stop` does: killing the broker alone would orphan init.
        teardown(&paths);
    });
    println!(
        "session started ({}, in Flatpak): Android init = pid {init}; log: {}",
        cfg.image.name,
        p.kmsg_log().display()
    );
    if follow {
        // A closed stdout ends the log, not the session.
        let _ = session::tail(&p.kmsg_log(), &|| sys::alive(init));
    }
    pivot.wait()?;
    Ok(())
}

/// Starts the session in a sandbox of its own, through the Flatpak portal's
/// Spawn (`flatpak-spawn` without `--host`: a new instance of this app with
/// its permissions, not an escape), so it outlives the caller's sandbox.
/// Returns once it is up.
pub fn spawn(p: &Paths, exe: &Path, headless: bool) -> Result<()> {
    let mut c = Command::new("flatpak-spawn");
    c.arg("/usr/bin/env");
    for (k, v) in env::vars_os() {
        let name = k.to_string_lossy();
        if name.starts_with("INNKEEPER_")
            || [
                "XDG_DATA_HOME",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_STATE_HOME",
            ]
            .contains(&name.as_ref())
        {
            // Flatpak sets XDG_*_HOME in the new sandbox over any --env;
            // env(1) sets them after it.
            let mut a = OsString::new();
            a.push(&k);
            a.push("=");
            a.push(&v);
            c.arg(a);
        }
    }
    c.arg(exe).arg("_serve");
    if headless {
        c.arg("--headless");
    }
    fs::create_dir_all(&p.state)?;
    let mut child = c
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(append(&p.state.join("spawn.log"))?)
        .spawn()
        .context("run flatpak-spawn")?;
    for _ in 0..600 {
        if let Some(s) = control::status(&p.control()) {
            // flatpak-spawn would wait for the session to end (and keep this
            // sandbox alive meanwhile); SIGKILL isn't forwarded to it.
            let _ = child.kill();
            let _ = child.wait();
            let pid = crate::Status::parse(&s)
                .pid
                .map_or("?".into(), |p| p.to_string());
            println!(
                "session started in a sandbox of its own: Android init = pid {pid} there; log: {}",
                p.kmsg_log().display()
            );
            let _ = std::io::stdout().flush();
            return Ok(());
        }
        if let Some(st) = child.try_wait()? {
            eprint!(
                "{}",
                fs::read_to_string(p.session_log()).unwrap_or_default()
            );
            bail!(
                "session failed to start ({st}; see {})",
                p.state.join("spawn.log").display()
            );
        }
        sleep(Duration::from_millis(100));
    }
    bail!("the session did not come up in 60 s")
}
