//! Shared runtime state: which tabs exist, held by which pall8t-tab pids.
//! Every mutation of this file — and of config.toml — happens under the
//! same lock, so concurrent pall8t instances serialize instead of racing
//! (ADR-0005).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabEntry {
    pub id: String,
    pub project: String,
    pub kind: String,
    pub title: String,
    pub pid: u32,
    pub socket: PathBuf,
    pub container: String,
    pub workspace: PathBuf,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub tabs: Vec<TabEntry>,
}

fn base_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".pall8t");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn tabs_dir() -> Result<PathBuf> {
    let dir = base_dir()?.join("tabs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn state_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("state.json"))
}

fn lock_path() -> Result<PathBuf> {
    Ok(base_dir()?.join("state.lock"))
}

/// Advisory lock via exclusive lockfile creation. Stale locks (a crashed
/// holder-spawner) are stolen after 5s.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    pub fn acquire() -> Result<Self> {
        let path = lock_path()?;
        for _ in 0..250 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(_) => {
                    let stale = std::fs::metadata(&path)
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|m| m.elapsed().ok())
                        .map(|age| age > Duration::from_secs(5))
                        .unwrap_or(false);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        Err(anyhow!("could not acquire pall8t state lock"))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn read() -> Registry {
    state_path()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write(reg: &Registry) -> Result<()> {
    let path = state_path()?;
    let text = serde_json::to_string_pretty(reg)?;
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Lock → re-read → mutate → write. The only way state.json changes.
pub fn locked<R>(f: impl FnOnce(&mut Registry) -> R) -> Result<R> {
    let _lock = Lock::acquire()?;
    let mut reg = read();
    let out = f(&mut reg);
    write(&reg)?;
    Ok(out)
}

pub fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
