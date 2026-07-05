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
            State::Running => "●",
            State::Stopped => "○",
            State::Absent => "·",
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

/// pall8t-<slug(project)>-<sha256(workspace path)[..8]>
pub fn container_name(project_name: &str, workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(workspace.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("pall8t-{}-{}", crate::workspace::slug(project_name), hex)
}

pub fn image_tag(base: &str, uid: u32, gid: u32) -> String {
    format!("{base}:{uid}-{gid}")
}

/// Like [`image_tag`], but suffixed with a Containerfile content hash (see
/// [`containerfile_content_hash`]) so a change to its contents resolves to
/// a new tag.
pub fn image_tag_hashed(base: &str, uid: u32, gid: u32, hash: &str) -> String {
    format!("{base}:{uid}-{gid}-{hash}")
}

/// First 12 hex chars of the sha256 of `containerfile`'s current bytes.
/// Hashing the working-tree contents (rather than, say, the last commit
/// that touched the file) means uncommitted edits are detected too, and a
/// rebuild can never poison a tag: the same content always resolves to the
/// same tag, so the tag always corresponds to the image built from it.
/// `None` if the file can't be read.
pub fn containerfile_content_hash(containerfile: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(containerfile).ok()?;
    let digest = Sha256::digest(&bytes);
    Some(digest.iter().take(6).map(|b| format!("{b:02x}")).collect())
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

pub fn cli_available() -> bool {
    Command::new("container").arg("--version").output().is_ok()
}

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
    // Exact match, not `contains`: with hash-suffixed tags, the unsuffixed
    // form (e.g. `pall8t-x:501-20`) is a substring of the suffixed one
    // (`pall8t-x:501-20-abc123456789`), so a substring match would report
    // a tag as existing when only a differently-hashed sibling does.
    fn any_str_equals(v: &Value, needle: &str) -> bool {
        match v {
            Value::String(s) => s == needle,
            Value::Array(a) => a.iter().any(|x| any_str_equals(x, needle)),
            Value::Object(m) => m.values().any(|x| any_str_equals(x, needle)),
            _ => false,
        }
    }
    run_ok(["image", "list", "--format", "json"])
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
        .map(|v| any_str_equals(&v, tag))
        .unwrap_or(false)
}

/// Image reference strings from `container image list` that start with
/// `prefix`, matched the same defensive way as [`image_exists`] (schema is
/// pre-1.0, see ADR-0001). Used to find superseded project image builds to
/// prune after a successful rebuild.
pub fn image_tags_with_prefix(prefix: &str) -> Result<Vec<String>> {
    fn collect(v: &Value, prefix: &str, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                if s.starts_with(prefix) {
                    out.push(s.clone());
                }
            }
            Value::Array(a) => a.iter().for_each(|x| collect(x, prefix, out)),
            Value::Object(m) => m.values().for_each(|x| collect(x, prefix, out)),
            _ => {}
        }
    }
    let stdout = run_ok(["image", "list", "--format", "json"])?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let v: Value =
        serde_json::from_str(trimmed).context("unexpected `container image list` JSON")?;
    let mut out = Vec::new();
    collect(&v, prefix, &mut out);
    out.sort();
    out.dedup();
    Ok(out)
}

/// Delete an image by tag/reference.
pub fn image_delete(tag: &str) -> Result<()> {
    run_ok(["image", "delete", tag])?;
    Ok(())
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
    pub workspace: PathBuf,
    pub image: String,
    pub cpus: u32,
    pub memory: String,
    pub uid: u32,
    pub gid: u32,
}

/// Identity-path mount: the workspace is visible at the same absolute path
/// inside the container (ADR-0004).
pub fn run_detached(spec: &RunSpec) -> Result<()> {
    let home = home_mount()?;
    let ws = spec.workspace.to_string_lossy().into_owned();
    run_ok([
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        spec.name.clone(),
        "-v".to_string(),
        format!("{ws}:{ws}"),
        "-v".to_string(),
        format!("{}:/home/dev", home.display()),
        "-w".to_string(),
        ws.clone(),
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

/// Image reference a container was created from (via `container inspect`).
pub fn image_ref(name: &str) -> Option<String> {
    let out = run_ok(["inspect", name]).ok()?;
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    v.pointer("/0/configuration/image/reference")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// argv for a tab's PTY child: exec into the project container at the
/// workspace path. pall8t itself runs with a full PATH, so bare `container`
/// resolves here (unlike v1's external tabs).
pub fn exec_argv(name: &str, workspace: &Path, cmd: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "exec".into(),
        "-it".into(),
        "--user".into(),
        "dev".into(),
        "-w".into(),
        workspace.to_string_lossy().into_owned(),
        name.to_string(),
    ];
    argv.extend(cmd.iter().cloned());
    argv
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn containerfile_content_hash_is_stable_and_12_chars() {
        let dir = std::env::temp_dir().join(format!("pall8t-test-hash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Containerfile");
        fs::write(&file, "FROM scratch\n").unwrap();

        let first = containerfile_content_hash(&file).expect("hash");
        let second = containerfile_content_hash(&file).expect("hash");
        assert_eq!(first.len(), 12);
        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn containerfile_content_hash_changes_with_content() {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-hash-diff-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Containerfile");

        fs::write(&file, "FROM scratch\n").unwrap();
        let a = containerfile_content_hash(&file).unwrap();
        fs::write(&file, "FROM scratch\nRUN true\n").unwrap();
        let b = containerfile_content_hash(&file).unwrap();

        assert_ne!(a, b);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn containerfile_content_hash_none_when_missing() {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-hash-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("Containerfile");

        assert_eq!(containerfile_content_hash(&file), None);
    }
}
