use std::path::{Path, PathBuf};

/// If `dir/.git` is a worktree pointer file, the main repository's common
/// `.git` directory — identity-mounting it alongside the worktree makes
/// git work inside the container exactly as on the host (FR-3): the
/// pointer file's absolute `gitdir:` path and the `worktrees/<name>/gitdir`
/// back-pointer both stay valid. `None` for a normal repository (its
/// `.git` directory is inside the cwd mount already), a non-repo, or an
/// unparsable layout.
pub fn main_git_dir(dir: &Path) -> Option<PathBuf> {
    let dotgit = dir.join(".git");
    if !std::fs::metadata(&dotgit).ok()?.is_file() {
        return None;
    }
    // Pointer file format: `gitdir: <path>` where <path> is the worktree's
    // private dir, `<main>/.git/worktrees/<name>`. Both paths here can be
    // absolute or relative — `Path::join` handles both, replacing rather
    // than appending when the operand is absolute.
    let text = std::fs::read_to_string(&dotgit).ok()?;
    let gitdir = dir.join(text.strip_prefix("gitdir:")?.trim());
    // That dir's `commondir` file points (usually relatively, `../..`) at
    // the main repository's common `.git`. Reading it, rather than
    // string-stripping `worktrees/<name>`, matches how git itself resolves
    // the common dir.
    let common = std::fs::read_to_string(gitdir.join("commondir")).ok()?;
    gitdir.join(common.trim()).canonicalize().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pall8t-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn none_for_normal_repo_and_non_repo() {
        let dir = tmp("wt-normal");
        assert_eq!(main_git_dir(&dir), None, "no .git at all");
        fs::create_dir_all(dir.join(".git")).unwrap();
        assert_eq!(main_git_dir(&dir), None, ".git directory (normal repo)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_common_dir_through_real_worktree_layout() {
        // Layout as `git worktree add` creates it:
        //   main/.git/worktrees/task/commondir -> "../.."
        //   wt/task/.git -> "gitdir: <main>/.git/worktrees/task"
        let root = tmp("wt-layout");
        let main_git = root.join("main").join(".git");
        let wt_private = main_git.join("worktrees").join("task");
        fs::create_dir_all(&wt_private).unwrap();
        fs::write(wt_private.join("commondir"), "../..\n").unwrap();
        let wt = root.join("wt").join("task");
        fs::create_dir_all(&wt).unwrap();
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", wt_private.display()),
        )
        .unwrap();

        let got = main_git_dir(&wt).expect("worktree should resolve");
        assert_eq!(got, main_git.canonicalize().unwrap());
        let _ = fs::remove_dir_all(&root);
    }

    /// The layout `herdr worktree create` produces: the checkout lives
    /// under herdr's own root (`~/.herdr/worktrees/<repo>/<branch-slug>`
    /// by default), nowhere near the repository it belongs to. Built with
    /// real git rather than hand-written pointer files, because what this
    /// resolution can drift against is git's on-disk format, and the
    /// hand-built fixture above would keep passing if it changed.
    #[test]
    fn resolves_a_herdr_style_worktree_created_by_real_git() {
        let root = tmp("wt-herdr");
        let repo = root.join("src").join("my-project");
        fs::create_dir_all(&repo).unwrap();
        match git(&repo, &["init", "-q", "-b", "main", "."]) {
            // No git at all: nothing to test against, skip honestly.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!("skipping: no git on PATH");
                return;
            }
            // Anything else is git having an opinion, and the fixture
            // failing to build is a red test, not a quiet skip.
            r => assert!(
                r.expect("git could not be run"),
                "`git init` failed: the fixture this test resolves against \
                 was never built, so skipping here would report success for \
                 coverage that did not run"
            ),
        }
        fs::write(repo.join("f"), "x").unwrap();
        assert!(git(&repo, &["add", "f"]).expect("git could not be run"));
        assert!(git(
            &repo,
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "seed",
            ]
        )
        .expect("git could not be run"));

        // herdr's own command shape: `git -C <repo> worktree add -b
        // <branch> <path> <base>`, with <path> under its worktrees root.
        let checkout = root
            .join(".herdr")
            .join("worktrees")
            .join("my-project")
            .join("feature-x");
        assert!(git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/x",
                &checkout.display().to_string(),
                "main",
            ]
        )
        .expect("git could not be run"));

        let got = main_git_dir(&checkout).expect(
            "a herdr-created worktree must resolve to the main repository's .git, \
             or git inside the sandbox sees a dangling pointer",
        );
        assert_eq!(
            got,
            repo.join(".git").canonicalize().unwrap(),
            "the mount pall8t adds has to be the main repo's common .git — \
             the worktree's own pointer file names it by absolute path"
        );
        assert!(
            !got.starts_with(&checkout),
            "the resolved .git is outside the workspace mount, which is the \
             whole reason pall8t mounts it separately"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Runs git in `dir`. `Err` means git could not be *started*;
    /// `Ok(false)` means git ran and failed. Keeping those apart is the
    /// point: a machine with no git is a legitimate skip, while a git that
    /// ran and rejected the command is a broken fixture, and collapsing
    /// both into one `bool` let the second silently delete this test's
    /// coverage while CI stayed green (review finding, PR #68).
    ///
    /// Runs with the host's git configuration switched off, which is not
    /// tidiness but correctness: this test asserts what *pall8t* resolves
    /// out of git's on-disk layout, so any host setting that can make
    /// `git commit` fail turns a green build red for a reason the test is
    /// not about. `commit.gpgsign = true` with an unusable signing program
    /// is the one that was reported (review finding, PR #68), but it is an
    /// instance rather than the problem — `core.hooksPath` pointing at a
    /// rejecting pre-commit hook, a `commit.template` that is missing, or a
    /// `gpg.format` the host cannot satisfy all break it the same way. So
    /// the whole of the host's config is dropped rather than the single
    /// setting overridden: `GIT_CONFIG_GLOBAL`/`SYSTEM` take out
    /// `~/.gitconfig` and `/etc/gitconfig`, and clearing `GIT_CONFIG_COUNT`
    /// (with git's internal `GIT_CONFIG_PARAMETERS`) takes out the
    /// inline-config channel, which outranks both and would otherwise walk
    /// straight back in from the environment the suite was launched with.
    ///
    /// The identity the commits need is passed per-invocation by the
    /// caller, since there is no global config left to supply it.
    ///
    /// Config is only half of it: git also takes a *repository* from the
    /// environment, and `GIT_DIR` outranks the `-C <dir>` above. Exported
    /// by every git hook (`git commit` runs its hooks with it set), by
    /// `git rebase --exec` and by `git bisect run` — so `cargo test` from
    /// any of those would point this fixture at the surrounding
    /// repository. Measured, not feared: with `GIT_DIR` set, `git -C
    /// <fixture> init` re-initializes the *outer* repo, the fixture
    /// directory is left with no `.git` at all, and the `seed` commit
    /// below lands in the developer's own history. `GIT_WORK_TREE`,
    /// `GIT_INDEX_FILE` and `GIT_COMMON_DIR` are the rest of that channel
    /// and travel with it.
    fn git(dir: &Path, args: &[&str]) -> std::io::Result<bool> {
        git_command(dir, args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
    }

    /// The command `git` runs, separated out so the isolation above is
    /// assertable without running git at all.
    fn git_command(dir: &Path, args: &[&str]) -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        for key in GIT_ENV_TO_CLEAR {
            cmd.env_remove(key);
        }
        cmd
    }

    /// Every environment channel that can point git at a repository or a
    /// configuration other than the fixture's own.
    const GIT_ENV_TO_CLEAR: &[&str] = &[
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ];

    /// Pins the isolation itself, because its absence is invisible in a
    /// normal run: the suite passes from a plain shell either way, and
    /// only a `cargo test` launched with `GIT_DIR` in the environment (a
    /// git hook, `rebase --exec`, `bisect run`) tells the difference — by
    /// writing a commit into the surrounding repository. Asserted on the
    /// built command rather than by exporting `GIT_DIR` around a live git
    /// run, since the suite is multi-threaded and the environment is
    /// process-wide.
    #[test]
    fn the_fixture_git_cannot_be_pointed_at_the_surrounding_repository() {
        let cmd = git_command(Path::new("/nonexistent"), &["status"]);
        let envs: Vec<_> = cmd.get_envs().collect();

        // Spelled out here rather than iterating `GIT_ENV_TO_CLEAR`: a
        // test that reads the same list the code does cannot notice an
        // entry being deleted from it — it would just check one thing
        // fewer and stay green. This list is the specification; the
        // constant is the implementation of it.
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_COMMON_DIR",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
        ] {
            assert!(
                envs.iter()
                    .any(|(k, v)| *k == std::ffi::OsStr::new(key) && v.is_none()),
                "{key} must be cleared for the fixture's git: it is inherited \
                 wherever the suite runs under git itself, and it outranks the \
                 `-C <fixture>` this helper passes"
            );
        }

        for (key, expected) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ] {
            assert!(
                envs.iter().any(|(k, v)| *k == std::ffi::OsStr::new(key)
                    && *v == Some(std::ffi::OsStr::new(expected))),
                "{key} must be sent to {expected}, or a host setting such as \
                 `commit.gpgsign` decides whether this test passes"
            );
        }
    }

    #[test]
    fn none_when_commondir_missing() {
        let root = tmp("wt-broken");
        let wt = root.join("task");
        fs::create_dir_all(&wt).unwrap();
        fs::write(wt.join(".git"), "gitdir: /nonexistent/worktrees/task\n").unwrap();
        assert_eq!(main_git_dir(&wt), None);
        let _ = fs::remove_dir_all(&root);
    }
}
