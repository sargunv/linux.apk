//! session.conf: everything a session needs, decided once by `start` and read
//! by the session and selftest.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::{Image, Paths};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// No windows (tests): the composer's headless backend.
    pub headless: bool,
    /// The only DRM render node Android may open; None selects lavapipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_node: Option<PathBuf>,
    /// The Vulkan HAL matched to render_node at start (radv, anv or lvp).
    pub vulkan_driver: String,
    /// The host compositor's socket, for the bridge's windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wayland: Option<PathBuf>,
    /// Seconds without activity before the session stops itself; 0: never
    /// ([`crate::lifecycle`]).
    #[serde(default)]
    pub idle_grace: u64,
    /// The environment a wakeup starts the session again with.
    #[serde(default)]
    pub start_env: BTreeMap<String, String>,
    /// `INNKEEPER_BROKER_DEBUG`, passed to the broker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker_debug: Option<String>,
    /// The desktop bridge's arguments and environment.
    pub bridge_args: Vec<String>,
    pub bridge_env: BTreeMap<String, String>,
    pub paths: Paths,
    pub image: Image,
}

impl Config {
    pub fn new(paths: Paths, image: Image, headless: bool, wayland: Option<PathBuf>) -> Self {
        let selection = if env::var("INNKEEPER_VULKAN").as_deref() == Ok("lvp") {
            None
        } else {
            crate::graphics::select_node(
                Path::new("/sys/class/drm"),
                Path::new("/dev/dri"),
                &fs::read_to_string(paths.share.join("anv-pci-ids")).unwrap_or_default(),
            )
        };
        let (render_node, vulkan_driver) = selection
            .map(|(node, driver)| (Some(node), driver.to_string()))
            .unwrap_or((None, "lvp".to_string()));
        let mut c = Config {
            headless,
            render_node,
            vulkan_driver,
            wayland,
            idle_grace: crate::lifecycle::grace_secs(headless),
            start_env: crate::lifecycle::start_env(),
            broker_debug: env::var("INNKEEPER_BROKER_DEBUG")
                .ok()
                .filter(|v| !v.is_empty()),
            bridge_args: vec![],
            bridge_env: BTreeMap::new(),
            paths,
            image,
        };
        c.bridge_setup(env::var("INNKEEPER_TEST").is_ok_and(|v| v == "1"));
        c
    }

    pub fn load(path: &Path) -> Result<Self> {
        let s = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        toml::from_str(&s).with_context(|| format!("parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let p = self.paths.conf();
        fs::write(&p, toml::to_string(self)?).with_context(|| format!("write {}", p.display()))
    }

    /// The start-time layer (see [`crate::layer`]).
    pub fn run_layer(&self) -> PathBuf {
        self.paths.run.join("layer")
    }

    /// The desktop bridge's arguments and environment
    /// (host/bridge/README.md). It runs outside Android, as the user, and its
    /// socket dir is Android's `/dev/socket/innkeeper`.
    fn bridge_setup(&mut self, test: bool) {
        let p = &self.paths;
        let a = &mut self.bridge_args;
        for (k, v) in [("--run-dir", &p.run), ("--state-dir", &p.state)] {
            a.push(k.into());
            a.push(v.to_string_lossy().into_owned());
        }
        if let Some(sink) = env::var("INNKEEPER_AUDIO_SINK")
            .ok()
            .filter(|v| !v.is_empty())
        {
            a.push("--sink".into());
            a.push(sink);
        }
        if env::var("INNKEEPER_BRIDGE_VERBOSE").is_ok_and(|v| v == "1") {
            a.push("-v".into());
        }
        let bus = env::var("DBUS_SESSION_BUS_ADDRESS")
            .ok()
            .filter(|v| !v.is_empty());
        if test {
            // Persist isolation in session.conf, including across wakeups.
            a.push("--test-session".into());
        }
        // The display service and the clipboard use the host compositor; the
        // system service the caller's session bus; desktop entries for apps
        // installed inside Android go to the caller's XDG_DATA_HOME.
        let e = &mut self.bridge_env;
        e.insert(
            "WAYLAND_DISPLAY".into(),
            self.wayland
                .as_ref()
                .map(|w| w.to_string_lossy().into_owned())
                .unwrap_or_default(),
        );
        if let Some(b) = bus {
            e.insert("DBUS_SESSION_BUS_ADDRESS".into(), b);
        }
        // The host state service reads UPower on the caller's system bus,
        // and the settings service reads the desktop's locale (plasma-localerc
        // on KDE).
        for v in [
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_CURRENT_DESKTOP",
            "DBUS_SYSTEM_BUS_ADDRESS",
            "INNKEEPER_GAMEPAD_TEST_DIR",
            "INNKEEPER_BENCH",
            "INNKEEPER_AUDIO_SOURCE",
        ] {
            if let Some(d) = env::var(v).ok().filter(|v| !v.is_empty()) {
                e.insert(v.into(), d);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(profile: &str) -> Config {
        let dir = PathBuf::from("/nonexistent");
        Config {
            headless: true,
            render_node: None,
            vulkan_driver: "lvp".into(),
            wayland: None,
            idle_grace: 0,
            start_env: BTreeMap::new(),
            broker_debug: None,
            bridge_args: vec![],
            bridge_env: BTreeMap::new(),
            paths: Paths {
                profile: profile.into(),
                libexec: dir.clone(),
                share: dir.clone(),
                run: dir.clone(),
                state: dir.clone(),
                data_home: dir.clone(),
                data: dir.clone(),
                images: dir.clone(),
                pin: dir.clone(),
                root: dir.clone(),
            },
            image: Image {
                name: String::new(),
                dir: dir.clone(),
                img: dir.clone(),
                apexdir: dir.clone(),
                derived: dir,
            },
        }
    }

    #[test]
    fn only_the_test_switch_isolates_the_bridge() {
        let mut c = config("default");
        c.bridge_setup(false);
        assert!(!c.bridge_args.iter().any(|a| a == "--test-session"));
        let mut c = config("default");
        c.bridge_setup(true);
        assert!(c.bridge_args.iter().any(|a| a == "--test-session"));
    }
}
