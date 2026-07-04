use crate::config::ProjectEntry;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

pub fn slug(name: &str) -> String {
    let s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let trimmed = s.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed
    }
}

fn hash8(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// <workspace_root>/<slug(name)>-<sha256(name)[..8]>
pub fn workspace_path(root: &Path, project_name: &str) -> PathBuf {
    expand_tilde(root).join(format!("{}-{}", slug(project_name), hash8(project_name)))
}

fn git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run: git {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Create the workspace layout and seed a clone of each source repo
/// (host-side; same-filesystem clones hardlink objects). See ADR-0004.
/// Returns a short summary. Idempotent: existing clones are left alone.
pub fn seed(workspace: &Path, entry: &ProjectEntry) -> Result<String> {
    std::fs::create_dir_all(workspace.join("repos"))?;
    std::fs::create_dir_all(workspace.join("wt"))?;
    let mut cloned = 0usize;
    let mut skipped = 0usize;
    for repo in &entry.repos {
        let repo = expand_tilde(repo);
        let name = repo
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or_else(|| anyhow!("bad repo path: {}", repo.display()))?;
        let target = workspace.join("repos").join(&name);
        if target.exists() {
            skipped += 1;
            continue;
        }
        if !repo.join(".git").exists() {
            return Err(anyhow!("not a git repo: {}", repo.display()));
        }
        let repo_s = repo.to_string_lossy().into_owned();
        let target_s = target.to_string_lossy().into_owned();
        git(&["clone", &repo_s, &target_s])?;
        // Point origin at the real upstream (if the source repo has one),
        // so fetch/push work from inside the container.
        if let Ok(url) = git(&["-C", &repo_s, "remote", "get-url", "origin"]) {
            let url = url.trim().to_string();
            if !url.is_empty() {
                git(&["-C", &target_s, "remote", "set-url", "origin", &url])?;
            }
        }
        cloned += 1;
    }
    Ok(format!(
        "workspace ready: {cloned} repo(s) seeded, {skipped} already present"
    ))
}
