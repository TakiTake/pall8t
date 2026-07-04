use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_CONTAINERFILE: &str = include_str!("../Containerfile");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Absent,
    Stopped,
    Running,
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Running => "● running",
            State::Stopped => "○ stopped",
            State::Absent => "· absent",
        }
    }
}

pub fn host_ids() -> (u32, u32) {
    (read_id("-u").unwrap_or(501), read_id("-g").unwrap_or(20))
}

fn read_id(flag: &str) -> Option<u32> {
    let out = Command::new("id").arg(flag).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

pub fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    s.trim_matches('-').to_string()
}

pub fn container_name(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    let dir = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    format!("pall8t-{}-{}", slug(&dir), hex)
}

pub fn image_tag(base: &str, uid: u32, gid: u32) -> String {
    format!("{base}:{uid}-{gid}")
}

fn run_ok<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();
    let out = Command::new("container")
        .args(&argv)
        .output()
        .with_context(|| format!("failed to run: container {}", argv.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`container {}` failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// True if the `container` CLI is on PATH.
pub fn cli_available() -> bool {
    Command::new("container")
        .arg("--version")
        .output()
        .is_ok()
}

/// True if the apple/container system service (apiserver) is running.
pub fn system_running() -> bool {
    Command::new("container")
        .args(["system", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reconcile source of truth: `container list --all --format json`.
/// Parsed defensively (schema is pre-1.0, see ADR-0001).
pub fn list_all() -> Result<Vec<(String, State)>> {
    let stdout = run_ok(["list", "--all", "--format", "json"])?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let v: Value = serde_json::from_str(trimmed).context("unexpected `container list` JSON")?;
    let mut items = Vec::new();
    if let Some(arr) = v.as_array() {
        for item in arr {
            let name = item
                .pointer("/configuration/id")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))
                .or_else(|| item.get("name").and_then(Value::as_str));
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(name) = name {
                let state = if status.eq_ignore_ascii_case("running") {
                    State::Running
                } else {
                    State::Stopped
                };
                items.push((name.to_string(), state));
            }
        }
    }
    Ok(items)
}

pub fn image_exists(tag: &str) -> bool {
    fn any_str_contains(v: &Value, needle: &str) -> bool {
        match v {
            Value::String(s) => s.contains(needle),
            Value::Array(a) => a.iter().any(|x| any_str_contains(x, needle)),
            Value::Object(m) => m.values().any(|x| any_str_contains(x, needle)),
            _ => false,
        }
    }
    run_ok(["image", "list", "--format", "json"])
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
        .map(|v| any_str_contains(&v, tag))
        .unwrap_or(false)
}

pub fn build_image(containerfile: &Path, ctx_dir: &Path, tag: &str, uid: u32, gid: u32) -> Result<()> {
    run_ok([
        "build".to_string(),
        "-f".to_string(),
        containerfile.to_string_lossy().into_owned(),
        "-t".to_string(),
        tag.to_string(),
        "--build-arg".to_string(),
        format!("UID={uid}"),
        "--build-arg".to_string(),
        format!("GID={gid}"),
        ctx_dir.to_string_lossy().into_owned(),
    ])?;
    Ok(())
}

pub struct RunSpec {
    pub name: String,
    pub project: PathBuf,
    pub image: String,
    pub cpus: u32,
    pub memory: String,
    pub uid: u32,
    pub gid: u32,
}

pub fn run_detached(spec: &RunSpec) -> Result<()> {
    let home = home_mount()?;
    run_ok([
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        spec.name.clone(),
        "-v".to_string(),
        format!("{}:/work", spec.project.display()),
        "-v".to_string(),
        format!("{}:/home/dev", home.display()),
        "-w".to_string(),
        "/work".to_string(),
        "--user".to_string(),
        "dev".to_string(),
        "--uid".to_string(),
        spec.uid.to_string(),
        "--gid".to_string(),
        spec.gid.to_string(),
        "--cpus".to_string(),
        spec.cpus.to_string(),
        "--memory".to_string(),
        spec.memory.clone(),
        spec.image.clone(),
        "sleep".to_string(),
        "infinity".to_string(),
    ])?;
    Ok(())
}

pub fn start(name: &str) -> Result<()> {
    run_ok(["start", name])?;
    Ok(())
}

pub fn stop(name: &str) -> Result<()> {
    run_ok(["stop", name])?;
    Ok(())
}

pub fn delete(name: &str) -> Result<()> {
    run_ok(["delete", name])?;
    Ok(())
}

pub fn logs(name: &str) -> Result<String> {
    run_ok(["logs", name])
}

/// Absolute path to the `container` CLI, resolved once. Spawned terminal
/// tabs may run with a minimal PATH (e.g. Ghostty uses
/// `bash --noprofile --norc`), so a bare `container` is not found there.
pub fn cli_path() -> &'static str {
    static CLI_PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CLI_PATH.get_or_init(|| {
        Command::new("which")
            .arg("container")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "container".to_string())
    })
}

/// The command every terminal tab runs. Raw `container exec`, so the tab
/// keeps working even if pall8t exits.
pub fn exec_shell_command(name: &str, claude: bool) -> String {
    let prog = if claude { "claude" } else { "bash -l" };
    format!("{} exec -it --user dev -w /work {name} {prog}", cli_path())
}

/// Persistent container-side $HOME (claude auth, shell history, dotfiles).
pub fn home_mount() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".pall8t")
        .join("home");
    std::fs::create_dir_all(&home)?;
    Ok(home)
}

/// Writes the embedded default Containerfile to ~/.pall8t/Containerfile.
pub fn default_containerfile_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".pall8t");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("Containerfile");
    std::fs::write(&path, DEFAULT_CONTAINERFILE)?;
    Ok(path)
}
