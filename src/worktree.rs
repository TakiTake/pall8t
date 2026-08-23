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
        if !git(&repo, &["init", "-q", "-b", "main", "."]) {
            eprintln!("skipping: no usable git on PATH");
            return;
        }
        fs::write(repo.join("f"), "x").unwrap();
        assert!(git(&repo, &["add", "f"]));
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
        ));

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
        ));

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

    /// Runs git in `dir`, reporting whether it succeeded. Returns false
    /// when git is missing entirely, so the test can skip rather than
    /// fail red on a machine without it.
    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
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
