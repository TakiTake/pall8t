//! Resolving `[[mounts]]` config entries into the bind mounts a run
//! actually gets.
//!
//! A mount is literal: the directory named by `source` appears inside the
//! container at `target` (its own path by default), writable unless
//! `readonly = true`. There is no copying, cloning, or other indirection —
//! what you mount is what the agent sees, which is the whole reason this
//! replaced `[[repos]]` (ADR-0009).

use crate::config::MountEntry;
use crate::container::Mount;
use crate::util::expand_tilde;
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// True if one path is the other or an ancestor of it — i.e. a mount at
/// `a` would shadow `b`, or vice versa. Component-wise (`Path::starts_with`),
/// so `/a/bc` does not overlap `/a/b`.
fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// What an existing mount does to a path pall8t wants to add.
#[derive(Debug, PartialEq, Eq)]
enum Coverage {
    /// The path is already reachable, and reading it there yields the same
    /// host directory — so adding another mount would be redundant.
    SamePath,
    /// Something else is mounted over that path. Named by its target, for
    /// the error.
    Conflict(PathBuf),
}

/// Whether `path` is already covered inside the container by an existing
/// mount or a protected path, and if so whether the content behind it is
/// the same directory.
///
/// "Same" is decided by walking the covering mount's own mapping: a mount
/// of `/a` at `/a` reaches `/a/b/.git` as exactly that host path, while a
/// mount of `/elsewhere` at `/a` reaches something entirely different by
/// the same container path.
fn covering_mount(mounts: &[Mount], protected: &[PathBuf], path: &Path) -> Option<Coverage> {
    if let Some(p) = protected.iter().find(|p| path.starts_with(p)) {
        // The workspace and the worktree git dir are identity-mounted, and
        // the container home is pall8t's own — a path under any of them is
        // already there, as itself.
        return Some(if path == p || p == Path::new("/home/dev") {
            Coverage::Conflict(p.clone())
        } else {
            Coverage::SamePath
        });
    }
    let m = mounts.iter().find(|m| path.starts_with(&m.dest))?;
    let reached = path
        .strip_prefix(&m.dest)
        .map_or_else(|_| m.host.clone(), |rest| m.host.join(rest));
    Some(if reached == path {
        Coverage::SamePath
    } else {
        Coverage::Conflict(m.dest.clone())
    })
}

/// Resolves configured entries into mounts, in order.
///
/// `protected` are the *container-side* paths this run has already
/// committed to — the workspace, a worktree's main `.git`, and the
/// container home. A target overlapping one of them is an error, checked
/// before anything is mounted: covering the workspace would hide the code
/// the agent was started to work on, and covering `/home/dev` would take
/// out the agent's own config and session history. Targets are also
/// checked against each other, since the second of two overlapping mounts
/// silently wins.
///
/// `cli_readonly` is `--readonly` from the command line, applied to every
/// entry over its own setting (see [`crate::config::mount_readonly`]).
pub fn resolve(
    entries: &[MountEntry],
    protected: &[PathBuf],
    cli_readonly: Option<bool>,
) -> Result<Vec<Mount>> {
    let mut mounts: Vec<Mount> = Vec::new();
    for entry in entries {
        let source = expand_tilde(&entry.source);
        let source = source
            .canonicalize()
            .with_context(|| format!("mount source not found: {}", source.display()))?;
        // `canonicalize` is happy with a regular file, and apple/container
        // only objects once the container is starting ("path ... is not a
        // directory") — after a build, with a mount line already printed
        // claiming it worked. Say it here instead.
        if !source.is_dir() {
            return Err(anyhow!(
                "mount source must be a directory: {} — [[mounts]] mounts \
                 directories, not individual files",
                source.display()
            ));
        }
        let target = match &entry.target {
            // An identity mount is the default because it is what keeps
            // absolute paths meaningful on both sides: git metadata, build
            // outputs, and anything the agent reports back all resolve the
            // same way on the host.
            None => source.clone(),
            // Deliberately *not* tilde-expanded: `target` is a path inside
            // the container, and `~` there is the container's home, not the
            // host's. Expanding it would quietly turn `~/notes` into
            // `/Users/you/notes` — a host path, on the wrong side of the
            // boundary, differing per machine. Rejecting it as non-absolute
            // says so instead.
            Some(t) if !t.is_absolute() => {
                return Err(anyhow!(
                    "mount target must be an absolute container path: {} (for source {}). \
                     `~` is not expanded in a target — it would mean the host's home, \
                     not the container's; write the container path you want, e.g. \
                     /home/dev/notes",
                    t.display(),
                    source.display()
                ));
            }
            Some(t) => t.clone(),
        };
        let readonly = crate::config::mount_readonly(entry, cli_readonly);

        for p in protected {
            if overlaps(&target, p) {
                return Err(anyhow!(
                    "mount target {} overlaps {}, which this run already mounts — \
                     it would cover the workspace, the worktree's git directory, or \
                     the container home; give the mount a different `target`",
                    target.display(),
                    p.display()
                ));
            }
        }
        if let Some(clash) = mounts.iter().find(|m| overlaps(&target, &m.dest)) {
            return Err(anyhow!(
                "mount targets {} and {} overlap — the later mount would silently \
                 hide the earlier one",
                clash.dest.display(),
                target.display()
            ));
        }

        // A linked git worktree keeps its real git directory outside the
        // worktree: `.git` is a pointer file naming an absolute path. Mount
        // that too, or the agent gets a directory git cannot read as a
        // repository at all — the same rule FR-3 applies to the workspace.
        // Only for an identity mount: the pointer file and its back-pointer
        // are absolute, so they resolve only if the worktree is where it
        // says it is.
        let git_dir = if target == source {
            crate::worktree::main_git_dir(&source)
        } else {
            None
        };

        mounts.push(Mount {
            host: source.clone(),
            dest: target,
            readonly,
        });
        if let Some(git_dir) = git_dir {
            // This mount is pall8t's idea, not the user's, so it defers to
            // anything already covering that path rather than stacking a
            // second mount inside it. Nesting is not harmless: an earlier
            // writable mount of the main checkout plus a later read-only
            // worktree would land a read-only `.git` inside the writable
            // checkout, and commits there would start failing for a reason
            // nothing in the config hints at.
            match covering_mount(&mounts, protected, &git_dir) {
                // Already reachable, with the same content behind it.
                Some(Coverage::SamePath) => {}
                Some(Coverage::Conflict(dest)) => {
                    return Err(anyhow!(
                        "{} is a linked git worktree, so its main git directory {} has to \
                         be mounted for git to work — but {} is already mounted over that \
                         path with different contents; mount the main checkout at its own \
                         path, or drop the worktree entry",
                        source.display(),
                        git_dir.display(),
                        dest.display()
                    ));
                }
                // Inherits `readonly`: a writable worktree needs to write
                // refs and the index into the main `.git`, so mounting it
                // read-only would break exactly the commits the writable
                // mount is for.
                None => mounts.push(Mount::new(git_dir.clone(), git_dir, readonly)),
            }
        }
    }
    Ok(mounts)
}

/// One line per mount for the user, naming what the agent may do with it.
/// A run must never leave the difference implicit — read-only and writable
/// are the whole point of the setting.
pub fn describe(mount: &Mount) -> String {
    let mode = if mount.readonly {
        "read-only"
    } else {
        "writable"
    };
    if mount.host == mount.dest {
        format!("mount {} ({mode})", mount.host.display())
    } else {
        format!(
            "mount {} → {} ({mode})",
            mount.host.display(),
            mount.dest.display()
        )
    }
}

/// The warning for a `--readonly` that cannot do anything, or `None` when
/// the flag has entries to act on (or was not given).
///
/// The flag governs `[[mounts]]` entries and nothing else, so with none
/// configured it is a no-op — and a silent one, since a run with no mounts
/// prints no mount lines either. Someone reaching for it is usually
/// reaching for "make this sandbox read-only", which is not what it does
/// and not something pall8t offers: the workspace and the container home
/// are always writable, or the agent could not work and could not record
/// its own session.
pub fn no_mounts_warning(cli_readonly: Option<bool>, mount_count: usize) -> Option<String> {
    if cli_readonly.is_none() || mount_count > 0 {
        return None;
    }
    Some(
        "pall8t: warning: --readonly has no effect here — no [[mounts]] are \
         configured, so there is nothing for it to mount. It governs [[mounts]] \
         entries only; the workspace and the container home are always writable."
            .to_string(),
    )
}

/// `GIT_CONFIG_*` environment marking every read-only path as a git
/// `safe.directory`, or empty when there are none.
///
/// Necessary because a read-only virtiofs mount does not carry
/// apple/container's uid/gid remapping: a directory the host shows as
/// `501:20` appears inside the container as `0:0`, while the writable
/// workspace mounted beside it appears as `501:20` (measured on 1.2.2).
/// Contents stay readable — the mode bits are unchanged — but git compares
/// the repository's owner against its own euid and refuses everything with
/// "detected dubious ownership". A repository the agent cannot run
/// `git log` in is not delivering the read access it was mounted for.
///
/// Applied to every read-only mount, not only the ones that look like
/// repositories: a directory that merely *contains* checkouts hits the
/// same wall, and a `safe.directory` entry naming a path with no repo in
/// it does nothing. Scoped to the exact paths pall8t mounted, via env
/// rather than a config file, because `safe.directory = *` in the image
/// would disable the check for every repository the sandbox ever sees.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory that exists, for `resolve` to canonicalize. Keyed by
    /// test name + pid so parallel tests don't share one.
    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-mounts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(source: &Path) -> MountEntry {
        MountEntry {
            source: source.to_path_buf(),
            target: None,
            readonly: None,
        }
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

    /// Any directory mounts — the `.git` requirement `[[repos]]` carried
    /// was a consequence of cloning, and cloning is gone (ADR-0009).
    #[test]
    fn resolve_mounts_a_plain_directory_writable_by_default() {
        let dir = tmp_dir("plain");
        let canonical = dir.canonicalize().unwrap();

        let mounts = resolve(&[entry(&dir)], &[], None).unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            (&mounts[0].host, &mounts[0].dest),
            (&canonical, &canonical),
            "the default target is the source's own path, so absolute paths \
             mean the same thing on both sides"
        );
        assert!(
            !mounts[0].readonly,
            "writable is the default: a mount that silently refused writes would \
             surprise anyone who did not ask for read-only"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_honors_target_and_readonly() {
        let dir = tmp_dir("target-ro");
        let canonical = dir.canonicalize().unwrap();
        let e = MountEntry {
            source: dir.clone(),
            target: Some("/notes".into()),
            readonly: Some(true),
        };

        let mounts = resolve(&[e], &[], None).unwrap();

        assert_eq!(mounts[0].host, canonical);
        assert_eq!(
            mounts[0].dest,
            PathBuf::from("/notes"),
            "an explicit target is where it lands inside the container"
        );
        assert!(mounts[0].readonly);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The flag beats the entry in both directions. Asserting only the
    /// `true` case would pass even if a false override were ignored,
    /// whenever the entry did not ask for read-only anyway.
    #[test]
    fn resolve_cli_override_decides_both_ways() {
        let dir = tmp_dir("override");
        let mut ro = entry(&dir);
        ro.readonly = Some(true);
        let mut rw = entry(&dir);
        rw.readonly = Some(false);

        assert!(
            !resolve(std::slice::from_ref(&ro), &[], Some(false)).unwrap()[0].readonly,
            "--readonly=false overrides an entry that asked for read-only"
        );
        assert!(
            resolve(std::slice::from_ref(&rw), &[], Some(true)).unwrap()[0].readonly,
            "--readonly overrides an entry that asked for writable"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The guard exists so a mount cannot cover what the run is built on.
    /// `/home/dev` is in `protected` for a concrete reason: covering it
    /// takes out the agent's config and its session history.
    #[test]
    fn resolve_rejects_a_target_covering_a_protected_path() {
        let dir = tmp_dir("protected");

        for protected in ["/home/dev", "/Users/me/src/proj"] {
            let e = MountEntry {
                source: dir.clone(),
                target: Some(protected.into()),
                readonly: None,
            };
            let err = resolve(&[e], &[PathBuf::from(protected)], None).unwrap_err();
            let msg = format!("{err:#}");
            assert!(
                msg.contains(protected),
                "the error must name the path that would be covered: {msg}"
            );
        }

        // A parent of a protected path hides it just as thoroughly.
        let e = MountEntry {
            source: dir.clone(),
            target: Some("/home".into()),
            readonly: None,
        };
        assert!(
            resolve(&[e], &[PathBuf::from("/home/dev")], None).is_err(),
            "mounting over /home hides /home/dev inside it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_rejects_two_targets_that_overlap_each_other() {
        let a = tmp_dir("clash-a");
        let b = tmp_dir("clash-b");
        let entries = vec![
            MountEntry {
                source: a.clone(),
                target: Some("/shared".into()),
                readonly: None,
            },
            MountEntry {
                source: b.clone(),
                target: Some("/shared/inner".into()),
                readonly: None,
            },
        ];

        let err = resolve(&entries, &[], None).unwrap_err();
        assert!(
            format!("{err:#}").contains("/shared"),
            "the second mount would silently win; say which two collide"
        );

        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// `canonicalize` accepts a regular file, and apple/container only
    /// objects while starting the container — after a build, and after
    /// pall8t has already printed a mount line saying it worked
    /// (`CodeRabbit`, PR #46).
    #[test]
    fn resolve_rejects_a_file_as_a_source() {
        let dir = tmp_dir("file-source");
        let file = dir.join("a.txt");
        std::fs::write(&file, "x").unwrap();

        let err = resolve(&[entry(&file)], &[], None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("must be a directory") && msg.contains("a.txt"),
            "the error must name the file and say what was expected, rather than \
             leaving the runtime to fail later: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `~` in a *target* would mean the container's home, not the host's,
    /// so expanding it host-side produced a `/Users/...` path on the wrong
    /// side of the boundary — silently, and differently per machine
    /// (`CodeRabbit`, PR #46).
    #[test]
    fn resolve_does_not_expand_tilde_in_a_target() {
        let dir = tmp_dir("tilde-target");
        let e = MountEntry {
            source: dir.clone(),
            target: Some("~/notes".into()),
            readonly: None,
        };

        let err = resolve(&[e], &[], None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("absolute") && msg.contains('~'),
            "rejecting it is only useful if the message explains why a target is \
             not tilde-expanded: {msg}"
        );
        assert!(
            !msg.contains(&dirs::home_dir().unwrap().display().to_string()),
            "and the host's home must not appear in the target at all: {msg}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The generated git-dir mount skipped the overlap checks the
    /// configured ones get. Mounting a main checkout and then its linked
    /// worktree would nest a `.git` mount inside the checkout's mount —
    /// and with differing modes, land a read-only `.git` inside a writable
    /// checkout, breaking commits there for a reason nothing in the config
    /// hints at (`CodeRabbit`, PR #46).
    #[test]
    fn resolve_does_not_nest_a_git_dir_inside_an_existing_mount() {
        let root = tmp_dir("nested-gitdir");
        let main = root.join("main");
        let linked = root.join("linked");
        std::fs::create_dir_all(&main).unwrap();
        let main_s = main.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let argv: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
            crate::util::run_ok("git", &argv).unwrap()
        };
        git(&["init", "-q", &main_s]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        git(&["-C", &main_s, "add", "-A"]);
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
        ]);
        git(&[
            "-C",
            &main_s,
            "worktree",
            "add",
            "-q",
            &linked.to_string_lossy(),
            "-b",
            "feat",
        ]);

        // Main checkout writable, then the worktree read-only: the git dir
        // lives inside the first mount and is already reachable there.
        let mut worktree = entry(&linked);
        worktree.readonly = Some(true);
        let mounts = resolve(&[entry(&main), worktree], &[], None).unwrap();

        assert_eq!(
            mounts.len(),
            2,
            "one mount per entry and no generated third: the main checkout \
             already exposes its own .git — {mounts:?}"
        );
        let git_dir = main.join(".git").canonicalize().unwrap();
        assert!(
            !mounts.iter().any(|m| m.dest == git_dir),
            "a nested .git mount would make the main checkout's git directory \
             read-only inside a writable checkout"
        );

        // Two linked worktrees of the same repository resolve to the same
        // main `.git`. The second must not emit a duplicate mount: two
        // virtiofs entries with the same source and target are at best
        // redundant and are rejected outright by some runtime versions
        // (`CodeRabbit`, PR #46).
        let second = root.join("linked2");
        git(&[
            "-C",
            &main_s,
            "worktree",
            "add",
            "-q",
            &second.to_string_lossy(),
            "-b",
            "feat2",
        ]);
        let mounts = resolve(&[entry(&linked), entry(&second)], &[], None).unwrap();
        assert_eq!(
            mounts.len(),
            3,
            "two worktrees plus one shared git dir, not two: {mounts:?}"
        );
        let git_mounts = mounts.iter().filter(|m| m.dest == git_dir).count();
        assert_eq!(
            git_mounts, 1,
            "the shared git dir must be mounted exactly once — a duplicate \
             source/target pair is redundant at best and rejected at worst"
        );

        // Something else mounted over the git dir's path is not a mount
        // pall8t can silently skip: git would resolve to the wrong content.
        // The target is spelled canonically because container-side paths
        // are compared as written — the host cannot canonicalize a path
        // that only exists inside the container.
        let mut over = entry(&root);
        over.target = Some(git_dir.clone());
        let mut worktree = entry(&linked);
        worktree.readonly = Some(true);
        let err = resolve(&[over, worktree], &[], None).unwrap_err();
        assert!(
            format!("{err:#}").contains("already mounted over"),
            "a conflicting cover must be reported, not silently accepted: {err:#}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_rejects_a_relative_target_and_a_missing_source() {
        let dir = tmp_dir("bad-input");
        let e = MountEntry {
            source: dir.clone(),
            target: Some("notes".into()),
            readonly: None,
        };
        assert!(
            format!("{:#}", resolve(&[e], &[], None).unwrap_err()).contains("absolute"),
            "a relative target has no meaning inside the container"
        );

        let missing = dir.join("nope");
        assert!(
            format!("{:#}", resolve(&[entry(&missing)], &[], None).unwrap_err())
                .contains("not found"),
            "a source that does not exist must fail loudly, not mount an empty dir"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A linked git worktree keeps its real git directory outside itself,
    /// named by an absolute path in a `.git` pointer file. Mounting the
    /// worktree alone hands the sandbox a directory git cannot read as a
    /// repository at all, so the main `.git` comes too — the rule FR-3
    /// already applies to the workspace.
    #[test]
    fn resolve_linked_worktree_also_mounts_the_main_git_dir() {
        let root = tmp_dir("linked-worktree");
        let main = root.join("main");
        let linked = root.join("linked");
        std::fs::create_dir_all(&main).unwrap();
        let main_s = main.to_string_lossy().into_owned();
        let git = |args: &[&str]| {
            let argv: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
            crate::util::run_ok("git", &argv).unwrap()
        };
        git(&["init", "-q", &main_s]);
        std::fs::write(main.join("a.txt"), "a").unwrap();
        git(&["-C", &main_s, "add", "-A"]);
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
        ]);
        git(&[
            "-C",
            &main_s,
            "worktree",
            "add",
            "-q",
            &linked.to_string_lossy(),
            "-b",
            "feat",
        ]);

        let mut e = entry(&linked);
        e.readonly = Some(true);
        let mounts = resolve(&[e], &[], None).unwrap();

        let expected = main.join(".git").canonicalize().unwrap();
        assert_eq!(mounts.len(), 2, "the worktree plus its git dir: {mounts:?}");
        assert_eq!(mounts[1].host, expected);
        assert_eq!(
            mounts[1].dest, expected,
            "the pointer file and its back-pointer are absolute host paths, so \
             the git dir resolves only at its own path"
        );
        assert!(
            mounts.iter().all(|m| m.readonly),
            "the git dir inherits the entry's mode — mounting it writable under a \
             read-only worktree would reopen exactly what read-only closed"
        );

        // Retargeted, the identity assumption is gone: the pointer file
        // still names the original path, so pulling the git dir in would
        // not make git work and would mount something unasked for.
        let mut moved = entry(&linked);
        moved.target = Some("/elsewhere".into());
        assert_eq!(
            resolve(&[moved], &[], None).unwrap().len(),
            1,
            "a retargeted worktree gets exactly the mount it asked for"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn describe_names_the_mode_and_the_retarget() {
        let identity = Mount::new("/src/lib".into(), "/src/lib".into(), true);
        let d = describe(&identity);
        assert!(
            d.contains("/src/lib") && d.contains("read-only") && !d.contains('→'),
            "an identity mount has one path worth printing: {d}"
        );

        let moved = Mount::new("/src/lib".into(), "/notes".into(), false);
        let d = describe(&moved);
        assert!(
            d.contains("/src/lib") && d.contains("/notes") && d.contains("writable"),
            "a retargeted mount must show both ends, or the agent's view is a \
             mystery from the host side: {d}"
        );
    }

    /// A flag the user typed that cannot do anything must say so. With no
    /// `[[mounts]]` configured a run prints no mount lines at all, so
    /// `--readonly` would otherwise look like it took effect while changing
    /// nothing — and the workspace stays writable, which is what someone
    /// reaching for the flag is most likely testing.
    #[test]
    fn no_mounts_warning_table() {
        let warning = no_mounts_warning(Some(true), 0).expect("a no-op flag must be reported");
        assert!(
            warning.contains("[[mounts]]") && warning.contains("workspace"),
            "the warning must name what the flag governs and what it does not, or \
             it just restates the confusion: {warning}"
        );
        assert_eq!(
            no_mounts_warning(Some(false), 0),
            warning.into(),
            "--readonly=false is equally inert with nothing configured"
        );
        assert_eq!(
            no_mounts_warning(Some(true), 1),
            None,
            "with an entry to act on, the flag is doing its job"
        );
        assert_eq!(
            no_mounts_warning(None, 0),
            None,
            "no flag, no claim to correct — an ordinary run must stay quiet"
        );
    }

    /// The env has to be exactly what git reads: a count, and contiguous
    /// `KEY_n`/`VALUE_n` pairs from zero. Git stops at the count, so an
    /// off-by-one silently drops the last exception and that path goes back
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
}
