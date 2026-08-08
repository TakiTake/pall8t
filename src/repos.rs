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

/// Longest slug a [`path_key`] carries. apple/container 1.2.0 rejects any
/// container name longer than 63 characters (`ManagedContainer.nameValid`,
/// apple/container#1956); 1.0.0 checked the shape only, so a workspace with
/// a long basename ran there and fails here. [`crate::container::run_name`]
/// spends 7 characters on `pall8t-`, 9 on the `-` and the hash, and one
/// more `-` before the pid, so a 32-character slug leaves 14 digits of pid
/// headroom against the cap — far more than any OS hands out.
const SLUG_MAX: usize = 32;

/// Stable short key for a path: `<slug(basename)>-<sha256(path)[..8hex]>`,
/// the slug capped at [`SLUG_MAX`]. The hash keeps two paths sharing a
/// basename distinct — and carries that uniqueness by itself, so capping
/// the slug costs readability, never correctness. Shared by container
/// names, image tag bases, and reference-repo clone dirs so the derivation
/// can't drift between them.
pub(crate) fn path_key(path: &Path) -> String {
    let name = path.file_name().map_or_else(
        || "workspace".to_string(),
        |s| s.to_string_lossy().into_owned(),
    );
    format!(
        "{}-{}",
        capped_slug(&name),
        crate::container::sha256_hex_prefix(path.to_string_lossy().as_bytes(), 4)
    )
}

/// [`slug`], cut to [`SLUG_MAX`]. Counted in `char`s, the way
/// apple/container counts them (`name.count`, over Swift Characters) — the
/// slug is ASCII either way, so the two agree. A cut landing inside a run
/// of `-` would leave `--` in front of the hash, so the tail is re-trimmed;
/// `slug` trims both ends, so the slug starts with an alphanumeric and at
/// least that first character always survives.
fn capped_slug(name: &str) -> String {
    let s = slug(name);
    if s.chars().count() <= SLUG_MAX {
        return s;
    }
    s.chars()
        .take(SLUG_MAX)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
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
    ///
    /// `git_dir` is the main repository's common `.git` when the source is
    /// a *linked worktree* ([`crate::worktree::main_git_dir`]), and must be
    /// mounted alongside it: such a worktree's `.git` is a pointer file
    /// naming an absolute path outside the source, so mounting the source
    /// alone yields a directory git cannot read as a repository at all. The
    /// copy path never needs this — `git clone --local` resolves the
    /// pointer host-side and produces a standalone repo — which is why it
    /// only surfaced when read-only became the default.
    ReadOnly {
        source: PathBuf,
        git_dir: Option<PathBuf>,
    },
    /// A `git clone --local` copy (under `~/.pall8t/repos`) mounted at
    /// `source`'s path, so writes land in the copy and the real checkout is
    /// untouched — protection by duplication, which is what pall8t did for
    /// every repo before read-only mounts were verified. Still the way to
    /// let an agent commit or fetch in a reference repo.
    ///
    /// The copy outlives the run: [`prepare`] reuses an existing clone
    /// as-is, so an agent's commits are there again next time, and the way
    /// to start over from the source is to delete it under
    /// `~/.pall8t/repos`. Disposable in the sense that losing it costs
    /// nothing pall8t tracks — not in the sense of being cleaned up.
    Copy { source: PathBuf, clone: PathBuf },
}

impl RepoMount {
    /// The mounts this repo needs — usually one, two when a read-only
    /// source is a linked worktree and its main `.git` has to come along
    /// (see [`RepoMount::ReadOnly`]).
    pub fn mounts(&self) -> Vec<crate::container::Mount> {
        match self {
            RepoMount::ReadOnly { source, git_dir } => {
                let mut out = vec![crate::container::Mount::ro(source.clone(), source.clone())];
                if let Some(git_dir) = git_dir {
                    // Identity path, like the workspace's own worktree mount
                    // (FR-3): the pointer file's `gitdir:` is absolute, and
                    // the back-pointer inside it names the worktree by
                    // absolute path too, so both only resolve if the common
                    // `.git` appears at the very path it has on the host.
                    out.push(crate::container::Mount::ro(
                        git_dir.clone(),
                        git_dir.clone(),
                    ));
                }
                out
            }
            RepoMount::Copy { source, clone } => {
                vec![crate::container::Mount::rw(clone.clone(), source.clone())]
            }
        }
    }

    /// One line for the user saying which protection this repo got — the
    /// two modes differ in what an agent may do, so a run must not leave
    /// that ambiguous.
    pub fn describe(&self) -> String {
        match self {
            RepoMount::ReadOnly {
                source,
                git_dir: None,
            } => format!("reference repo {} (read-only)", source.display()),
            RepoMount::ReadOnly {
                source,
                git_dir: Some(git_dir),
            } => format!(
                "reference repo {} (read-only; linked worktree — also mounting {} read-only)",
                source.display(),
                git_dir.display()
            ),
            RepoMount::Copy { source, clone } => format!(
                "reference repo {} (writable copy — writes hit {}, not the original)",
                source.display(),
                clone.display()
            ),
        }
    }
}

/// `GIT_CONFIG_*` environment marking every read-only repo path as a git
/// `safe.directory`, or empty when there are none.
///
/// Necessary because a read-only virtiofs mount does not carry
/// apple/container's uid/gid remapping: a directory the host shows as
/// `501:20` appears inside the container as `0:0`, while the writable
/// workspace mounted beside it appears as `501:20` (measured on 1.2.2).
/// Contents stay readable — the mode bits are unchanged — but git compares
/// the repository's owner against its own euid and refuses everything with
/// "detected dubious ownership" until told otherwise. A reference repo the
/// agent cannot run `git log` in is not delivering the read access that is
/// the whole point of mounting it.
///
/// Scoped to the exact paths pall8t mounted, via env rather than a config
/// file: `safe.directory = *` in the image would switch the check off for
/// every repository the sandbox ever sees, including ones arriving later by
/// other means, to solve a problem only these mounts have.
pub fn safe_directory_env(readonly_paths: &[PathBuf]) -> Vec<(String, String)> {
    if readonly_paths.is_empty() {
        return Vec::new();
    }
    let mut env = vec![(
        "GIT_CONFIG_COUNT".to_string(),
        readonly_paths.len().to_string(),
    )];
    for (i, path) in readonly_paths.iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), "safe.directory".to_string()));
        env.push((
            format!("GIT_CONFIG_VALUE_{i}"),
            path.to_string_lossy().into_owned(),
        ));
    }
    env
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
/// agent's commits would land in the clone rather than the workspace;
/// mounted read-only, the live checkout the agent is supposed to be
/// working in would turn read-only underneath it.
pub fn prepare(
    entries: &[RepoEntry],
    protected: &[PathBuf],
    cli_readonly: Option<bool>,
) -> Result<Vec<RepoMount>> {
    prepare_in(entries, protected, cli_readonly, &clones_root()?)
}

/// [`prepare`] with the clone root as an argument, so the copy path can be
/// tested against a temp directory instead of the caller's real
/// `~/.pall8t/repos` (docs/testing.md: dependencies are arguments).
pub(crate) fn prepare_in(
    entries: &[RepoEntry],
    protected: &[PathBuf],
    cli_readonly: Option<bool>,
    root: &Path,
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
        if !source.join(".git").exists() {
            return Err(anyhow!("not a git repo: {}", source.display()));
        }
        let readonly = crate::config::repo_readonly(entry, cli_readonly);
        // A linked worktree drags its main `.git` in with it, and that is a
        // second mount — so it is resolved before the overlap check, which
        // has to see every path this entry would cover.
        let git_dir = if readonly {
            crate::worktree::main_git_dir(&source)
        } else {
            None
        };
        for path in std::iter::once(&source).chain(git_dir.iter()) {
            if let Some(p) = protected.iter().find(|p| overlaps(path, p)) {
                return Err(anyhow!(
                    "reference repo {} overlaps {} — mounting it would cover the live \
                     checkout this run works in, either swallowing the agent's commits \
                     into a clone or turning the checkout read-only; remove \
                     it from [[repos]]",
                    path.display(),
                    p.display()
                ));
            }
        }
        if readonly {
            mounts.push(RepoMount::ReadOnly { source, git_dir });
            continue;
        }
        std::fs::create_dir_all(root)?;
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
            RepoMount::ReadOnly { source, git_dir } => {
                assert_eq!(
                    *source, canonical,
                    "the source is mounted as it stands, canonicalized"
                );
                assert_eq!(*git_dir, None, "a normal repo keeps its .git inside itself");
            }
            RepoMount::Copy { .. } => {
                panic!("read-only is the default — a copy here means the default flipped")
            }
        }
        assert!(
            !clones_root().unwrap().join(path_key(&canonical)).exists(),
            "a read-only entry must not leave a clone behind: the disk cost and \
             the staleness that came with duplication are what ADR-0009 removes"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Both directions of `--repos-readonly` must actually decide the mode.
    /// A `Some(true)`-only test would pass even if `prepare` ignored a false
    /// override entirely, since an unset entry is read-only anyway
    /// (`CodeRabbit`, PR #46).
    #[test]
    fn prepare_cli_override_decides_both_ways() {
        let root = fake_repo("cli-override");
        let source = root.join("src");
        let clones = root.join("clones");
        std::fs::create_dir_all(&source).unwrap();
        git(&["init", "-q", &source.to_string_lossy()]).unwrap();
        std::fs::write(source.join("f.txt"), "x").unwrap();

        let asked_for_a_copy = RepoEntry {
            source: source.clone(),
            readonly: Some(false),
        };
        let asked_for_readonly = RepoEntry {
            source: source.clone(),
            readonly: Some(true),
        };

        assert!(
            matches!(
                prepare_in(
                    std::slice::from_ref(&asked_for_a_copy),
                    &[],
                    Some(true),
                    &clones
                )
                .unwrap()
                .as_slice(),
                [RepoMount::ReadOnly { .. }]
            ),
            "--repos-readonly overrides an entry that asked for a copy"
        );

        let copied = prepare_in(
            std::slice::from_ref(&asked_for_readonly),
            &[],
            Some(false),
            &clones,
        )
        .unwrap();
        match copied.as_slice() {
            [RepoMount::Copy { clone, .. }] => {
                assert!(
                    clone.starts_with(&clones) && clone.join(".git").exists(),
                    "--repos-readonly=false must produce a real clone under the \
                     clone root, not just a differently-shaped mount: {}",
                    clone.display()
                );
                assert!(
                    source.join("f.txt").exists(),
                    "…and must not disturb the source it copied from"
                );
            }
            other => panic!("--repos-readonly=false must reach the copy path, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A linked git worktree used as a reference repo needs its main `.git`
    /// mounted too: its own `.git` is a pointer file naming an absolute
    /// path outside the source, so mounting the source alone hands the
    /// sandbox a directory git cannot read as a repository. The copy path
    /// never had this problem — `git clone --local` resolves the pointer on
    /// the host — so it appeared only when read-only became the default
    /// (`CodeRabbit`, PR #46).
    #[test]
    fn prepare_readonly_linked_worktree_also_mounts_the_main_git_dir() {
        let root = fake_repo("linked-worktree");
        let main = root.join("main");
        let linked = root.join("linked");
        std::fs::create_dir_all(&main).unwrap();
        let main_s = main.to_string_lossy().into_owned();
        git(&["init", "-q", &main_s]).unwrap();
        std::fs::write(main.join("a.txt"), "a").unwrap();
        git(&["-C", &main_s, "add", "-A"]).unwrap();
        git(&[
            "-C",
            &main_s,
            "-c",
            "user.email=t@e",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ])
        .unwrap();
        git(&[
            "-C",
            &main_s,
            "worktree",
            "add",
            "-q",
            &linked.to_string_lossy(),
            "-b",
            "feat",
        ])
        .unwrap();

        let entry = RepoEntry {
            source: linked.clone(),
            readonly: None,
        };
        let prepared = prepare(std::slice::from_ref(&entry), &[], None).unwrap();

        let expected_git_dir = main.join(".git").canonicalize().unwrap();
        match prepared.as_slice() {
            [RepoMount::ReadOnly {
                git_dir: Some(dir), ..
            }] => assert_eq!(
                *dir, expected_git_dir,
                "the main repository's common .git is what the pointer file \
                 resolves through"
            ),
            other => panic!("a linked worktree must carry its git dir, got {other:?}"),
        }

        let mounts = prepared[0].mounts();
        assert_eq!(mounts.len(), 2, "source plus its git dir");
        assert!(
            mounts.iter().all(|m| m.readonly),
            "both go in read-only — mounting the main .git writable would hand \
             the sandbox the very checkout the read-only default protects"
        );
        assert!(
            mounts.iter().all(|m| m.host == m.dest),
            "both are identity mounts: the pointer file and its back-pointer \
             are absolute host paths and resolve nowhere else"
        );

        // The same pointer file is why the guard has to see the git dir: a
        // main repo that *is* the workspace would otherwise be mounted
        // read-only underneath the agent by way of a reference entry.
        let err = prepare(
            std::slice::from_ref(&entry),
            std::slice::from_ref(&expected_git_dir),
            None,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains(&expected_git_dir.display().to_string()),
            "the overlap check must cover the git dir, not just the source"
        );

        let _ = std::fs::remove_dir_all(&root);
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
            git_dir: None,
        }
        .mounts();
        assert_eq!(ro.len(), 1, "a normal repo needs one mount");
        let ro = &ro[0];
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
        .mounts();
        assert_eq!(copy.len(), 1, "a copy is one mount: the clone");
        let copy = copy.into_iter().next().unwrap();
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

    /// The env has to be exactly what git reads: a count, and contiguous
    /// `KEY_n`/`VALUE_n` pairs from zero. Git stops at the count, so an
    /// off-by-one silently drops the last exception and that repo goes back
    /// to failing with "dubious ownership".
    #[test]
    fn safe_directory_env_table() {
        assert!(
            safe_directory_env(&[]).is_empty(),
            "no read-only mounts, no git config to inject"
        );

        let env = safe_directory_env(&[PathBuf::from("/a"), PathBuf::from("/b/c")]);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map_or_else(|| panic!("{k} missing from {env:?}"), |(_, v)| v.clone())
        };
        assert_eq!(get("GIT_CONFIG_COUNT"), "2");
        assert_eq!(get("GIT_CONFIG_KEY_0"), "safe.directory");
        assert_eq!(get("GIT_CONFIG_VALUE_0"), "/a");
        assert_eq!(get("GIT_CONFIG_KEY_1"), "safe.directory");
        assert_eq!(get("GIT_CONFIG_VALUE_1"), "/b/c");
        assert_eq!(
            env.len(),
            5,
            "the count plus one key/value pair per path, and nothing else: {env:?}"
        );
    }

    #[test]
    fn repo_mount_describe_names_the_protection() {
        let ro = RepoMount::ReadOnly {
            source: "/Users/me/src/lib".into(),
            git_dir: None,
        }
        .describe();
        assert!(
            ro.contains("/Users/me/src/lib") && ro.contains("read-only"),
            "the line must say which repo and that it cannot be written: {ro}"
        );

        let linked = RepoMount::ReadOnly {
            source: "/Users/me/src/lib".into(),
            git_dir: Some("/Users/me/src/main/.git".into()),
        }
        .describe();
        assert!(
            linked.contains("/Users/me/src/main/.git"),
            "a linked worktree pulls in a second mount, and a run that mounts \
             something the user never listed must say so: {linked}"
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

    #[test]
    fn capped_slug_table() {
        let at_cap = "a".repeat(SLUG_MAX);
        assert_eq!(
            capped_slug(&at_cap),
            at_cap,
            "a slug exactly at the cap is untouched"
        );
        assert_eq!(
            capped_slug(&format!("{at_cap}b")),
            at_cap,
            "one character over is cut to the cap"
        );
        assert_eq!(
            capped_slug("short-name"),
            "short-name",
            "the common case must round-trip byte for byte — capping may not \
             churn existing image tags and clone dirs"
        );

        // The cut lands inside the run of dashes `slug` left behind for the
        // spaces, which would otherwise put `--` in front of the hash.
        let cut_on_a_dash = format!("{}   tail", "a".repeat(SLUG_MAX - 1));
        assert_eq!(
            capped_slug(&cut_on_a_dash),
            "a".repeat(SLUG_MAX - 1),
            "a cut landing in a run of separators must not leave a trailing dash"
        );
    }

    #[test]
    fn path_key_is_bounded_and_still_separates_paths() {
        let long = "a-very-long-workspace-directory-name-".repeat(6);
        let a = Path::new("/Users/me/one").join(&long);
        let b = Path::new("/Users/me/two").join(&long);

        for p in [&a, &b] {
            assert!(
                path_key(p).chars().count() <= SLUG_MAX + 1 + 8,
                "the key is the capped slug, a dash, and 8 hex characters: {}",
                path_key(p)
            );
        }
        assert_ne!(
            path_key(&a),
            path_key(&b),
            "two paths sharing a truncated basename must stay distinct — the \
             hash is what carries uniqueness once the slug is cut"
        );
    }
}
