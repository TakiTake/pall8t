use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_image")]
    pub default_image: String,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_workspace_root")]
    pub workspace_root: PathBuf,
    #[serde(default = "default_prefix")]
    pub prefix: String,
    #[serde(default = "default_notify")]
    pub notify: String,
    #[serde(default = "default_agent_command")]
    pub agent_command: String,
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
    #[serde(default)]
    pub agents: HashMap<String, AgentPatternsConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_image: default_image(),
            cpus: default_cpus(),
            memory: default_memory(),
            workspace_root: default_workspace_root(),
            prefix: default_prefix(),
            notify: default_notify(),
            agent_command: default_agent_command(),
            projects: Vec::new(),
            agents: HashMap::new(),
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
fn default_workspace_root() -> PathBuf {
    PathBuf::from("~/.pall8t/workspaces")
}
fn default_prefix() -> String {
    "ctrl+b".to_string()
}
fn default_notify() -> String {
    "bell".to_string()
}
fn default_agent_command() -> String {
    "claude".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    #[serde(default)]
    pub repos: Vec<PathBuf>,
    /// Legacy v1 field; migrated into `repos` by `load()`.
    #[serde(default, skip_serializing)]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containerfile: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentPatternsConfig {
    #[serde(default)]
    pub waiting_patterns: Vec<String>,
    #[serde(default)]
    pub working_patterns: Vec<String>,
}

pub fn config_path() -> Result<PathBuf> {
    // Deliberately ~/.config (not dirs::config_dir(), which is
    // ~/Library/Application Support on macOS) — see DESIGN.md §8.
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
    let mut cfg: Config =
        toml::from_str(&text).with_context(|| format!("invalid TOML in {}", path.display()))?;
    // v1 → v2 migration: `path = "..."` becomes `repos = ["..."]`.
    for entry in &mut cfg.projects {
        if entry.repos.is_empty() {
            if let Some(p) = entry.path.take() {
                entry.repos.push(p);
            }
        }
    }
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

/// Parse a prefix spec like "ctrl+b" into (char). Only ctrl+<char> is supported.
pub fn parse_prefix(spec: &str) -> char {
    spec.trim()
        .strip_prefix("ctrl+")
        .and_then(|s| s.chars().next())
        .unwrap_or('b')
}
