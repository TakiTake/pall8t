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

/// A prepared reference repository. Either way it appears inside the
/// container at `source`'s own absolute path, so anything referencing the
/// original path keeps working (FR-4); the variants differ in how the real
/// checkout is protected (ADR-0009).
#[derive(Debug)]
pub enum RepoMount {
    /// The source itself, mounted read-only. The guest kernel refuses the
    /// write, so the agent cannot touch the real checkout — protection by
    /// the runtime, the default since ADR-0009.
    ReadOnly { source: PathBuf },
    /// A disposable `git clone --local` copy (under `~/.pall8t/repos`)
    /// mounted at `source`'s path, so writes land in the copy and the real
    /// checkout is untouched — protection by duplication, which is what
    /// pall8t did for every repo before read-only mounts were verified.
    /// Still the way to let an agent commit or fetch in a reference repo.
    Copy { source: PathBuf, clone: PathBuf },
}

impl RepoMount {
    /// Host path to mount and whether it goes in read-only.
    pub fn mount(&self) -> crate::container::Mount {
        match self {
            RepoMount::ReadOnly { source } => {
                crate::container::Mount::ro(source.clone(), source.clone())
            }
            RepoMount::Copy { source, clone } => {
                crate::container::Mount::rw(clone.clone(), source.clone())
            }
        }
    }

    /// One line for the user saying which protection this repo got — the
    /// two modes differ in what an agent may do, so a run must not leave
    /// that ambiguous.
    pub fn describe(&self) -> String {
        match self {
            RepoMount::ReadOnly { source } => {
                format!("reference repo {} (read-only)", source.display())
            }
            RepoMount::Copy { source, clone } => format!(
                "reference repo {} (writable copy — writes hit {}, not the original)",
                source.display(),
                clone.display()
            ),
        }
    }
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

/// Prepares each configured reference repo for mounting (FR-4).
///
/// A read-only entry needs nothing prepared — the source is mounted as it
/// stands and the runtime refuses the writes (ADR-0009). A writable one is
/// duplicated via `git clone --local` (same-filesystem clones hardlink
/// objects; `cp -al` was rejected — see FR-4), idempotently: an existing
/// clone is reused as-is, so delete it under `~/.pall8t/repos` to re-clone
/// from the current source state. `~/.pall8t/repos` itself is only created
/// when some entry actually needs a copy.
///
/// `cli_readonly` is `--repos-readonly` from the command line, applied to
/// every entry over its own setting (see [`crate::config::repo_readonly`]).
///
/// `protected` are the live identity-mounted paths of this run (the
/// workspace cwd and, for a worktree, the main repository's `.git`). A
/// source overlapping one of them is an error, checked before anything is
/// cloned, and it is an error under either mode: mounted as a copy, the
/// agent's commits would land in the disposable clone and be thrown away;
/// mounted read-only, the live checkout the agent is supposed to be
/// working in would turn read-only underneath it.
pub fn prepare(
    entries: &[RepoEntry],
    protected: &[PathBuf],
    cli_readonly: Option<bool>,
) -> Result<Vec<RepoMount>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let mut mounts = Vec::new();
    for entry in entries {
        let source = expand_tilde(&entry.source);
        let source = source
            .canonicalize()
            .with_context(|| format!("reference repo not found: {}", source.display()))?;
        if let Some(p) = protected.iter().find(|p| overlaps(&source, p)) {
            return Err(anyhow!(
                "reference repo {} overlaps {} — mounting it would cover the live \
                 checkout this run works in, either swallowing the agent's commits \
                 into a disposable clone or turning the checkout read-only; remove \
                 it from [[repos]]",
                source.display(),
                p.display()
            ));
        }
        if !source.join(".git").exists() {
            return Err(anyhow!("not a git repo: {}", source.display()));
        }
        if crate::config::repo_readonly(entry, cli_readonly) {
            mounts.push(RepoMount::ReadOnly { source });
            continue;
        }
        let root = clones_root()?;
        std::fs::create_dir_all(&root)?;
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
        mounts.push(RepoMount::Copy { source, clone });
    }
    Ok(mounts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that looks like a git repo to [`prepare`], which checks
    /// for `.git` and nothing more. Keyed by test name + pid so parallel
    /// tests don't share one.
    fn fake_repo(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-repos-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        dir
    }

    /// The read-only path prepares nothing: no clone, no `~/.pall8t/repos`
    /// entry, just the source itself. Pins ADR-0009's cost claim — a
    /// read-only entry costs no disk and cannot go stale — which a
    /// `prepare` that quietly kept cloning would break while every mount
    /// still looked right.
    #[test]
    fn prepare_readonly_entry_clones_nothing() {
        let dir = fake_repo("ro-no-clone");
        let entry = RepoEntry {
            source: dir.clone(),
            readonly: None,
        };

        let mounts = prepare(std::slice::from_ref(&entry), &[], None).unwrap();
        let canonical = dir.canonicalize().unwrap();

        assert_eq!(mounts.len(), 1);
        match &mounts[0] {
            RepoMount::ReadOnly { source } => assert_eq!(
                *source, canonical,
                "the source is mounted as it stands, canonicalized"
            ),
            RepoMount::Copy { .. } => {
                panic!("read-only is the default — a copy here means the default flipped")
            }
        }
        assert!(
            !clones_root().unwrap().join(path_key(&canonical)).exists(),
            "a read-only entry must not leave a clone behind: the disk cost and \
             the staleness that came with duplication are what ADR-0009 removes"
        );

        // An explicit `--repos-readonly=false` still reaches the copy path,
        // which is IO-heavy (`git clone --local`) and covered end to end
        // rather than here; what matters at this seam is that the flag is
        // what decides, not the entry alone.
        assert!(
            matches!(
                prepare(&[entry], &[], Some(true)).unwrap().as_slice(),
                [RepoMount::ReadOnly { .. }]
            ),
            "--repos-readonly keeps the read-only path"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The overlap guard fires under read-only too. Before ADR-0009 the
    /// only harm was a swallowed commit; now the same configuration would
    /// also turn the live checkout read-only under the agent, so the check must not
    /// have become mode-specific.
    #[test]
    fn prepare_rejects_a_repo_overlapping_the_workspace() {
        let dir = fake_repo("overlap");
        let canonical = dir.canonicalize().unwrap();
        let entry = RepoEntry {
            source: dir.clone(),
            readonly: Some(true),
        };

        let err = prepare(&[entry], std::slice::from_ref(&canonical), None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&canonical.display().to_string()),
            "the error must name the offending repo: {msg}"
        );
        assert!(
            msg.contains("read-only") || msg.contains("live checkout"),
            "…and say what it would do to the workspace, in either mode: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn repo_mount_maps_each_mode_to_its_mount() {
        let source = PathBuf::from("/Users/me/src/lib");

        let ro = RepoMount::ReadOnly {
            source: source.clone(),
        }
        .mount();
        assert_eq!(ro.host, source);
        assert_eq!(
            ro.dest, source,
            "a read-only repo is mounted at its own path — the point of the \
             identity path is that references to it keep resolving"
        );
        assert!(ro.readonly, "…and read-only, or it protects nothing");

        let clone = PathBuf::from("/Users/me/.pall8t/repos/lib-abc12345");
        let copy = RepoMount::Copy {
            source: source.clone(),
            clone: clone.clone(),
        }
        .mount();
        assert_eq!(
            (copy.host, copy.dest),
            (clone, source),
            "the copy is what gets mounted, at the source's path — reversing \
             these would mount the real checkout writable, which is the exact \
             thing duplication exists to avoid"
        );
        assert!(
            !copy.readonly,
            "the copy is writable: letting the agent commit and fetch is the \
             only reason to prefer it over a read-only mount"
        );
    }

    #[test]
    fn repo_mount_describe_names_the_protection() {
        let ro = RepoMount::ReadOnly {
            source: "/Users/me/src/lib".into(),
        }
        .describe();
        assert!(
            ro.contains("/Users/me/src/lib") && ro.contains("read-only"),
            "the line must say which repo and that it cannot be written: {ro}"
        );

        let copy = RepoMount::Copy {
            source: "/Users/me/src/lib".into(),
            clone: "/Users/me/.pall8t/repos/lib-abc12345".into(),
        }
        .describe();
        assert!(
            copy.contains("/Users/me/.pall8t/repos/lib-abc12345"),
            "a writable copy must name the copy, so the user knows where the \
             agent's commits actually went: {copy}"
        );
    }

    #[test]
    fn slug_table() {
        assert_eq!(slug("My Repo"), "my-repo");
        assert_eq!(slug("--x--"), "x");
        assert_eq!(slug(""), "workspace");
        assert_eq!(slug("日本語"), "workspace");
    }
}
