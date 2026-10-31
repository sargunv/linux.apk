//! Profiles ([`innkeeper_session::profile`]) as the CLI and the manager show
//! them: listed with their state and settings, created, renamed, deleted,
//! started and stopped, and the one to use when none is named. Each works on
//! the named profile, whatever this process's is.

use std::fs;

use anyhow::{Result, bail};
use innkeeper_session::profile;
pub use innkeeper_session::profile::{FIRST, Mode, Settings, Size, check_name};

use crate::launcher::Session;
use crate::paths::Prefix;
use crate::xdg::{self, Dirs};

/// A profile and what it is doing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub running: bool,
    pub settings: Settings,
}

fn paths(name: &str) -> Result<innkeeper_session::Paths> {
    Prefix::locate()?.profile_paths(name)
}

/// The profiles' names.
fn names() -> Vec<String> {
    let mut v = profile::list(&innkeeper_session::paths::data_home());
    // One whose data lives elsewhere (`INNKEEPER_DATA`) is still one.
    if let Ok(p) = crate::paths::profile()
        && !v.contains(&p)
    {
        v.push(p);
    }
    v
}

/// Every profile, by name. One whose settings can't be read shows the
/// defaults; [`settings`] reports the error.
pub fn list() -> Result<Vec<Profile>> {
    names()
        .into_iter()
        .map(|name| {
            let p = paths(&name)?;
            Ok(Profile {
                running: innkeeper_session::status(&p).pid.is_some(),
                settings: Settings::load(&p.data).unwrap_or_default(),
                name,
            })
        })
        .collect()
}

/// The profile to use when none is named (`--profile`, `INNKEEPER_PROFILE`):
/// the one last [`choose`]n, else the first. With none, `create` makes
/// [`FIRST`].
pub fn resolve(create: bool) -> Result<String> {
    match crate::paths::profile() {
        Ok(p) => Ok(p),
        Err(_) => fallback(create),
    }
}

/// [`resolve`], whatever this process's profile is.
pub fn fallback(create: bool) -> Result<String> {
    let names = profile::list(&innkeeper_session::paths::data_home());
    let chosen = fs::read_to_string(profile::chosen_file()).ok();
    if let Some(p) = profile::pick(&names, chosen.as_deref().map(str::trim)) {
        return Ok(p.clone());
    }
    if !create {
        bail!("there are no profiles yet");
    }
    self::create(FIRST, &Settings::default())?;
    Ok(FIRST.into())
}

/// Makes `name` the profile used when none is named.
pub fn choose(name: &str) -> Result<()> {
    check_name(name)?;
    let file = profile::chosen_file();
    fs::create_dir_all(file.parent().unwrap())?;
    fs::write(file, format!("{name}\n"))?;
    Ok(())
}

/// Profile `name`'s settings.
pub fn settings(name: &str) -> Result<Settings> {
    Settings::load(&paths(name)?.data)
}

/// Sets profile `name`'s mode and screen preset, for its next start.
pub fn set_mode(name: &str, mode: Option<Mode>, size: Option<Size>) -> Result<Settings> {
    let data = paths(name)?.data;
    let mut s = Settings::load(&data)?;
    s.mode = mode.unwrap_or(s.mode);
    s.size = size.unwrap_or(s.size);
    s.save(&data)?;
    Ok(s)
}

/// Creates profile `name` with `settings`. Android sets up its data at the
/// first start.
pub fn create(name: &str, settings: &Settings) -> Result<()> {
    let p = paths(name)?;
    if p.data.exists() {
        bail!("profile {name} exists already");
    }
    settings.save(&p.data)
}

/// Renames profile `from` to `to`, with its apps' app-menu entries. It must
/// be stopped.
pub fn rename(from: &str, to: &str) -> Result<()> {
    profile::rename(&paths(from)?, &paths(to)?)?;
    xdg::move_entries(&Dirs::from_env(), from, to);
    if chosen().as_deref() == Some(from) {
        choose(to)?;
    }
    Ok(())
}

/// Deletes profile `name`: its apps and their data, its settings and its
/// app-menu entries. It must be stopped.
pub fn delete(name: &str) -> Result<()> {
    let p = paths(name)?;
    if !p.data.exists() {
        bail!("no profile {name}");
    }
    profile::delete(&p)?;
    let d = Dirs::from_env();
    for app in xdg::desktop_apps(&d) {
        if app.profile == name {
            xdg::remove_app(&d, &app.package);
        }
    }
    if chosen().as_deref() == Some(name) {
        let _ = fs::remove_file(profile::chosen_file());
    }
    Ok(())
}

fn chosen() -> Option<String> {
    fs::read_to_string(profile::chosen_file())
        .ok()
        .map(|s| s.trim().to_string())
}

/// Starts profile `name` (with windows) and waits for it to boot.
pub fn start(name: &str, status: &dyn Fn(&str)) -> Result<()> {
    Session::of_profile(name)?.ensure(status)
}

/// Stops profile `name`, if it runs.
pub fn stop(name: &str) -> Result<()> {
    Session::of_profile(name)?.stop()
}
