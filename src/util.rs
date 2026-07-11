//! Small helpers shared across modules.

use anyhow::{anyhow, Context, Result};
use std::os::fd::AsFd;
use std::path::Path;
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
/// capturing it — for long commands (`container build`) whose progress the
/// user needs to see, not parse. Both the child's stdout and stderr are
/// pointed at *our* stderr (stdout via a dup'd fd, not a forwarding
/// thread), so a caller's own stdout stays clean/machine-readable (e.g.
/// `pall8t build`'s final `built <tag>` line, `pall8t ls --json`) while
/// the child's chatter interleaves with pall8t's own `eprintln!` progress
/// messages. Whether it actually arrives line-by-line depends on the
/// child's own stdio buffering (typically line-buffered on a TTY,
/// block-buffered otherwise) — this just removes pall8t's own buffering,
/// it doesn't control the child's. Stdin is explicitly closed: a
/// long-running build has no business reading it, and leaving it to
/// `Command::status()`'s default (unlike [`run_ok`]'s `Command::output()`,
/// which closes it) would hand a piped prompt or the controlling TTY to
/// the child. A spawn failure or non-zero exit becomes an error carrying
/// the command line; unlike `run_ok` it cannot also carry the child's
/// stderr, since that already streamed to the user rather than being
/// captured.
pub(crate) fn run_streaming(program: &str, args: &[String]) -> Result<()> {
    let err = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .context("cannot duplicate stderr for streaming")?;
    let status = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(run_streaming("true", &[]).is_ok());
    }

    #[test]
    fn run_streaming_errors_with_command_line_on_nonzero_exit() {
        let err = run_streaming("sh", &["-c".to_string(), "exit 7".to_string()])
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
        let err = run_streaming("pall8t-test-definitely-not-a-real-binary", &[])
            .expect_err("a missing program must be an Err");
        assert!(err
            .to_string()
            .contains("pall8t-test-definitely-not-a-real-binary"));
    }
}
