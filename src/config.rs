use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_image")]
    pub default_image: String,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_image: default_image(),
            cpus: default_cpus(),
            memory: default_memory(),
            projects: Vec::new(),
        }
    }
}

fn default_image() -> String {
    "pall8t-base".to_string()
}
fn default_cpus() -> u32 {
    4
}
fn default_memory() -> String {
    "4G".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    pub path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containerfile: Option<PathBuf>,
}

pub fn config_path() -> Result<PathBuf> {
    // Deliberately ~/.config (not dirs::config_dir(), which is
    // ~/Library/Application Support on macOS) — see DESIGN.md §7.
    let base = dirs::home_dir()
        .map(|h| h.join(".config"))
        .context("cannot determine home directory")?;
    Ok(base.join("pall8t").join("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))?;
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = toml::to_string_pretty(cfg).context("cannot serialize config")?;
    std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}
