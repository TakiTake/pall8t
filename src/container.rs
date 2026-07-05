use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_CONTAINERFILE: &str = include_str!("../Containerfile");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Absent,
    Stopped,
    Running,
}

impl State {
    pub fn label(&self) -> &'static str {
        match self {
            State::Running => "●",
            State::Stopped => "○",
            State::Absent => "·",
        }
    }
}

pub fn host_ids() -> (u32, u32) {
    (read_id("-u").unwrap_or(501), read_id("-g").unwrap_or(20))
}

fn read_id(flag: &str) -> Option<u32> {
    let out = Command::new("id").arg(flag).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// pall8t-<slug(project)>-<sha256(workspace path)[..8]>
pub fn container_name(project_name: &str, workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(workspace.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("pall8t-{}-{}", crate::workspace::slug(project_name), hex)
}

pub fn image_tag(base: &str, uid: u32, gid: u32) -> String {
    format!("{base}:{uid}-{gid}")
}

/// Like [`image_tag`], but suffixed with a Containerfile content hash (see
/// [`containerfile_content_hash`]) so a change to its contents resolves to
/// a new tag.
pub fn image_tag_hashed(base: &str, uid: u32, gid: u32, hash: &str) -> String {
    format!("{base}:{uid}-{gid}-{hash}")
}

/// First 12 hex chars of the sha256 of `containerfile`'s current bytes.
/// Hashing the working-tree contents (rather than, say, the last commit
/// that touched the file) means uncommitted edits are detected too, and a
/// rebuild can never poison a tag: the same content always resolves to the
/// same tag, so the tag always corresponds to the image built from it.
/// `None` if the file can't be read.
pub fn containerfile_content_hash(containerfile: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(containerfile).ok()?;
    let digest = Sha256::digest(&bytes);
    Some(digest.iter().take(6).map(|b| format!("{b:02x}")).collect())
}

fn run_ok<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();
    let out = Command::new("container")
        .args(&argv)
        .output()
        .with_context(|| format!("failed to run: container {}", argv.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`container {}` failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn cli_available() -> bool {
    Command::new("container").arg("--version").output().is_ok()
}

pub fn system_running() -> bool {
    Command::new("container")
        .args(["system", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reconcile source of truth: `container list --all --format json`.
/// Parsed defensively (schema is pre-1.0, see ADR-0001).
pub fn list_all() -> Result<Vec<(String, State)>> {
    let stdout = run_ok(["list", "--all", "--format", "json"])?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let v: Value = serde_json::from_str(trimmed).context("unexpected `container list` JSON")?;
    let mut items = Vec::new();
    if let Some(arr) = v.as_array() {
        for item in arr {
            let name = item
                .pointer("/configuration/id")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))
                .or_else(|| item.get("name").and_then(Value::as_str));
            let status = item
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(name) = name {
                let state = if status.eq_ignore_ascii_case("running") {
                    State::Running
                } else {
                    State::Stopped
                };
                items.push((name.to_string(), state));
            }
        }
    }
    Ok(items)
}

/// Normalizes an image reference string as it can appear from `container
/// image list`/`inspect` down to bare `base:tag` form, so references can
/// be compared regardless of registry/repo qualification
/// (`registry:5000/ns/base:tag`) or a `@sha256:...` digest suffix
/// (`base:tag@sha256:...`). There is a single normalization point so
/// qualification/digest handling can't drift between call sites (previously
/// `ref_matches` and `ref_has_prefix` disagreed on digests, which let a
/// freshly built, digest-qualified image be classified as prunable and
/// self-delete). Strips the digest first — it's always the last
/// `@`-delimited component — then strips everything up to and including
/// the last `/`: registry/namespace qualification never itself contains a
/// `:tag`, so the last `/` is always the boundary between qualification
/// and `name:tag` (a `registry:port/...` prefix's colon doesn't interfere,
/// since it's before that last `/`).
fn normalize_ref(s: &str) -> &str {
    let without_digest = s.split('@').next().unwrap_or(s);
    without_digest.rsplit('/').next().unwrap_or(without_digest)
}

/// True if `s` (a reference string from `container image list`/`inspect`)
/// refers to `tag` once normalized (see [`normalize_ref`]). Deliberately
/// not a substring match: with hash-suffixed tags, the unsuffixed form
/// (e.g. `pall8t-x:501-20`) is a substring of a differently-hashed sibling
/// (`pall8t-x:501-20-abc123456789`), so substring matching would report a
/// tag as existing when only that sibling does.
pub(crate) fn ref_matches(s: &str, tag: &str) -> bool {
    normalize_ref(s) == tag
}

/// True if `s` starts with `prefix` once normalized (see [`normalize_ref`]).
/// Same acceptance rule as [`ref_matches`], for prefix rather than exact
/// matching.
pub(crate) fn ref_has_prefix(s: &str, prefix: &str) -> bool {
    normalize_ref(s).starts_with(prefix)
}

/// True if `s` is an image reference for `base` scoped to `uid`-`gid`:
/// either the unsuffixed fallback tag (`base:uid-gid`) or a hash-suffixed
/// variant (`base:uid-gid-<hash>`), matched per [`ref_matches`]/
/// [`ref_has_prefix`]. Used to scope pruning so a `pall8t-<slug>` base
/// shared across host users doesn't delete a different uid/gid's images.
/// The trailing `-` on the hash-suffix prefix also disambiguates e.g. gid
/// `2` from gid `20`: `base:uid-2-` is not a prefix of `base:uid-20-...`,
/// since the character right after `2` differs (`-` vs `0`).
pub(crate) fn image_owned_by(s: &str, base: &str, uid: u32, gid: u32) -> bool {
    let unsuffixed = image_tag(base, uid, gid);
    let hash_prefix = format!("{unsuffixed}-");
    ref_matches(s, &unsuffixed) || ref_has_prefix(s, &hash_prefix)
}

/// True if `s` is a superseded-build candidate that pruning should delete:
/// it belongs to `base`/`uid`/`gid` (see [`image_owned_by`]) and it is not
/// `keep_tag`. The keep-exclusion uses [`ref_matches`], not `!=`, because
/// `s` can be registry/digest-qualified (per [`image_owned_by`]) while
/// `keep_tag` — the tag just passed to `container build -t` — never is; a
/// raw string inequality would then treat the qualified form of the image
/// just built as "not `keep_tag`" and delete it out from under the caller.
pub(crate) fn should_prune(s: &str, keep_tag: &str, base: &str, uid: u32, gid: u32) -> bool {
    !ref_matches(s, keep_tag) && image_owned_by(s, base, uid, gid)
}

/// Walks a `container image list`/`inspect` JSON value (schema is pre-1.0,
/// see ADR-0001, hence the defensive walk rather than a fixed pointer path)
/// and calls `f` with every string found. Shared by every function that
/// scans image references, so there's one place that knows how the JSON is
/// shaped.
fn for_each_string(v: &Value, f: &mut impl FnMut(&str)) {
    match v {
        Value::String(s) => f(s),
        Value::Array(a) => a.iter().for_each(|x| for_each_string(x, f)),
        Value::Object(m) => m.values().for_each(|x| for_each_string(x, f)),
        _ => {}
    }
}

pub fn image_exists(tag: &str) -> bool {
    let Some(v) = run_ok(["image", "list", "--format", "json"])
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok())
    else {
        return false;
    };
    let mut found = false;
    for_each_string(&v, &mut |s| found = found || ref_matches(s, tag));
    found
}

/// Pure filter+dedup core of [`prunable_images`], factored out for
/// testability. From a flat list of reference strings, returns the ones
/// that pruning should delete: owned by `base`/`uid`/`gid`, not `keep_tag`
/// (the tag just built), and not `in_use` (typically the image `ctx`'s
/// container currently runs, if any — deleting it out from under a
/// live/stopped container would break it) — see [`should_prune`]. All
/// comparisons are qualification/digest-aware (see [`normalize_ref`]).
/// Deduped by normalized form: the CLI can expose the same image under
/// multiple qualified spellings (e.g. `x:t` and `localhost/x:t`), and
/// calling `image_delete` on the same image twice under different
/// spellings would report a spurious failure on the second attempt.
fn filter_prunable<'a>(
    refs: impl Iterator<Item = &'a str>,
    base: &str,
    keep_tag: &str,
    uid: u32,
    gid: u32,
    in_use: Option<&str>,
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in refs {
        if !should_prune(s, keep_tag, base, uid, gid) {
            continue;
        }
        if let Some(in_use) = in_use {
            if normalize_ref(s) == normalize_ref(in_use) {
                continue;
            }
        }
        if seen.insert(normalize_ref(s).to_string()) {
            out.push(s.to_string());
        }
    }
    out.sort();
    out
}

/// Reference strings from `container image list` that pruning after a
/// successful build should delete. See [`filter_prunable`] for the
/// matching/dedup rules.
pub fn prunable_images(
    base: &str,
    keep_tag: &str,
    uid: u32,
    gid: u32,
    in_use: Option<&str>,
) -> Result<Vec<String>> {
    let stdout = run_ok(["image", "list", "--format", "json"])?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let v: Value =
        serde_json::from_str(trimmed).context("unexpected `container image list` JSON")?;
    let mut refs = Vec::new();
    for_each_string(&v, &mut |s| refs.push(s.to_string()));
    Ok(filter_prunable(
        refs.iter().map(String::as_str),
        base,
        keep_tag,
        uid,
        gid,
        in_use,
    ))
}

/// Delete an image by tag/reference.
pub fn image_delete(tag: &str) -> Result<()> {
    run_ok(["image", "delete", tag])?;
    Ok(())
}

pub fn build_image(containerfile: &Path, ctx_dir: &Path, tag: &str, uid: u32, gid: u32) -> Result<()> {
    run_ok([
        "build".to_string(),
        "-f".to_string(),
        containerfile.to_string_lossy().into_owned(),
        "-t".to_string(),
        tag.to_string(),
        "--build-arg".to_string(),
        format!("UID={uid}"),
        "--build-arg".to_string(),
        format!("GID={gid}"),
        ctx_dir.to_string_lossy().into_owned(),
    ])?;
    Ok(())
}

pub struct RunSpec {
    pub name: String,
    pub workspace: PathBuf,
    pub image: String,
    pub cpus: u32,
    pub memory: String,
    pub uid: u32,
    pub gid: u32,
}

/// Identity-path mount: the workspace is visible at the same absolute path
/// inside the container (ADR-0004).
pub fn run_detached(spec: &RunSpec) -> Result<()> {
    let home = home_mount()?;
    let ws = spec.workspace.to_string_lossy().into_owned();
    run_ok([
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        spec.name.clone(),
        "-v".to_string(),
        format!("{ws}:{ws}"),
        "-v".to_string(),
        format!("{}:/home/dev", home.display()),
        "-w".to_string(),
        ws.clone(),
        "--user".to_string(),
        "dev".to_string(),
        "--uid".to_string(),
        spec.uid.to_string(),
        "--gid".to_string(),
        spec.gid.to_string(),
        "--cpus".to_string(),
        spec.cpus.to_string(),
        "--memory".to_string(),
        spec.memory.clone(),
        spec.image.clone(),
        "sleep".to_string(),
        "infinity".to_string(),
    ])?;
    Ok(())
}

pub fn start(name: &str) -> Result<()> {
    run_ok(["start", name])?;
    Ok(())
}

pub fn stop(name: &str) -> Result<()> {
    run_ok(["stop", name])?;
    Ok(())
}

pub fn delete(name: &str) -> Result<()> {
    run_ok(["delete", name])?;
    Ok(())
}

pub fn logs(name: &str) -> Result<String> {
    run_ok(["logs", name])
}

/// Image reference a container was created from (via `container inspect`).
pub fn image_ref(name: &str) -> Option<String> {
    let out = run_ok(["inspect", name]).ok()?;
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    v.pointer("/0/configuration/image/reference")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// argv for a tab's PTY child: exec into the project container at the
/// workspace path. pall8t itself runs with a full PATH, so bare `container`
/// resolves here (unlike v1's external tabs).
pub fn exec_argv(name: &str, workspace: &Path, cmd: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "exec".into(),
        "-it".into(),
        "--user".into(),
        "dev".into(),
        "-w".into(),
        workspace.to_string_lossy().into_owned(),
        name.to_string(),
    ];
    argv.extend(cmd.iter().cloned());
    argv
}

/// Persistent container-side $HOME (claude auth, shell history, dotfiles).
pub fn home_mount() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".pall8t")
        .join("home");
    std::fs::create_dir_all(&home)?;
    Ok(home)
}

/// Writes the embedded default Containerfile to ~/.pall8t/Containerfile.
pub fn default_containerfile_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("cannot determine home directory")?
        .join(".pall8t");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("Containerfile");
    std::fs::write(&path, DEFAULT_CONTAINERFILE)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn normalize_ref_table() {
        assert_eq!(normalize_ref("pall8t-x:501-20"), "pall8t-x:501-20");
        assert_eq!(
            normalize_ref("localhost/pall8t-x:501-20"),
            "pall8t-x:501-20"
        );
        assert_eq!(
            normalize_ref("registry:5000/ns/pall8t-x:501-20"),
            "pall8t-x:501-20"
        );
        assert_eq!(
            normalize_ref("pall8t-x:501-20@sha256:deadbeef"),
            "pall8t-x:501-20"
        );
        assert_eq!(
            normalize_ref("localhost/pall8t-x:501-20@sha256:deadbeef"),
            "pall8t-x:501-20"
        );
    }

    #[test]
    fn ref_matches_table() {
        let tag = "pall8t-x:501-20";
        assert!(ref_matches(tag, tag), "exact match");
        assert!(
            ref_matches("localhost/pall8t-x:501-20", tag),
            "registry-qualified match"
        );
        assert!(
            ref_matches("pall8t-x:501-20@sha256:deadbeef", tag),
            "digest-qualified match"
        );
        assert!(
            ref_matches("localhost/pall8t-x:501-20@sha256:deadbeef", tag),
            "registry- and digest-qualified match"
        );
        assert!(
            !ref_matches("pall8t-x:501-20-abc123456789", tag),
            "hash-suffixed sibling must not match the unsuffixed tag"
        );
        assert!(
            !ref_matches("pall8t-x:501-2", "pall8t-x:501-20"),
            "501-2 must not match 501-20"
        );
    }

    #[test]
    fn ref_has_prefix_table() {
        let prefix = "pall8t-x:501-20-";
        assert!(ref_has_prefix("pall8t-x:501-20-abc123456789", prefix));
        assert!(ref_has_prefix(
            "localhost/pall8t-x:501-20-abc123456789",
            prefix
        ));
        assert!(ref_has_prefix(
            "pall8t-x:501-20-abc123456789@sha256:deadbeef",
            prefix
        ));
        assert!(
            !ref_has_prefix("pall8t-x:501-2-abc123456789", prefix),
            "501-2- must not match the 501-20- prefix"
        );
    }

    #[test]
    fn image_owned_by_table() {
        let base = "pall8t-x";
        assert!(
            image_owned_by("pall8t-x:501-20", base, 501, 20),
            "unsuffixed exact match"
        );
        assert!(
            image_owned_by("localhost/pall8t-x:501-20-abc123456789", base, 501, 20),
            "registry-qualified hash-suffixed match"
        );
        assert!(
            !image_owned_by("pall8t-x:501-20-abc123456789", base, 501, 2),
            "hash-suffixed image for a different gid must not match"
        );
        assert!(
            !image_owned_by("pall8t-x:501-20", base, 501, 2),
            "501-2 must not match a 501-20 image"
        );
    }

    #[test]
    fn should_prune_table() {
        let base = "pall8t-x";
        let keep_tag = "pall8t-x:501-20-newhash123456";
        assert!(
            !should_prune(keep_tag, keep_tag, base, 501, 20),
            "verbatim keep_tag must not be pruned"
        );
        assert!(
            !should_prune(&format!("localhost/{keep_tag}"), keep_tag, base, 501, 20),
            "registry-qualified form of keep_tag must not be pruned"
        );
        assert!(
            should_prune(
                "pall8t-x:501-20-oldhash654321",
                keep_tag,
                base,
                501,
                20
            ),
            "a differently-hashed sibling must be pruned"
        );
        assert!(
            should_prune(
                "localhost/pall8t-x:501-20-oldhash654321",
                keep_tag,
                base,
                501,
                20
            ),
            "a registry-qualified differently-hashed sibling must be pruned"
        );
        assert!(
            !should_prune("pall8t-x:501-2-oldhash654321", keep_tag, base, 501, 20),
            "a different gid's image must not be pruned even if not keep_tag"
        );
        assert!(
            !should_prune(
                &format!("{keep_tag}@sha256:deadbeef"),
                keep_tag,
                base,
                501,
                20
            ),
            "digest-qualified form of keep_tag must not be pruned"
        );
        assert!(
            should_prune(
                "pall8t-x:501-20-oldhash654321@sha256:deadbeef",
                keep_tag,
                base,
                501,
                20
            ),
            "a digest-qualified differently-hashed sibling must be pruned"
        );
    }

    #[test]
    fn filter_prunable_table() {
        let base = "pall8t-x";
        let keep_tag = "pall8t-x:501-20-newhash123456";
        let old = "pall8t-x:501-20-oldhash654321";
        let refs = [
            keep_tag,
            &format!("localhost/{keep_tag}"), // keep_tag under another spelling
            old,
            &format!("localhost/{old}"), // same superseded image, listed twice
            "pall8t-x:501-2-oldhash654321", // different gid — not ours to prune
        ];

        let pruned = filter_prunable(refs.iter().copied(), base, keep_tag, 501, 20, None);
        assert_eq!(
            pruned,
            vec![old.to_string()],
            "keeps keep_tag and the other gid's image, dedupes the qualified duplicate"
        );

        let none_in_use = filter_prunable(refs.iter().copied(), base, keep_tag, 501, 20, Some(old));
        assert!(
            none_in_use.is_empty(),
            "an in_use image must not be pruned even if it's a superseded sibling"
        );
    }

    #[test]
    fn containerfile_content_hash_is_stable_and_12_chars() {
        let dir = std::env::temp_dir().join(format!("pall8t-test-hash-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Containerfile");
        fs::write(&file, "FROM scratch\n").unwrap();

        let first = containerfile_content_hash(&file).expect("hash");
        let second = containerfile_content_hash(&file).expect("hash");
        assert_eq!(first.len(), 12);
        assert_eq!(first, second);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn containerfile_content_hash_changes_with_content() {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-hash-diff-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Containerfile");

        fs::write(&file, "FROM scratch\n").unwrap();
        let a = containerfile_content_hash(&file).unwrap();
        fs::write(&file, "FROM scratch\nRUN true\n").unwrap();
        let b = containerfile_content_hash(&file).unwrap();

        assert_ne!(a, b);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn containerfile_content_hash_none_when_missing() {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-hash-missing-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let file = dir.join("Containerfile");

        assert_eq!(containerfile_content_hash(&file), None);
    }
}
