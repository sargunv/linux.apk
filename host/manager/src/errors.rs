//! Turns `innkeeper-core` errors into messages for people: a heading, a
//! sentence or two, and the original text as details.

/// What the user was doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Read,
    Install,
    Open,
    Uninstall,
    Session,
    AppSettings,
    Preferences,
    Profiles,
    /// Downloading and unpacking the Android image.
    Prepare,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Humane {
    pub heading: String,
    pub body: String,
    /// The full error chain, for a "Details" expander.
    pub details: String,
}

/// `name` is the app's name where one is known.
pub fn humane(op: Op, name: Option<&str>, err: &anyhow::Error) -> Humane {
    let details = format!("{err:#}");
    let (heading, body) = classify(op, name, err.downcast_ref::<innkeeper_core::ErrorKind>());
    Humane {
        heading,
        body,
        details,
    }
}

fn classify(
    op: Op,
    name: Option<&str>,
    kind: Option<&innkeeper_core::ErrorKind>,
) -> (String, String) {
    use innkeeper_core::ErrorKind::*;
    let app = name.unwrap_or("the app");
    let pair = |h: &str, b: &str| (h.to_string(), b.to_string());
    match kind {
        Some(NotAnApk) => pair("Not a Readable Android App", "This file couldn’t be read as an APK or APK bundle. It may be incomplete or corrupt; try downloading it again."),
        Some(UnsupportedBundle) => pair("Unsupported Bundle", "This bundle doesn’t contain exactly one installable app. Encrypted bundles aren’t supported."),
        Some(WrongAbi) => pair("Not Built for This Computer", "This app’s native code doesn’t support this computer’s processor."),
        Some(SessionStartFailed) => pair("Android Couldn’t Start", "The Android session didn’t start. Try again, or check the details below."),
        Some(ImageNotPrepared) => pair("Android Isn’t Ready", "The Android image couldn’t be prepared. Check your connection and free disk space, then try again."),
        Some(NoLauncherActivity) => (format!("{} Can’t Be Opened", cap(app)), "It has no main screen. It may be a keyboard, a widget or a background service, which run inside other apps.".into()),
        Some(Timeout) => pair("Android Didn’t Respond", "The Android session took too long to answer. Try again in a moment."),
        Some(InstallFailed(code)) => match code.as_str() {
            "INSTALL_FAILED_UPDATE_INCOMPATIBLE" => ("A Different Version Is Installed".into(), format!("The installed copy of {app} was signed by someone else. Uninstall it first to install this one.")),
            "INSTALL_FAILED_VERSION_DOWNGRADE" => ("A Newer Version Is Installed".into(), format!("This is an older version of {app} than the one already installed.")),
            "INSTALL_FAILED_OLDER_SDK" => pair("Needs a Newer Android", "This app requires a newer version of Android than Innkeeper runs."),
            "INSTALL_FAILED_INSUFFICIENT_STORAGE" => pair("Not Enough Space", "There isn’t enough free disk space to install this app."),
            "INSTALL_FAILED_NO_MATCHING_ABIS" | "INSTALL_FAILED_CPU_ABI_INCOMPATIBLE" => pair("Not Built for This Computer", "This app’s native code doesn’t support this computer’s processor."),
            _ => pair("Android Refused the App", "Android couldn’t install this app. Check the details below for its reason."),
        },
        None => {
            let heading = match op {
                Op::Preferences => "Couldn’t Save Preferences".to_string(),
                Op::Profiles => "Couldn’t Change Profiles".to_string(),
                Op::Read => "Couldn’t Read the File".to_string(),
                Op::Install => format!("Couldn’t Install {}", cap(app)),
                Op::Open => format!("Couldn’t Open {}", cap(app)),
                Op::Uninstall => format!("Couldn’t Uninstall {}", cap(app)),
                Op::AppSettings => format!("Couldn’t Change {}’s Settings", cap(app)),
                Op::Session => "Couldn’t Control the Android Session".into(),
                Op::Prepare => "Couldn’t Download Android".into(),
            };
            (heading, "Something went wrong. Try again, or check the details below.".into())
        }
    }
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use innkeeper_core::ErrorKind;

    #[test]
    fn typed_context_selects_copy_independent_of_diagnostics() {
        for (kind, heading) in [
            (ErrorKind::NotAnApk, "Not a Readable Android App"),
            (ErrorKind::UnsupportedBundle, "Unsupported Bundle"),
            (ErrorKind::WrongAbi, "Not Built for This Computer"),
            (ErrorKind::SessionStartFailed, "Android Couldn’t Start"),
            (ErrorKind::ImageNotPrepared, "Android Isn’t Ready"),
            (ErrorKind::NoLauncherActivity, "Calculator Can’t Be Opened"),
            (ErrorKind::Timeout, "Android Didn’t Respond"),
            (
                ErrorKind::InstallFailed("INSTALL_FAILED_UPDATE_INCOMPATIBLE".into()),
                "A Different Version Is Installed",
            ),
            (
                ErrorKind::InstallFailed("INSTALL_FAILED_VERSION_DOWNGRADE".into()),
                "A Newer Version Is Installed",
            ),
            (
                ErrorKind::InstallFailed("INSTALL_FAILED_OLDER_SDK".into()),
                "Needs a Newer Android",
            ),
            (
                ErrorKind::InstallFailed("INSTALL_FAILED_INSUFFICIENT_STORAGE".into()),
                "Not Enough Space",
            ),
            (
                ErrorKind::InstallFailed("INSTALL_PARSE_FAILED_NO_CERTIFICATES".into()),
                "Android Refused the App",
            ),
        ] {
            let e = anyhow::anyhow!("arbitrary diagnostic")
                .context(kind)
                .context("outer operation");
            assert_eq!(humane(Op::Install, Some("Calculator"), &e).heading, heading);
        }
        let e = anyhow::anyhow!("INSTALL_FAILED_OLDER_SDK");
        let h = humane(Op::Open, Some("Calculator"), &e);
        assert_eq!(h.heading, "Couldn’t Open Calculator");
        assert_eq!(h.details, "INSTALL_FAILED_OLDER_SDK");
    }
}
