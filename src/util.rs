//! Small helpers shared across modules.

use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::process::Command;

/// Runs `program` with `args`, capturing stdout. A spawn failure or
/// non-zero exit becomes an error carrying the full command line and the
/// trimmed stderr. The one subprocess contract for every CLI this crate
/// shells out to (`container`, `git`), so error reporting can't drift
/// between them.
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
