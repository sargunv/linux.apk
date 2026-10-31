//! `tests/compat/corpus.toml` and fetching the pinned APKs.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Deserialize)]
pub struct Corpus {
    pub app: Vec<App>,
}

#[derive(Deserialize, Clone)]
pub struct App {
    pub package: String,
    pub name: String,
    pub group: String,
    pub version_code: u64,
    pub version_name: String,
    pub url: String,
    pub sha256: String,
    pub settle: Option<u64>,
    #[serde(default)]
    pub script: Vec<String>,
}

impl App {
    pub fn file_name(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }
}

pub fn load(path: &Path) -> Result<Corpus> {
    let s = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    Ok(toml::from_str(&s)?)
}

fn sha256(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Downloads the APK into `dir` if missing and checks its SHA-256.
pub fn fetch(app: &App, dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    let dst = dir.join(app.file_name());
    if dst.exists() && sha256(&dst)? == app.sha256 {
        return Ok(dst);
    }
    let part = dst.with_extension("apk.part");
    eprintln!("compat: downloading {}", app.url);
    let st = Command::new("curl")
        .args(["-sSfL", "--retry", "3", "-o"])
        .arg(&part)
        .arg(&app.url)
        .status()?;
    if !st.success() {
        bail!("download failed: {}", app.url);
    }
    let got = sha256(&part)?;
    if got != app.sha256 {
        let _ = fs::remove_file(&part);
        bail!("{}: SHA-256 mismatch (got {got})", app.file_name());
    }
    fs::rename(&part, &dst)?;
    Ok(dst)
}
