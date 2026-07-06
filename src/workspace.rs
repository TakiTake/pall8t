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
    crate::container::sha256_hex_prefix(input.as_bytes(), 4)
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
    write_claude_md(workspace, entry)?;
    Ok(format!(
        "workspace ready: {cloned} repo(s) seeded, {skipped} already present"
    ))
}

/// Generate CLAUDE.md at the workspace root (agent tabs start here, outside
/// any repo, so per-repo .claude/ skills are not loaded). Only written if
/// missing — user edits are preserved.
fn write_claude_md(workspace: &Path, entry: &ProjectEntry) -> Result<()> {
    let path = workspace.join("CLAUDE.md");
    if path.exists() {
        return Ok(());
    }
    let repo_names: Vec<String> = entry
        .repos
        .iter()
        .filter_map(|r| r.file_name().map(|s| s.to_string_lossy().into_owned()))
        .collect();
    let repo_list = if repo_names.is_empty() {
        "(none seeded yet)".to_string()
    } else {
        repo_names.join(", ")
    };
    let content = format!(
        r#"# pall8t workspace: {name}

You are running inside a Linux VM (apple/container) launched by **pall8t**, an
agent multiplexer on macOS. This directory is the project workspace — a host
directory mounted at the identical absolute path inside the container.
Everything here **persists** across container restarts and is readable by the
human's IDE on the host at the same path.

## Environment facts

- Files you create are owned by the host user. `sudo` works, but grants root
  only inside this VM.
- You have no access to the host beyond this workspace and your `$HOME`
  (`/home/dev`, persistent — login state survives rebuilds).
- The `container` CLI does **not** exist here; you are inside the container.

## Layout

- `repos/` — seeded clones of the source repos ({repo_list}); treat them as
  worktree parents, keep their checkouts clean.
- `wt/` — one git worktree per task. **Do your work here.**

## Git workflow (one worktree per task)

1. `git -C repos/<repo> fetch origin`
2. `git -C repos/<repo> worktree add ../../wt/<task>-<repo> -b <task-branch> origin/main`
3. Work, commit, and push from `wt/<task>-<repo>` (`origin` is the real
   upstream; credentials live in your persistent home).

Tasks may span multiple repos: one worktree per repo, same branch name.

## Being a good tab citizen

- pall8t watches your screen: approval/input prompts notify the human, who can
  jump to your tab. Ask normally — no special protocol.
- If your tab is closed, this process ends but the workspace persists.
"#,
        name = entry.name,
        repo_list = repo_list
    );
    std::fs::write(&path, content)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}
