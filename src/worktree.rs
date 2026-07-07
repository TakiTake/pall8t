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
