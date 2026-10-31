//! Where things are: the install prefix this program runs from, the session
//! profile it works with (parsed by the launcher library, the only reader of
//! `INNKEEPER_PROFILE`), and the XDG base directories.

use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

/// The session launcher, relative to the prefix.
const LAUNCHER: &str = "libexec/innkeeper/innkeeper-session";

/// An installed Innkeeper: `/app` in the Flatpak, `out/stage` in a checkout.
pub struct Prefix(PathBuf);

impl Prefix {
    /// The prefix this program is installed in: the nearest ancestor of its
    /// executable that holds the launcher (so it works from `bin/` and from
    /// `libexec/innkeeper/`).
    pub fn locate() -> Result<Prefix> {
        let exe = env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("can't find this program's path")?;
        exe.ancestors()
            .skip(1)
            .find(|d| d.join(LAUNCHER).is_file())
            .map(|d| Prefix(d.to_path_buf()))
            .with_context(|| {
                format!(
                    "innkeeper-session not found: {} isn't in an installed Innkeeper \
                     (in a checkout, run `mise run build` and use out/stage)",
                    exe.display()
                )
            })
            .context(crate::ErrorKind::SessionStartFailed)
    }

    pub fn launcher(&self) -> PathBuf {
        self.0.join(LAUNCHER)
    }

    /// Runtime paths shared by the CLI and its session.
    pub fn session_paths(&self) -> Result<innkeeper_session::Paths> {
        innkeeper_session::Paths::new(&self.0)
    }

    /// Runtime paths of profile `name`.
    pub fn profile_paths(&self, name: &str) -> Result<innkeeper_session::Paths> {
        innkeeper_session::Paths::for_profile(&self.0, name)
    }
}

/// The profile this process works with ([`innkeeper_session::paths::select`]
/// or `INNKEEPER_PROFILE`), else an error.
pub use innkeeper_session::paths::{profile, select};

/// An XDG base directory: `$var` if it is absolute, else `default` under
/// `$HOME`.
pub use innkeeper_session::paths::xdg_home;

/// The argv that runs `program` (a command line, such as `innkeeper`)
/// against session `profile`: prefixed with `env INNKEEPER_PROFILE=<profile>`
/// for a named profile, so a desktop entry reaches the session it was made
/// for.
pub fn command(program: &[String], profile: &str) -> Vec<String> {
    let mut v = vec![];
    if !profile.is_empty() {
        v.push("env".into());
        v.push(format!("INNKEEPER_PROFILE={profile}"));
    }
    v.extend_from_slice(program);
    v
}

/// A [`command`] argv read back (from a desktop entry's `Exec`).
#[derive(Debug, PartialEq, Eq)]
pub struct Invocation<'a> {
    /// The profile it runs against, empty for the main session.
    pub profile: &'a str,
    /// The program, the first word after the environment.
    pub program: Option<&'a str>,
}

pub fn invocation(argv: &[String]) -> Invocation<'_> {
    let env_len = match argv.first().map(String::as_str) {
        Some("env") => 1 + argv[1..].iter().take_while(|w| w.contains('=')).count(),
        _ => 0,
    };
    let env = argv.get(1..env_len).unwrap_or_default();
    Invocation {
        profile: env
            .iter()
            .find_map(|w| w.strip_prefix("INNKEEPER_PROFILE="))
            .unwrap_or_default(),
        program: argv.get(env_len).map(String::as_str),
    }
}

/// Where one-time notices remember they were shown:
/// `$XDG_STATE_HOME/innkeeper/notices` (shared by every profile).
pub fn notices_dir() -> PathBuf {
    xdg_home("XDG_STATE_HOME", ".local/state").join("innkeeper/notices")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn commands_read_back() {
        let named = command(&argv("/x/innkeeper"), "cli");
        assert_eq!(named.join(" "), "env INNKEEPER_PROFILE=cli /x/innkeeper");
        let i = invocation(&named);
        assert_eq!((i.profile, i.program), ("cli", Some("/x/innkeeper")));
        let main = command(&argv("/x/innkeeper"), "");
        assert_eq!(invocation(&main).profile, "");
        // Only the environment `env` sets counts.
        let args = argv("innkeeper run INNKEEPER_PROFILE=x");
        let i = invocation(&args);
        assert_eq!((i.profile, i.program), ("", Some("innkeeper")));
    }
}
