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
