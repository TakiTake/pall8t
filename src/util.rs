//! Small helpers shared across modules.

use anyhow::{anyhow, Context, Result};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Runs `program` with `args`, capturing stdout. A spawn failure or
/// non-zero exit becomes an error carrying the full command line and the
/// trimmed stderr. The contract for every CLI call in this crate whose
/// output is *parsed* (`container list`, `container image ls`, git) —
/// error reporting can't drift between those callers. [`run_streaming`] is
/// the sibling contract for calls whose output is only ever *shown*, not
/// parsed; its errors can't carry captured stderr, since none was captured.
pub(crate) fn run_ok(program: &str, args: &[String]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run: {program} {}", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `program` with `args`, streaming its output live instead of
/// capturing it — for long commands (`container build`, `container system
/// start`) whose progress the user needs to see, not parse. Both the
/// child's stdout and stderr are pointed at *our* stderr (stdout via a
/// dup'd fd, not a forwarding thread), so a caller's own stdout stays
/// clean/machine-readable (e.g. `pall8t build`'s final `built <tag>` line,
/// `pall8t ls --json`) while the child's chatter interleaves with pall8t's
/// own `eprintln!` progress messages. Whether it actually arrives
/// line-by-line depends on the child's own stdio buffering (typically
/// line-buffered on a TTY, block-buffered otherwise) — this just removes
/// pall8t's own buffering, it doesn't control the child's.
///
/// `stdin` is caller-chosen rather than hardcoded, because the right
/// answer genuinely differs by call site: a build has no business reading
/// it, so `container build` passes `Stdio::null()` — but `container
/// system start` can prompt interactively (e.g. apple/container's
/// default-kernel-install confirmation on a fresh machine), so that caller
/// picks `Stdio::inherit()` when its own stdin is a TTY (and `null()`
/// otherwise, to avoid handing a piped payload to an unexpected prompt).
/// `Command::status()` inherits stdin by default if left unset (unlike
/// [`run_ok`]'s `Command::output()`, which doesn't) — this parameter
/// exists so no caller relies on that default by accident.
///
/// A spawn failure or non-zero exit becomes an error carrying the command
/// line; unlike `run_ok` it cannot also carry the child's stderr, since
/// that already streamed to the user rather than being captured.
pub(crate) fn run_streaming(program: &str, args: &[String], stdin: Stdio) -> Result<()> {
    let err = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .context("cannot duplicate stderr for streaming")?;
    let status = Command::new(program)
        .args(args)
        .stdin(stdin)
        .stdout(Stdio::from(err))
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run: {program} {}", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!(
            "`{program} {}` failed (see output above)",
            args.join(" ")
        ));
    }
    Ok(())
}

/// Writes `content` to `path` (creating parent directories) only if the
/// file doesn't exist yet — an existing file, user-edited or not, is
/// never touched. Returns whether the file was created.
pub fn ensure_file(path: &Path, content: &str) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(true)
}

/// Expands a leading `~` against the host's home directory. Config paths
/// are written the way a user types them; every consumer needs the real
/// path.
pub fn expand_tilde(path: &Path) -> PathBuf {
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

/// Lowercase, ASCII-alphanumeric-or-dash rendering of `name`, trimmed of
/// leading/trailing dashes. `"workspace"` when nothing survives.
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
/// the slug costs readability, never correctness. Shared by container names
/// and image tag bases so the derivation can't drift between them.
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

/// How long ago `entry` was last modified, or `None` when that can't be
/// read — an absent or unreadable mtime, or one in the future (a clock
/// step, a filesystem with coarse timestamps). Both reapers in this crate
/// (`herdr::prune_stale_run_bins`, `relay::reap_stale_sockets`) delete
/// things based on this, and both must read `None` as *don't reap*, so
/// the "unknown age" case is decided here once rather than in each walk.
pub(crate) fn entry_age(entry: &std::fs::DirEntry) -> Option<std::time::Duration> {
    entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // The property that actually matters here — a child's stdout lands on
    // *our* stderr, never on our real stdout — isn't safely testable
    // in-process: proving it means swapping this test binary's own fd 1,
    // which `cargo test`'s default parallel harness also writes to from
    // every other concurrently running test. Doing that correctly needs an
    // out-of-process re-exec harness this crate doesn't have elsewhere, so
    // it's left unverified here rather than adding a fragile one-off. What
    // IS worth covering without any of that: a failing command still
    // surfaces as an `Err` carrying the command line, same contract as
    // `run_ok`.

    #[test]
    fn run_streaming_ok_on_success() {
        assert!(run_streaming("true", &[], Stdio::null()).is_ok());
    }

    #[test]
    fn run_streaming_errors_with_command_line_on_nonzero_exit() {
        let err = run_streaming(
            "sh",
            &["-c".to_string(), "exit 7".to_string()],
            Stdio::null(),
        )
        .expect_err("nonzero exit must be an Err");
        let msg = err.to_string();
        assert!(msg.contains("sh"), "message should name the program: {msg}");
        assert!(
            msg.contains("exit 7"),
            "message should include the full command line: {msg}"
        );
    }

    #[test]
    fn run_streaming_errors_when_program_missing() {
        let err = run_streaming(
            "pall8t-test-definitely-not-a-real-binary",
            &[],
            Stdio::null(),
        )
        .expect_err("a missing program must be an Err");
        assert!(err
            .to_string()
            .contains("pall8t-test-definitely-not-a-real-binary"));
    }

    #[test]
    fn run_streaming_stdin_parameter_reaches_the_child() {
        // We can't capture the child's streamed stdout to verify content
        // (that's the whole point of run_streaming), so use exit status as
        // a proxy: `read x` exits nonzero on immediate EOF and zero once it
        // actually reads a line — proving `stdin` is really threaded
        // through to the child rather than silently ignored, regardless of
        // what run_streaming's internals do with it.
        let closed = run_streaming(
            "sh",
            &["-c".to_string(), "read x".to_string()],
            Stdio::null(),
        )
        .expect_err("closed stdin must not satisfy `read`");
        assert!(
            closed.to_string().contains("read x"),
            "must fail via `sh`'s own nonzero exit (EOF), not a spawn error: {closed}"
        );

        let path = std::env::temp_dir().join(format!("pall8t-test-stdin-{}", std::process::id()));
        std::fs::write(&path, "hello\n").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let piped = run_streaming("sh", &["-c".to_string(), "read x".to_string()], file.into());
        let _ = std::fs::remove_file(&path);
        assert!(piped.is_ok(), "content on stdin must satisfy `read`");
    }
}
