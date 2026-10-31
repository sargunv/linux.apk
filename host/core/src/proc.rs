//! Child processes with argv only (no shell), a time limit and captured output.

use std::io::Read;
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status.success()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Runs `cmd` with `stdin`, killing it after `limit`.
pub fn run(cmd: &mut Command, stdin: Stdio, limit: Duration) -> Result<Output> {
    let mut child = cmd
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn {:?}", cmd.get_program()))?;
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let ro = thread::spawn(move || {
        let mut v = vec![];
        let _ = so.read_to_end(&mut v);
        v
    });
    let re = thread::spawn(move || {
        let mut v = vec![];
        let _ = se.read_to_end(&mut v);
        v
    });
    let t = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if t.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(
                anyhow::anyhow!("{:?} timed out after {limit:?}", cmd.get_program())
                    .context(crate::ErrorKind::Timeout),
            );
        }
        thread::sleep(Duration::from_millis(20));
    };
    let stdout = ro.join().unwrap_or_default();
    let stderr = String::from_utf8_lossy(&re.join().unwrap_or_default()).into_owned();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}
