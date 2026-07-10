use crate::config::RepoEntry;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

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
        "workspace".to_string()
    } else {
        trimmed
    }
}

/// Stable short key for a path: `<slug(basename)>-<sha256(path)[..8hex]>`.
/// The hash keeps two paths sharing a basename distinct; the slug keeps
/// the key readable. Shared by container names, image tag bases, and
/// reference-repo clone dirs so the derivation can't drift between them.
pub(crate) fn path_key(path: &Path) -> String {
    let name = path.file_name().map_or_else(
        || "workspace".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    format!(
        "{}-{}",
        slug(&name),
        crate::container::sha256_hex_prefix(path.to_string_lossy().as_bytes(), 4)
    )
}

fn git(args: &[&str]) -> Result<String> {
    let argv: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
    crate::util::run_ok("git", &argv)
}

/// A prepared reference repository: `clone` (under `~/.pall8t/repos`) is
/// mounted at `source`'s own absolute path inside the container, so
/// anything referencing the original path works while writes hit the
/// disposable copy — protection by duplication, compensating for
/// apple/container's missing read-only mounts (FR-4, apple/container#990).
pub struct RepoMount {
    pub source: PathBuf,
    pub clone: PathBuf,
}

/// Root under which reference-repo clones live.
fn clones_root() -> Result<PathBuf> {
    Ok(crate::config::pall8t_root()?.join("repos"))
}

/// True if one path is the other or an ancestor of it — i.e. a mount at
/// `a` would shadow `b`, or vice versa. Component-wise (`Path::starts_with`),
/// so `/a/bc` does not overlap `/a/b`.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Duplicates each configured reference repo via `git clone --local`
/// (same-filesystem clones hardlink objects; `cp -al` was rejected — see
/// FR-4) and returns the mounts. Idempotent: an existing clone is reused
/// as-is; delete it under `~/.pall8t/repos` to re-clone from the current
/// source state.
///
/// `protected` are the live identity-mounted paths of this run (the
/// workspace cwd and, for a worktree, the main repository's `.git`). A
/// source overlapping one of them is an error, checked before anything is
/// cloned: its clone would be mounted over the live checkout, so the
/// agent's commits would land in the disposable copy and be thrown away.
pub fn prepare(entries: &[RepoEntry], protected: &[PathBuf]) -> Result<Vec<RepoMount>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let root = clones_root()?;
    std::fs::create_dir_all(&root)?;
    let mut mounts = Vec::new();
    for entry in entries {
        let source = expand_tilde(&entry.source);
        let source = source
            .canonicalize()
            .with_context(|| format!("reference repo not found: {}", source.display()))?;
        if let Some(p) = protected.iter().find(|p| overlaps(&source, p)) {
            return Err(anyhow!(
                "reference repo {} overlaps {} — its disposable clone would be \
                 mounted over the live checkout, silently swallowing the agent's \
                 writes; remove it from [[repos]]",
                source.display(),
                p.display()
            ));
        }
        if !source.join(".git").exists() {
            return Err(anyhow!("not a git repo: {}", source.display()));
        }
        // Keyed by the source path (see [`path_key`]), so distinct sources
        // sharing a basename get distinct clones and the mapping is stable
        // across runs.
        let clone = root.join(path_key(&source));
        if !clone.exists() {
            // Clone into a temp dir and rename into place only once fully
            // configured: a failure/kill mid-setup must not leave a clone
            // whose origin is still the source's host path — inside the
            // container that path is the clone's own mount point, so
            // `git fetch` would silently fetch from itself.
            let tmp = clone.with_extension("partial");
            if tmp.exists() {
                std::fs::remove_dir_all(&tmp).with_context(|| {
                    format!("cannot remove stale partial clone {}", tmp.display())
                })?;
            }
            let source_s = source.to_string_lossy().into_owned();
            let tmp_s = tmp.to_string_lossy().into_owned();
            git(&["clone", "--local", &source_s, &tmp_s])?;
            // Point origin at the real upstream so fetch works from inside
            // the container; with no upstream, drop origin entirely rather
            // than leave it aimed at the mount point.
            match git(&["-C", &source_s, "remote", "get-url", "origin"]) {
                Ok(url) if !url.trim().is_empty() => {
                    git(&["-C", &tmp_s, "remote", "set-url", "origin", url.trim()])?;
                }
                _ => {
                    git(&["-C", &tmp_s, "remote", "remove", "origin"])?;
                }
            }
            std::fs::rename(&tmp, &clone).with_context(|| {
                format!("cannot move the prepared clone into {}", clone.display())
            })?;
        }
        mounts.push(RepoMount { source, clone });
    }
    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlaps_table() {
        let cwd = Path::new("/Users/me/src/proj");
        assert!(overlaps(cwd, cwd), "the workspace itself");
        assert!(
            overlaps(Path::new("/Users/me/src"), cwd),
            "an ancestor of the workspace"
        );
        assert!(
            overlaps(Path::new("/Users/me/src/proj/vendor/lib"), cwd),
            "a path inside the workspace"
        );
        assert!(
            !overlaps(Path::new("/Users/me/src/proj-lib"), cwd),
            "a sibling sharing a name prefix is component-wise distinct"
        );
        assert!(!overlaps(Path::new("/Users/me/src/other"), cwd));
    }

    #[test]
    fn slug_table() {
        assert_eq!(slug("My Repo"), "my-repo");
        assert_eq!(slug("--x--"), "x");
        assert_eq!(slug(""), "workspace");
        assert_eq!(slug("日本語"), "workspace");
    }
}
