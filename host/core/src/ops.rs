//! User-level operations shared by the CLI and the GUI. They block (reading
//! APKs, the session, Android's package manager), so the GUI runs them off its UI
//! thread; progress and warnings go to a [`Report`] callback.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub use crate::android::LaunchOptions;
use crate::android::{self, Launch};
use crate::apk::{self, ApkInfo};
use crate::bundle::{self, Input};
use crate::desktop::{self, AppEntry};
use crate::launcher::Session;
use crate::paths;
use crate::xdg::{self, DesktopApp, Dirs};
use crate::{ErrorKind, validate};

/// Progress from a long operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// What it's doing now ("waiting for Android to boot").
    Status(String),
    /// Something the user should know that doesn't stop it.
    Warning(String),
}

pub type Report<'a> = &'a dyn Fn(Event);

/// A [`Report`] that ignores everything.
pub fn quiet(_: Event) {}

/// The command line desktop entries run `innkeeper` with, for this session
/// profile ([`xdg::INNKEEPER`]).
pub fn cli_command() -> Result<Vec<String>> {
    Ok(paths::command(&[xdg::INNKEEPER.into()], &paths::profile()?))
}

/// The running session, after starting it if needed.
fn session(report: Report) -> Result<Session> {
    let s = Session::find()?;
    s.ensure(&|m| report(Event::Status(m.to_string())))?;
    Ok(s)
}

/// Starts the session if it isn't running, and waits for it to boot.
pub fn ensure_session(report: Report) -> Result<()> {
    session(report).map(drop)
}

/// An APK or bundle, read and checked but not installed yet: what an install
/// confirmation shows.
pub struct Package {
    pub file: PathBuf,
    pub package: String,
    /// The app's name (validated; the package name if it has none).
    pub label: String,
    /// `versionName` (validated; may be empty) and `versionCode`.
    pub version: String,
    pub version_code: String,
    /// Declared permissions (`android.permission.CAMERA`, ...).
    pub permissions: Vec<String>,
    /// ABIs it has native code for (empty: none, runs anywhere).
    pub abis: Vec<String>,
    /// Size of the input file in bytes.
    pub size: u64,
    /// Number of APKs (1 for a plain APK).
    pub apk_count: usize,
    /// A host-rendered adaptive icon or the largest legacy bitmap.
    pub icon: Option<Vec<u8>>,
    input: Input,
    /// Metadata of each member of `input`, in order.
    info: Vec<ApkInfo>,
    base: usize,
}

impl Package {
    fn base(&self) -> (&Path, &ApkInfo) {
        (
            self.input.apks[self.base].path.as_path(),
            &self.info[self.base],
        )
    }

    /// Whether this image can run it, judging by its native code alone (the
    /// session's ABI list is checked again at install).
    pub fn runs_here(&self) -> bool {
        // Use the ABI declaration applied to the pinned image. It excludes
        // NativeBridge ABIs until a translator is installed (image/vendor.prop).
        let abis = include_str!("../../../image/vendor.prop")
            .lines()
            .find_map(|l| l.strip_prefix("ro.product.cpu.abilist="))
            .unwrap_or_default();
        self.abis.is_empty()
            || abis
                .split(',')
                .any(|a| self.abis.iter().any(|b| b == a.trim()))
    }
}

/// Reads an APK or bundle (no session needed).
pub fn inspect(file: &Path, report: Report) -> Result<Package> {
    let size = file
        .metadata()
        .with_context(|| format!("open {}", file.display()))?
        .len();
    let input = bundle::open(file, &Dirs::from_env().cache)?;
    for s in &input.skipped {
        report(Event::Warning(format!(
            "skipping {} (OBB expansion files aren't supported yet)",
            validate::truncate(s, 120)
        )));
    }
    let mut info = vec![];
    for m in &input.apks {
        info.push(
            apk::read(&m.path)
                .context(ErrorKind::NotAnApk)
                .with_context(|| format!("read {}", validate::truncate(&m.origin, 120)))?,
        );
    }
    let bases: Vec<usize> = (0..info.len())
        .filter(|i| info[*i].split.is_empty())
        .collect();
    if bases.len() != 1 {
        return Err(anyhow::anyhow!(
            "expected one base APK in {}, found {}",
            file.display(),
            bases.len()
        )
        .context(ErrorKind::UnsupportedBundle));
    }
    let base = &info[bases[0]];
    let pkg = validate::package(&base.package)?.to_string();
    if let Some(b) = info.iter().find(|b| b.package != pkg) {
        return Err(anyhow::anyhow!(
            "bundle mixes packages ({pkg} and {})",
            validate::truncate(&b.package, 80)
        )
        .context(ErrorKind::UnsupportedBundle));
    }
    let label = validate::label(&base.label).unwrap_or_else(|| pkg.clone());
    let mut abis: BTreeSet<String> = base.native_code.iter().cloned().collect();
    abis.extend(info.iter().filter_map(|b| bundle::split_abi(&b.split)));
    let mut permissions: BTreeSet<String> = BTreeSet::new();
    for b in &info {
        permissions.extend(b.permissions.iter().cloned());
    }
    let icon = apk::icon::render(&input.apks[bases[0]].path, &base.icons);
    Ok(Package {
        file: file.to_path_buf(),
        label,
        version: version_name(&base.version_name),
        version_code: base.version_code.clone(),
        permissions: permissions.into_iter().collect(),
        abis: abis.into_iter().collect(),
        size,
        apk_count: input.apks.len(),
        icon,
        package: pkg,
        input,
        info,
        base: bases[0],
    })
}

/// A `versionName` safe to show and to write into a desktop entry.
fn version_name(v: &str) -> String {
    validate::label(v)
        .map(|v| v.chars().take(64).collect())
        .unwrap_or_default()
}

/// What [`install`] did.
#[derive(Debug, Clone)]
pub struct Installed {
    pub package: String,
    pub name: String,
    pub menu: xdg::Menu,
    pub schemes: Vec<String>,
}

/// Installs an inspected package into the session (starting it if needed)
/// and adds it to the app menu.
pub fn install(p: &Package, report: Report) -> Result<Installed> {
    let dirs = Dirs::from_env();
    let pkg = p.package.clone();
    let s = session(report)?;
    let abis: Vec<String> = s
        .getprop("ro.product.cpu.abilist")
        .split(',')
        .map(|a| a.trim().to_string())
        .collect();

    // Base first, then splits; ABI splits only for ABIs the session runs.
    let (base_path, base) = p.base();
    let mut apks: Vec<&Path> = vec![base_path];
    let mut abi_splits = 0;
    for (m, b) in p.input.apks.iter().zip(&p.info) {
        if b.split.is_empty() {
            continue;
        }
        if let Some(abi) = bundle::split_abi(&b.split) {
            abi_splits += 1;
            if !abis.contains(&abi) {
                continue;
            }
        }
        apks.push(m.path.as_path());
    }
    if abi_splits > 0
        && !p
            .info
            .iter()
            .any(|b| bundle::split_abi(&b.split).is_some_and(|a| abis.contains(&a)))
    {
        return Err(anyhow::anyhow!(
            "{pkg} has no native code for this session's ABIs ({})",
            abis.join(", ")
        )
        .context(ErrorKind::WrongAbi));
    }
    if !base.native_code.is_empty()
        && abi_splits == 0
        && !base.native_code.iter().any(|c| abis.contains(c))
    {
        return Err(anyhow::anyhow!(
            "{pkg} only has native code for {} (this session runs {})",
            base.native_code.join(", "),
            abis.join(", ")
        )
        .context(ErrorKind::WrongAbi));
    }
    report(Event::Status(format!(
        "installing {pkg} {} ({} APK{})",
        validate::truncate(&base.version_name, 40),
        apks.len(),
        if apks.len() == 1 { "" } else { "s" }
    )));
    android::install(&s, &apks)?;

    report(Event::Status("adding it to the app menu".into()));
    let info = android::app_info(&s, &pkg, xdg::ICON_SIZES).unwrap_or_else(|e| {
        report(Event::Warning(format!(
            "couldn't render the icon in Android ({e:#}); using the APK's PNG if it has one"
        )));
        android::AppInfo::default()
    });
    let mut icons = info.icons.clone();
    if icons.is_empty() {
        icons = apk_png_icons(base_path, &base.icons);
    }

    let name = validate::label(&base.label)
        .or_else(|| validate::label(&info.label))
        .unwrap_or_else(|| {
            report(Event::Warning(
                "the app's label is missing or unsafe; using the package name".into(),
            ));
            pkg.clone()
        });
    let mut seen = BTreeSet::new();
    let names = base
        .labels
        .iter()
        .filter_map(|(tag, l)| Some((validate::desktop_locale(tag)?, validate::label(l)?)))
        .filter(|(loc, l)| *l != name && seen.insert(loc.clone()))
        .collect();
    let mut run = cli_command()?;
    run.push("run".into());
    let mut entry = AppEntry {
        package: pkg.clone(),
        name: name.clone(),
        names,
        run,
        icon: None,
        version: p.version.clone(),
        categories: desktop::categories_for(base.app_category.or(info.category), base.is_game),
        schemes: base.schemes.clone(),
        web_hosts: base.web_hosts.clone(),
        origin: None,
        new_window: info.new_window,
        splash: info
            .splash
            .zip(info.window)
            .map(|(color, (width, height))| desktop::Splash {
                color,
                width,
                height,
            }),
    };
    let menu = xdg::install_app(&dirs, &mut entry, &icons)?;
    Ok(Installed {
        package: pkg,
        name,
        menu,
        schemes: entry.schemes,
    })
}

/// Reads up to 4 MiB of one zip entry.
fn zip_entry(zip: &mut zip::ZipArchive<std::fs::File>, path: &str) -> Option<Vec<u8>> {
    let e = zip.by_name(path).ok()?;
    let mut buf = vec![];
    std::io::Read::read_to_end(&mut std::io::Read::take(e, 4 << 20), &mut buf).ok()?;
    Some(buf)
}

/// Fallback icons: the APK's own PNG launcher icons, by pixel size.
fn apk_png_icons(apk: &Path, icons: &[(u32, String)]) -> Vec<(u32, Vec<u8>)> {
    let Ok(f) = std::fs::File::open(apk) else {
        return vec![];
    };
    let Ok(mut zip) = zip::ZipArchive::new(f) else {
        return vec![];
    };
    let mut out: Vec<(u32, Vec<u8>)> = vec![];
    for (_, path) in icons {
        if !path.ends_with(".png") {
            continue;
        }
        let Some(buf) = zip_entry(&mut zip, path) else {
            continue;
        };
        if let Some((w, h)) = xdg::png_size(&buf)
            && w == h
            && !out.iter().any(|(s, _)| *s == w)
        {
            out.push((w, buf));
        }
    }
    out
}

/// What [`uninstall`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Uninstalled {
    /// Android had it installed (as a user app) and removed it.
    pub from_android: bool,
    /// Its desktop entry or icons existed and were removed.
    pub from_desktop: bool,
}

/// Uninstalls a user app (system apps are refused) and removes its desktop
/// entry and icons.
pub fn uninstall(pkg: &str, keep_data: bool, report: Report) -> Result<Uninstalled> {
    let pkg = validate::package(pkg)?;
    let s = session(report)?;
    let from_android = android::user_packages(&s)?.iter().any(|p| p == pkg);
    if from_android {
        android::uninstall(&s, pkg, keep_data)?;
    }
    let from_desktop = xdg::remove_app(&Dirs::from_env(), pkg);
    Ok(Uninstalled {
        from_android,
        from_desktop,
    })
}

/// Launches an app (in a window of its own), optionally opening a URI in it.
/// An empty URI is none: the DynamicLauncher portal quotes `%U` in the
/// entries it writes (`'%U'`), which launchers expand to an empty argument.
/// Returns the window manager's reply.
pub fn run(pkg: &str, uri: Option<&str>, opts: &LaunchOptions, report: Report) -> Result<String> {
    let pkg = validate::package(pkg)?;
    let uri = uri
        .filter(|u| !u.is_empty())
        .map(validate::launch_uri)
        .transpose()?;
    let launch = || -> Result<String> {
        let s = session(report)?;
        let l = match uri {
            Some(u) => Launch::View {
                package: pkg.into(),
                uri: u.into(),
            },
            None => Launch::Main {
                component: android::launcher_component(&s, pkg)?,
            },
        };
        android::launch(&s, &l, opts)
    };
    match launch() {
        // The session stopped for idleness just as this came: once more.
        Err(_) if !Session::find()?.running() => launch(),
        r => r,
    }
}

/// Sets whether `(package, allowed)` may run in the background with no
/// window open, keeping the session up (docs/session.md, "Lifecycle").
/// Returns the apps allowed.
pub fn background(set: Option<(&str, bool)>, report: Report) -> Result<Vec<String>> {
    let s = session(report)?;
    let set = set
        .map(|(p, on)| validate::package(p).map(|p| (p, on)))
        .transpose()?;
    android::background(&s, set)
}

/// User apps in the session, with the `Name` of their desktop entry.
pub fn list(report: Report) -> Result<Vec<(String, Option<String>)>> {
    let s = session(report)?;
    let d = Dirs::from_env();
    Ok(android::user_packages(&s)?
        .into_iter()
        .map(|p| {
            let l = xdg::app_label(&d, &p);
            (p, l)
        })
        .collect())
}

/// Apps installed for this session profile, from their desktop entries (no
/// session needed). If `android` is given (the session's user packages),
/// apps installed without an entry are added too.
pub fn installed_apps(android: Option<&[String]>) -> Vec<DesktopApp> {
    let profile = paths::profile().unwrap_or_default();
    let mut v: Vec<DesktopApp> = xdg::desktop_apps(&Dirs::from_env())
        .into_iter()
        .filter(|a| a.profile == profile)
        .collect();
    if let Some(pkgs) = android {
        for p in pkgs {
            if !v.iter().any(|a| &a.package == p) {
                v.push(DesktopApp {
                    package: p.clone(),
                    name: p.clone(),
                    version: None,
                    profile: profile.clone(),
                    icon: None,
                });
            }
        }
    }
    v
}

/// The session's user packages, if it is running and booted (doesn't start
/// it).
pub fn android_packages() -> Option<Vec<String>> {
    let s = Session::find().ok()?;
    if !s.running() || !s.booted() {
        return None;
    }
    android::user_packages(&s).ok()
}

/// Repairs app entries and offers missing app-menu entries again (see
/// [`xdg::refresh_entries`]). Returns how many entries were rewritten.
pub fn refresh_entries() -> usize {
    xdg::refresh_entries(&Dirs::from_env())
}

/// Shown once after the first session starts.
pub const SINGLE_UID_NOTICE: &str = "Android apps are not isolated from each other: \
    inside Flatpak they all run as your user, as desktop apps do.";
pub const SINGLE_UID_DOCS: &str =
    "https://github.com/sargunv/innkeeper/blob/main/docs/security.md#single-uid-mode";

/// [`SINGLE_UID_NOTICE`] if the session is running and the
/// notice hasn't been shown yet. It counts as shown from now on (a stamp in
/// [`paths::notices_dir`]).
pub fn single_uid_notice(running: bool) -> Option<&'static str> {
    notice_once(&paths::notices_dir(), running)
}

fn notice_once(dir: &Path, running: bool) -> Option<&'static str> {
    if !running {
        return None;
    }
    fs::create_dir_all(dir).ok()?;
    // Whoever creates the stamp shows the notice.
    fs::File::create_new(dir.join("single-uid")).ok()?;
    Some(SINGLE_UID_NOTICE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compatibility_uses_image_abis() {
        let dir = crate::test_dir("package-abis");
        let file = dir.join("fixture.apk");
        let mut zip = zip::ZipWriter::new(fs::File::create(&file).unwrap());
        zip.start_file(
            "AndroidManifest.xml",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.finish().unwrap();

        // Metadata fixtures: no third-party APK or native libraries needed.
        for (native_code, runnable) in [
            (vec!["x86"], false),
            (vec!["x86_64"], true),
            (vec![], true),
            (vec!["arm64-v8a"], false),
            (vec!["x86", "x86_64"], true),
        ] {
            let info = ApkInfo {
                native_code: native_code.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            };
            let p = Package {
                file: file.clone(),
                package: "com.example.fixture".into(),
                label: "Fixture".into(),
                version: String::new(),
                version_code: String::new(),
                permissions: vec![],
                abis: info.native_code.clone(),
                size: 0,
                apk_count: 1,
                icon: None,
                input: bundle::open(&file, &dir).unwrap(),
                info: vec![info],
                base: 0,
            };
            assert_eq!(p.runs_here(), runnable, "native code: {native_code:?}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn versions() {
        assert_eq!(version_name(" 2.7.50-r-2024 "), "2.7.50-r-2024");
        assert_eq!(version_name("1.0\u{202E}"), "");
        assert_eq!(version_name(&"9".repeat(100)).chars().count(), 64);
    }

    #[test]
    fn single_uid_notice_shows_once() {
        let dir = crate::test_dir("single-uid-notice");
        let notices = dir.join("notices");
        assert_eq!(notice_once(&notices, true), Some(SINGLE_UID_NOTICE));
        assert_eq!(notice_once(&notices, true), None);
        assert!(notices.join("single-uid").is_file());
        assert_eq!(notice_once(&dir.join("other"), false), None);
        assert_eq!(notice_once(&dir.join("none"), false), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
