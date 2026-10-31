//! Desktop Entry (`.desktop`) generation, escaped per the Desktop Entry
//! Specification 1.5: string escapes (`\s \n \t \r \\`), list separators
//! (`\;`) and the Exec quoting rules (reserved characters, `%%`).

use std::fmt::Write as _;

use anyhow::{Result, bail};

use crate::validate::is_forbidden_char;

/// Desktop-file ID and icon name prefix; `StartupWMClass` uses it too.
/// The manager's own id is `dev.sargunv.Innkeeper` (capital I), so it does not
/// share this prefix and a directory watch of app entries ignores it.
pub const PREFIX: &str = "dev.sargunv.innkeeper.";
/// The manager app (`innkeeper-manager`): its application ID, desktop-file ID and
/// icon name. The capital I keeps it off [`PREFIX`], so no Android package can
/// collide with it.
pub const MANAGER_ID: &str = "dev.sargunv.Innkeeper";
/// The manager's icon (also used where an app has no icon of its own).
pub const MANAGER_ICON_SVG: &str = include_str!("../../manager/data/dev.sargunv.Innkeeper.svg");

/// Innkeeper's Flatpak app id (the dev build's differs from the release's).
pub fn flatpak_app_id() -> String {
    std::env::var("FLATPAK_ID").unwrap_or_else(|_| MANAGER_ID.into())
}

/// The id desktops match an app's windows (`app_id`, `StartupWMClass`),
/// notifications and MPRIS players to its menu entry by: `<app id>.<pkg>`,
/// the id the portal gives the entry and the only prefix a compositor lets a
/// sandboxed client's windows claim.
pub fn app_id(pkg: &str) -> String {
    format!("{}.{pkg}", flatpak_app_id())
}

/// [`app_id`] for an id Android sent (`dev.sargunv.innkeeper.<pkg>`, the system
/// app's window ids); anything else is kept.
///
/// The Android prefix and the Flatpak app id differ only by the case of
/// `innkeeper` (`dev.sargunv.innkeeper.` versus `dev.sargunv.Innkeeper.`). A
/// case-sensitive strip would miss the guest id and prefix it again.
pub fn desktop_app_id(android: &str) -> String {
    match android
        .get(PREFIX.len()..)
        .filter(|_| android.len() > PREFIX.len())
        .filter(|_| android[..PREFIX.len()].eq_ignore_ascii_case(PREFIX))
    {
        Some(pkg) => app_id(pkg),
        None => android.to_string(),
    }
}

/// Escapes a `string`/`localestring` value. Refuses control characters other
/// than the ones the spec has escapes for (we never write those either, but
/// the escaping must hold on its own).
pub fn escape_value(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    for (i, c) in s.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            ' ' if i == 0 => out.push_str("\\s"),
            c if is_forbidden_char(c) => bail!(
                "control character U+{:04X} in a desktop entry value",
                c as u32
            ),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// Escapes one item of a `;`-separated list value and appends the separator.
fn list(items: &[impl AsRef<str>]) -> Result<String> {
    let mut out = String::new();
    for it in items {
        out.push_str(&escape_value(it.as_ref())?.replace(';', "\\;"));
        out.push(';');
    }
    Ok(out)
}

/// Quotes one Exec argument (before the general string escaping, which
/// `exec_line` applies afterwards). Field codes are passed through unquoted by
/// `exec_line`, so every `%` here is literal.
pub fn quote_exec_arg(arg: &str) -> Result<String> {
    if arg.is_empty() {
        return Ok("\"\"".into());
    }
    if arg.chars().any(is_forbidden_char) {
        bail!("control character in an Exec argument");
    }
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#`()";
    let arg = arg.replace('%', "%%");
    if !arg.chars().any(|c| RESERVED.contains(c)) {
        return Ok(arg);
    }
    let mut q = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            q.push('\\');
        }
        q.push(c);
    }
    q.push('"');
    Ok(q)
}

/// Builds an Exec value from literal arguments plus an optional field code.
pub fn exec_line(args: &[String], field_code: Option<&str>) -> Result<String> {
    let mut parts = args
        .iter()
        .map(|a| quote_exec_arg(a))
        .collect::<Result<Vec<_>>>()?;
    if let Some(f) = field_code {
        parts.push(f.to_string());
    }
    escape_value(&parts.join(" "))
}

/// Splits an Exec value (already string-unescaped, as [`read_key`] returns
/// it) back into arguments: the reverse of [`exec_line`]'s quoting. Field
/// codes stay as they are.
pub fn exec_args(exec: &str) -> Vec<String> {
    let mut args = vec![];
    let mut it = exec.chars().peekable();
    loop {
        while it.next_if(|c| *c == ' ' || *c == '\t').is_some() {}
        let Some(&first) = it.peek() else {
            return args;
        };
        let mut arg = String::new();
        if first == '"' {
            it.next();
            while let Some(c) = it.next() {
                match c {
                    '"' => break,
                    '\\' => arg.extend(it.next()),
                    c => arg.push(c),
                }
            }
        } else {
            while let Some(c) = it.next_if(|c| *c != ' ' && *c != '\t') {
                arg.push(c);
            }
        }
        args.push(arg.replace("%%", "%"));
    }
}

/// An installed Android app's launcher entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppEntry {
    pub package: String,
    pub name: String,
    /// (desktop locale, label), e.g. ("pt_BR", "Calculadora").
    pub names: Vec<(String, String)>,
    /// Command prefix; `<package>` and `%U` are appended (`innkeeper run`).
    pub run: Vec<String>,
    pub icon: Option<String>,
    /// `versionName`, recorded as `X-Innkeeper-Version` (omitted if empty).
    pub version: String,
    pub categories: Vec<String>,
    /// Custom URI schemes, declared as `x-scheme-handler/<scheme>`.
    pub schemes: Vec<String>,
    /// Hosts of the app's verified-style `http(s)` deep links. Metadata only:
    /// `http`/`https` are never declared for an app.
    pub web_hosts: Vec<String>,
    /// Who wrote the entry, as `X-Innkeeper-Origin` (`android`: the bridge, for
    /// an app installed inside Android). None: `innkeeper install`.
    pub origin: Option<String>,
    /// What its launch splash looks like, as `X-Innkeeper-Splash` and
    /// `X-Innkeeper-Window`.
    pub splash: Option<Splash>,
    /// The launcher activity accepts Android MULTIPLE_TASK.
    pub new_window: bool,
}

/// An app's launch splash: the window its launcher activity opens in, in
/// logical pixels, filled with its splash background colour (`0xRRGGBB`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splash {
    pub color: u32,
    pub width: u32,
    pub height: u32,
}

impl Splash {
    /// From an entry's `X-Innkeeper-Splash=#rrggbb` and `X-Innkeeper-Window=WxH`.
    pub fn parse(color: &str, window: &str) -> Option<Splash> {
        let color = u32::from_str_radix(color.strip_prefix('#')?, 16)
            .ok()
            .filter(|c| *c <= 0xff_ffff)?;
        let (w, h) = window.split_once('x')?;
        let (width, height) = (w.parse().ok()?, h.parse().ok()?);
        let ok = |v: u32| (1..=16384).contains(&v);
        (ok(width) && ok(height)).then_some(Splash {
            color,
            width,
            height,
        })
    }
}

impl AppEntry {
    pub fn id(&self) -> String {
        format!("{PREFIX}{}", self.package)
    }

    pub fn render(&self) -> Result<String> {
        let mut s = String::from("[Desktop Entry]\nType=Application\nVersion=1.5\n");
        writeln!(s, "Name={}", escape_value(&self.name)?)?;
        for (loc, n) in &self.names {
            // Locale keys are generated from a validated grammar; check again.
            if !loc.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("bad locale key {loc:?}");
            }
            writeln!(s, "Name[{loc}]={}", escape_value(n)?)?;
        }
        writeln!(s, "Comment=Android app ({})", escape_value(&self.package)?)?;
        if let Some(icon) = &self.icon {
            writeln!(s, "Icon={}", escape_value(icon)?)?;
        }
        let mut run = self.run.clone();
        run.push(self.package.clone());
        writeln!(s, "Exec={}", exec_line(&run, Some("%U"))?)?;
        // The launcher then passes an activation token (XDG_ACTIVATION_TOKEN),
        // which `run` hands on to the app's window.
        s.push_str("Terminal=false\nStartupNotify=true\n");
        writeln!(
            s,
            "StartupWMClass={}",
            escape_value(&app_id(&self.package))?
        )?;
        if !self.categories.is_empty() {
            writeln!(s, "Categories={}", list(&self.categories)?)?;
        }
        if !self.schemes.is_empty() {
            let mimes: Vec<String> = self
                .schemes
                .iter()
                .map(|sc| format!("x-scheme-handler/{sc}"))
                .collect();
            writeln!(s, "MimeType={}", list(&mimes)?)?;
        }
        s.push_str("Keywords=Android;\n");
        writeln!(s, "X-Innkeeper-Package={}", escape_value(&self.package)?)?;
        if !self.version.is_empty() {
            writeln!(s, "X-Innkeeper-Version={}", escape_value(&self.version)?)?;
        }
        if !self.web_hosts.is_empty() {
            writeln!(s, "X-Innkeeper-Web-Hosts={}", list(&self.web_hosts)?)?;
        }
        if let Some(o) = &self.origin {
            writeln!(s, "X-Innkeeper-Origin={}", escape_value(o)?)?;
        }
        if let Some(sp) = &self.splash {
            writeln!(s, "X-Innkeeper-Splash=#{:06x}", sp.color & 0xff_ffff)?;
            writeln!(s, "X-Innkeeper-Window={}x{}", sp.width, sp.height)?;
        }
        writeln!(s, "X-Innkeeper-NewWindow={}", self.new_window)?;
        if self.new_window {
            s.push_str("Actions=NewWindow;\n\n[Desktop Action NewWindow]\nName=New Window\n");
            let mut run = self.run.clone();
            run.extend(["--new-window".into(), self.package.clone()]);
            writeln!(s, "Exec={}", exec_line(&run, None)?)?;
        }
        Ok(s)
    }
}

/// The package formats the manager opens: plain APKs, and the bundle formats
/// its MIME definitions (`share/mime/packages/dev.sargunv.Innkeeper.xml`) add.
pub const PACKAGE_MIME_TYPES: &[&str] = &[
    "application/vnd.android.package-archive",
    "application/x-android-apks",
    "application/x-android-xapk",
    "application/x-android-apkm",
];

/// Category mapping for `ApplicationInfo.category` / `android:appCategory`.
pub fn categories_for(app_category: Option<i32>, is_game: bool) -> Vec<String> {
    let v: &[&str] = if is_game {
        &["Game"]
    } else {
        match app_category {
            Some(0) => &["Game"],
            Some(1) => &["AudioVideo", "Audio"],
            Some(2) => &["AudioVideo", "Video"],
            Some(3) => &["Graphics"],
            Some(4) => &["Network", "InstantMessaging"],
            Some(5) => &["Network", "News"],
            Some(6) => &["Utility", "Maps"],
            Some(7) => &["Office"],
            Some(8) => &["Utility", "Accessibility"],
            _ => &["Utility"],
        }
    };
    v.iter().map(|s| s.to_string()).collect()
}

/// Reads one key from the `[Desktop Entry]` group (unescaped for the simple
/// escapes; enough for reading back our own files).
pub fn read_key(text: &str, key: &str) -> Option<String> {
    let mut in_group = false;
    for line in text.lines() {
        if line.starts_with('[') {
            in_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_group {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim_end() == key
        {
            let v = v.trim_start();
            let mut out = String::new();
            let mut it = v.chars();
            while let Some(c) = it.next() {
                if c == '\\' {
                    match it.next() {
                        Some('s') => out.push(' '),
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some(o) => out.push(o),
                        None => {}
                    }
                } else {
                    out.push(c);
                }
            }
            return Some(out);
        }
    }
    None
}

/// `text` with the value of `key` in the `[Desktop Entry]` group replaced by
/// `value` (already escaped); every other line is kept as it is. None if the
/// key isn't there.
pub fn replace_key(text: &str, key: &str, value: &str) -> Option<String> {
    replace_group_key(text, "Desktop Entry", key, value)
}

/// Replaces a key in a named desktop-file group, keeping all other lines.
pub fn replace_group_key(text: &str, group: &str, key: &str, value: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut in_group = false;
    let mut found = false;
    for line in text.split_inclusive('\n') {
        if line.starts_with('[') {
            in_group = line.trim_end() == format!("[{group}]");
        } else if in_group
            && !found
            && line
                .split_once('=')
                .is_some_and(|(k, _)| k.trim_end() == key)
        {
            found = true;
            out.push_str(&format!("{key}={value}\n"));
            continue;
        }
        out.push_str(line);
    }
    found.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn android_window_ids_match_the_flatpak_prefix_ignoring_case() {
        let guest = "dev.sargunv.innkeeper.org.example";
        assert_eq!(desktop_app_id(guest), app_id("org.example"));
        assert_eq!(
            desktop_app_id("dev.sargunv.Innkeeper.org.example"),
            app_id("org.example")
        );
        assert_eq!(desktop_app_id("org.example"), "org.example");
        assert_eq!(
            desktop_app_id("dev.sargunv.innkeeper."),
            "dev.sargunv.innkeeper."
        );
    }

    fn entry() -> AppEntry {
        AppEntry {
            package: "org.wikipedia".into(),
            name: "Wikipedia".into(),
            names: vec![
                ("de".into(), "Wikipedia DE".into()),
                ("pt_BR".into(), "Wikipédia".into()),
            ],
            run: vec!["innkeeper".into(), "run".into()],
            icon: Some("dev.sargunv.innkeeper.org.wikipedia".into()),
            version: "2.7.50".into(),
            categories: vec!["Utility".into()],
            schemes: vec!["wikipedia".into()],
            web_hosts: vec!["*.wikipedia.org".into()],
            origin: None,
            new_window: false,
            splash: Some(Splash {
                color: 0xf8f9fa,
                width: 1280,
                height: 800,
            }),
        }
    }

    #[test]
    fn renders_app_entry() {
        let s = entry().render().unwrap();
        assert_eq!(
            s,
            "[Desktop Entry]\nType=Application\nVersion=1.5\nName=Wikipedia\n\
             Name[de]=Wikipedia DE\nName[pt_BR]=Wikipédia\nComment=Android app (org.wikipedia)\n\
             Icon=dev.sargunv.innkeeper.org.wikipedia\n\
             Exec=innkeeper run org.wikipedia %U\nTerminal=false\n\
             StartupNotify=true\nStartupWMClass=dev.sargunv.Innkeeper.org.wikipedia\nCategories=Utility;\n\
             MimeType=x-scheme-handler/wikipedia;\nKeywords=Android;\n\
             X-Innkeeper-Package=org.wikipedia\nX-Innkeeper-Version=2.7.50\n\
             X-Innkeeper-Web-Hosts=*.wikipedia.org;\nX-Innkeeper-Splash=#f8f9fa\n\
             X-Innkeeper-Window=1280x800\nX-Innkeeper-NewWindow=false\n"
        );
        assert_eq!(
            Splash::parse(
                &read_key(&s, "X-Innkeeper-Splash").unwrap(),
                &read_key(&s, "X-Innkeeper-Window").unwrap()
            ),
            entry().splash
        );
        assert_eq!(Splash::parse("#1000000", "1x1"), None);
        assert_eq!(Splash::parse("#000000", "0x1"), None);
        assert_eq!(Splash::parse("000000", "1x1"), None);
        assert!(!s.contains("x-scheme-handler/http"));
    }

    #[test]
    fn new_window_action_keeps_the_launcher_command_and_profile() {
        let mut e = entry();
        assert!(!e.render().unwrap().contains("Desktop Action"));
        e.new_window = true;
        e.run = vec![
            "env".into(),
            "INNKEEPER_PROFILE=test".into(),
            "innkeeper".into(),
            "run".into(),
        ];
        let text = e.render().unwrap();
        assert!(text.contains("Actions=NewWindow;\n"));
        assert!(text.ends_with("[Desktop Action NewWindow]\nName=New Window\nExec=env INNKEEPER_PROFILE=test innkeeper run --new-window org.wikipedia\n"));
    }

    #[test]
    fn no_mimetype_without_schemes() {
        let mut e = entry();
        e.schemes.clear();
        e.web_hosts.clear();
        let s = e.render().unwrap();
        assert!(!s.contains("MimeType="));
        assert!(!s.contains("X-Innkeeper-Web-Hosts"));
    }

    #[test]
    fn value_escaping() {
        assert_eq!(escape_value("a\\b").unwrap(), "a\\\\b");
        assert_eq!(escape_value(" lead").unwrap(), "\\slead");
        assert_eq!(escape_value("mid space").unwrap(), "mid space");
        assert_eq!(escape_value("a\nb\tc\rd").unwrap(), "a\\nb\\tc\\rd");
        assert!(escape_value("a\u{1b}[31m").is_err());
        assert!(escape_value("a\u{0}").is_err());
        assert!(escape_value("x\u{202E}y").is_err());
    }

    #[test]
    fn label_cannot_inject_keys() {
        let mut e = entry();
        e.name = "Calc\nExec=rm -rf ~".into();
        // Validated labels never contain newlines; even if one got here it is
        // escaped onto a single line.
        let s = e.render().unwrap();
        assert!(s.contains("Name=Calc\\nExec=rm -rf ~\n"));
        assert_eq!(s.lines().filter(|l| l.starts_with("Exec=")).count(), 1);
        e.name = "Calc\u{7}".into();
        assert!(e.render().is_err());
    }

    #[test]
    fn exec_quoting() {
        assert_eq!(
            quote_exec_arg("/usr/bin/innkeeper").unwrap(),
            "/usr/bin/innkeeper"
        );
        assert_eq!(quote_exec_arg("/a b/x").unwrap(), "\"/a b/x\"");
        assert_eq!(quote_exec_arg("50%").unwrap(), "50%%");
        assert_eq!(quote_exec_arg("$HOME").unwrap(), "\"\\$HOME\"");
        assert_eq!(quote_exec_arg("a\"b").unwrap(), "\"a\\\"b\"");
        assert_eq!(quote_exec_arg("a`b`").unwrap(), "\"a\\`b\\`\"");
        assert_eq!(quote_exec_arg("").unwrap(), "\"\"");
        assert!(quote_exec_arg("a\nb").is_err());
        // Backslash: quoted (\\) then string-escaped (\\\\).
        assert_eq!(exec_line(&["a\\b".into()], None).unwrap(), "\"a\\\\\\\\b\"");
    }

    #[test]
    fn exec_round_trip() {
        let args: Vec<String> = [
            "/opt/linux apk/bin/innkeeper",
            "a\\b",
            "50%",
            "$HOME",
            "",
            "x\"y",
        ]
        .map(String::from)
        .into();
        let line = exec_line(&args, Some("%U")).unwrap();
        let mut back = args.clone();
        back.push("%U".into());
        let unescaped = read_key(&format!("[Desktop Entry]\nExec={line}\n"), "Exec").unwrap();
        assert_eq!(exec_args(&unescaped), back);
    }

    #[test]
    fn replaces_one_key() {
        let t = "[Desktop Entry]\nName=A\nExec=old\n[Desktop Action x]\nExec=other\n";
        assert_eq!(
            replace_key(t, "Exec", "new").as_deref(),
            Some("[Desktop Entry]\nName=A\nExec=new\n[Desktop Action x]\nExec=other\n")
        );
        assert_eq!(
            replace_key("[Desktop Entry]\nName=A\n", "Exec", "new"),
            None
        );
    }

    #[test]
    fn reads_back_keys() {
        let s = entry().render().unwrap();
        assert_eq!(read_key(&s, "Name").as_deref(), Some("Wikipedia"));
        assert_eq!(
            read_key(&s, "X-Innkeeper-Package").as_deref(),
            Some("org.wikipedia")
        );
        assert_eq!(read_key("[Other]\nName=x\n", "Name"), None);
        assert_eq!(
            read_key("[Desktop Entry]\nName=\\sa\\\\b\n", "Name").as_deref(),
            Some(" a\\b")
        );
    }
}
