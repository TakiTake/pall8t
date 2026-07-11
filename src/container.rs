use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_CONTAINERFILE: &str = include_str!("../Containerfile");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stopped,
    Running,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Running => "running",
            State::Stopped => "stopped",
        }
    }
}

pub fn host_ids() -> (u32, u32) {
    // SAFETY: getuid/getgid cannot fail and have no preconditions.
    unsafe { (libc::getuid(), libc::getgid()) }
}

/// First `n` bytes of `bytes`'s sha256 digest, as lowercase hex (`2*n`
/// characters). Shared by every call site that needs a short, stable
/// content fingerprint, so the digest/truncation logic can't drift
/// between them.
pub(crate) fn sha256_hex_prefix(bytes: &[u8], n: usize) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .take(n)
        .fold(String::new(), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// pall8t-<path key of cwd>-<pid> (see [`crate::repos::path_key`]). The
/// pid keeps parallel runs from the same directory from colliding on
/// `--name`.
pub fn run_name(workspace: &Path) -> String {
    format!(
        "pall8t-{}-{}",
        crate::repos::path_key(workspace),
        std::process::id()
    )
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
    let bytes = std::fs::read(containerfile).ok()?;
    Some(sha256_hex_prefix(&bytes, 6))
}

fn run_ok<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();
    crate::util::run_ok("container", &argv)
}

pub enum SystemStatus {
    Running,
    Stopped,
    /// The `container` CLI itself couldn't be spawned.
    CliMissing,
}

/// One `container system status` probe. A spawn failure doubles as the
/// missing-CLI check, so the happy path costs a single subprocess.
pub fn system_status() -> SystemStatus {
    match Command::new("container")
        .args(["system", "status"])
        .output()
    {
        Ok(out) if out.status.success() => SystemStatus::Running,
        Ok(_) => SystemStatus::Stopped,
        Err(_) => SystemStatus::CliMissing,
    }
}

/// Starts the apple/container system service (`container system start`),
/// inheriting stdio so its progress is visible.
pub fn system_start() -> Result<()> {
    let status = Command::new("container")
        .args(["system", "start"])
        .status()
        .context("failed to run: container system start")?;
    if !status.success() {
        return Err(anyhow!("`container system start` failed"));
    }
    Ok(())
}

/// One row of `container list --all`.
pub struct ContainerInfo {
    pub name: String,
    pub state: State,
    /// Image reference the container was created from, when the listing
    /// carries it.
    pub image: Option<String>,
}

/// All containers: `container list --all --format json`.
/// Parsed defensively (schema is pre-1.0, see ADR-0001).
pub fn list_all() -> Result<Vec<ContainerInfo>> {
    let stdout = run_ok(["list", "--all", "--format", "json"])?;
    parse_list_all(&stdout)
}

/// Pure core of [`list_all`], factored out for testability against literal
/// `container list --all --format json` output.
fn parse_list_all(stdout: &str) -> Result<Vec<ContainerInfo>> {
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
            // `status` is a nested object (`{state, networks, startedDate}`)
            // on current apple/container, but the schema is pre-1.0 (ADR-0001)
            // — fall back to a bare string in case an older/other CLI build
            // reports it directly. Getting this wrong silently misreports
            // every running container as stopped (`unwrap_or_default` below
            // never matches "running"), so the nested lookup comes first.
            let status = item
                .pointer("/status/state")
                .and_then(Value::as_str)
                .or_else(|| item.get("status").and_then(Value::as_str))
                .unwrap_or_default();
            let image = item
                .pointer("/configuration/image/reference")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(name) = name {
                let state = if status.eq_ignore_ascii_case("running") {
                    State::Running
                } else {
                    State::Stopped
                };
                items.push(ContainerInfo {
                    name: name.to_string(),
                    state,
                    image,
                });
            }
        }
    }
    Ok(items)
}

/// Containers started by pall8t (names carry the `pall8t-` prefix, see
/// [`run_name`]).
pub fn list_pall8t() -> Result<Vec<ContainerInfo>> {
    Ok(list_all()?
        .into_iter()
        .filter(|c| c.name.starts_with("pall8t-"))
        .collect())
}

/// Strips the digest suffix from a reference: a reference contains at most
/// one `@` (introducing the digest), so everything from the first `@`
/// onward is the digest, and stripping it is a no-op for a reference that
/// has none.
fn strip_digest(s: &str) -> &str {
    s.split('@').next().unwrap_or(s)
}

/// Normalizes an image reference string as it can appear from `container
/// image list`/`inspect` down to bare `base:tag` form, so references can
/// be compared regardless of registry/repo qualification
/// (`registry:5000/ns/base:tag`) or a `@sha256:...` digest suffix
/// (`base:tag@sha256:...`). There is a single normalization point so
/// qualification/digest handling can't drift between call sites (previously
/// `ref_matches` and `ref_has_prefix` disagreed on digests, which let a
/// freshly built, digest-qualified image be classified as prunable and
/// self-delete). Strips the digest first (see [`strip_digest`]), then
/// strips everything up to and including the last `/`: registry/namespace
/// qualification never itself contains a `:tag`, so the last `/` is always
/// the boundary between qualification and `name:tag` (a `registry:port/...`
/// prefix's colon doesn't interfere, since it's before that last `/`).
/// Used by [`ref_has_prefix`] and the dedup/in-use matching that only ever
/// deals with references this crate itself builds — never with a
/// caller-supplied, possibly cross-registry `tag` (see [`ref_matches`],
/// which needs a subtler comparison for that case).
fn normalize_ref(s: &str) -> &str {
    let without_digest = strip_digest(s);
    without_digest.rsplit('/').next().unwrap_or(without_digest)
}

/// True if `s` (a reference string from `container image list`/`inspect`)
/// refers to `tag`. Both are digest-stripped (see [`strip_digest`]) and
/// then compared for equality OR a `/`-bounded suffix match in either
/// direction — NOT via [`normalize_ref`], which would strip qualification
/// down to bare `name:tag` on both sides and so treat any two images
/// sharing a bare name as the same image regardless of registry (e.g.
/// `ghcr.io/org/tool:1` and `docker.io/other/tool:1` would wrongly match).
/// The suffix check instead only accepts one side being an unqualified
/// tail of the other at a `/` boundary — e.g. a bare `postgres:16` matches
/// `docker.io/library/postgres:16`, since that's how `container inspect`
/// can report a reference that was configured or built bare — while still
/// rejecting a differently-registried qualification. The boundary
/// requirement also keeps hash-suffixed tags safe: with
/// `pall8t-x:501-20` vs. a differently-hashed sibling
/// `pall8t-x:501-20-abc123456789`, the shorter is a plain substring but
/// not a `/`-bounded suffix of the longer, so they correctly don't match
/// (equally, `xpostgres:16` doesn't match `postgres:16`: `x` precedes the
/// shared suffix, not `/`).
///
/// Inherent limitation, accepted: if `container inspect` currently reports
/// a BARE ref (e.g. `postgres:16`) and the config then switches to a
/// DIFFERENT registry's qualification of the same `name:tag` (e.g.
/// `ghcr.io/myorg/postgres:16`), that change goes undetected — a bare ref
/// is a legitimate `/`-suffix of any qualification, so it can't be
/// rejected without also breaking the bare↔qualified acceptance this
/// function exists for.
pub(crate) fn ref_matches(s: &str, tag: &str) -> bool {
    let a = strip_digest(s);
    let b = strip_digest(tag);
    a == b || is_slash_suffix(a, b) || is_slash_suffix(b, a)
}

/// True if `longer` ends with `shorter` immediately preceded by a `/`,
/// i.e. `shorter` is `longer`'s unqualified `name:tag` suffix. The `/`
/// boundary check is what stops a plain-substring false positive like
/// `xpostgres:16` "ending with" `postgres:16`.
fn is_slash_suffix(longer: &str, shorter: &str) -> bool {
    longer.len() > shorter.len()
        && longer.ends_with(shorter)
        && longer.as_bytes()[longer.len() - shorter.len() - 1] == b'/'
}

/// True if `s` starts with `prefix` once normalized (see [`normalize_ref`]).
/// Same acceptance rule as [`ref_matches`], for prefix rather than exact
/// matching.
pub(crate) fn ref_has_prefix(s: &str, prefix: &str) -> bool {
    normalize_ref(s).starts_with(prefix)
}

/// True if `candidate` refers to the same image as any entry of `in_use`.
/// Normalized comparison (see [`normalize_ref`]) — sound here because both
/// sides only ever come from this crate's own builds and listings, never a
/// caller-supplied cross-registry reference. The one in-use predicate,
/// shared by pruning and poisoned-tag deletion so their matching can't
/// drift.
pub(crate) fn in_use_contains(in_use: &[String], candidate: &str) -> bool {
    in_use
        .iter()
        .any(|u| normalize_ref(candidate) == normalize_ref(u))
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
/// (the tag just built), and not in `in_use` (the images existing
/// containers currently run — deleting one out from under a live/stopped
/// container would break it) — see [`should_prune`]. All comparisons are
/// qualification/digest-aware (see [`ref_matches`] and [`normalize_ref`]).
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
    in_use: &[String],
) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for s in refs {
        if !should_prune(s, keep_tag, base, uid, gid) {
            continue;
        }
        if in_use_contains(in_use, s) {
            continue;
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
    in_use: &[String],
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

pub fn build_image(
    containerfile: &Path,
    ctx_dir: &Path,
    tag: &str,
    uid: u32,
    gid: u32,
) -> Result<()> {
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

/// One `-v host:dest` bind mount of a [`RunSpec`].
pub struct Mount {
    pub host: PathBuf,
    pub dest: PathBuf,
}

impl Mount {
    /// Identity-path mount: `host` is visible at the same absolute path
    /// inside the container, so git metadata and path references stay
    /// valid on both sides (ADR-0004's insight, retained by ADR-0006).
    pub fn identity(path: PathBuf) -> Self {
        Mount {
            host: path.clone(),
            dest: path,
        }
    }
}

pub struct RunSpec {
    pub name: String,
    pub image: String,
    pub workdir: PathBuf,
    pub mounts: Vec<Mount>,
    pub cpus: u32,
    pub memory: String,
    pub uid: u32,
    pub gid: u32,
    /// Allocate a TTY (`-t`). Callers pass whether their stdin is a
    /// terminal: apple/container 1.0.0 fails outright when `-t` is
    /// requested without one, which would break scripted callers.
    pub tty: bool,
    pub command: Vec<String>,
}

/// argv (after `container`) for the foreground run: interactive (TTY when
/// the caller has one, see [`RunSpec::tty`]), removed on exit — session
/// lifetime equals process lifetime (ADR-0006).
pub fn run_argv(spec: &RunSpec) -> Vec<String> {
    let mut argv: Vec<String> = vec!["run".into(), "-i".into()];
    if spec.tty {
        argv.push("-t".into());
    }
    argv.extend(["--rm".into(), "--name".into(), spec.name.clone()]);
    for m in &spec.mounts {
        argv.push("-v".into());
        argv.push(format!("{}:{}", m.host.display(), m.dest.display()));
    }
    argv.extend([
        "-w".into(),
        spec.workdir.to_string_lossy().into_owned(),
        "--user".into(),
        "dev".into(),
        "--uid".into(),
        spec.uid.to_string(),
        "--gid".into(),
        spec.gid.to_string(),
        "--cpus".into(),
        spec.cpus.to_string(),
        "--memory".into(),
        spec.memory.clone(),
        spec.image.clone(),
    ]);
    argv.extend(spec.command.iter().cloned());
    argv
}

/// argv (after `container`) for `pall8t exec`: a command inside a running
/// container (all pall8t containers have the `dev` user). `tty` follows
/// the same rule as [`RunSpec::tty`]. `workdir` — the directory the
/// container was created with (see [`workdir`]) — anchors the command to
/// the workspace instead of the image WORKDIR; omitted when unknown.
pub fn exec_argv(name: &str, cmd: &[String], tty: bool, workdir: Option<&str>) -> Vec<String> {
    let mut argv: Vec<String> = vec!["exec".into(), "-i".into()];
    if tty {
        argv.push("-t".into());
    }
    if let Some(w) = workdir {
        argv.extend(["-w".into(), w.to_string()]);
    }
    argv.extend(["--user".into(), "dev".into(), name.to_string()]);
    argv.extend(cmd.iter().cloned());
    argv
}

pub fn stop(name: &str) -> Result<()> {
    run_ok(["stop", name])?;
    Ok(())
}

/// One string field out of `container inspect <name>` by JSON pointer.
fn inspect_str(name: &str, pointer: &str) -> Option<String> {
    let out = run_ok(["inspect", name]).ok()?;
    let v: Value = serde_json::from_str(out.trim()).ok()?;
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Initial working directory a container was created with (via `container
/// inspect`) — for `pall8t run` containers, the workspace it mounted.
pub fn workdir(name: &str) -> Option<String> {
    inspect_str(name, "/0/configuration/initProcess/workingDirectory")
}

/// Persistent container-side $HOME (claude auth, shell history, dotfiles).
pub fn home_mount() -> Result<PathBuf> {
    let home = crate::config::pall8t_root()?.join("home");
    std::fs::create_dir_all(&home)?;
    Ok(home)
}

/// Where the default Containerfile lives: ~/.pall8t/Containerfile.
pub fn default_containerfile_location() -> Result<PathBuf> {
    Ok(crate::config::pall8t_root()?.join("Containerfile"))
}

/// Materializes the embedded default Containerfile at
/// ~/.pall8t/Containerfile if it doesn't exist yet. An existing file is
/// left untouched — it's the user's to edit (a shipped update to the
/// embedded default therefore doesn't propagate to it; delete the file to
/// re-materialize the current default).
pub fn default_containerfile_path() -> Result<PathBuf> {
    let path = default_containerfile_location()?;
    crate::util::ensure_file(&path, DEFAULT_CONTAINERFILE)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Real shape from `container list --all --format json` (apple/container
    /// 1.0.0): `status` is a nested object, not a bare string. Regression
    /// test for the bug where every container was misreported `stopped`
    /// because the parser looked for a top-level string that never existed.
    #[test]
    fn parse_list_all_reads_nested_status_state() {
        let json = r#"[
            {
                "id": "pall8t-x-1",
                "configuration": {
                    "id": "pall8t-x-1",
                    "image": { "reference": "pall8t-x:501-20-abc123" }
                },
                "status": {
                    "state": "running",
                    "networks": [],
                    "startedDate": "2026-07-11T02:33:10Z"
                }
            },
            {
                "id": "pall8t-x-2",
                "configuration": {
                    "id": "pall8t-x-2",
                    "image": { "reference": "pall8t-x:501-20-def456" }
                },
                "status": { "state": "stopped", "networks": [] }
            }
        ]"#;
        let items = parse_list_all(json).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "pall8t-x-1");
        assert_eq!(items[0].state, State::Running);
        assert_eq!(items[0].image.as_deref(), Some("pall8t-x:501-20-abc123"));
        assert_eq!(items[1].name, "pall8t-x-2");
        assert_eq!(items[1].state, State::Stopped);
    }

    /// Defensive fallback (schema is pre-1.0, ADR-0001): a bare top-level
    /// `status` string, in case a different apple/container build reports
    /// it that way, still parses correctly.
    #[test]
    fn parse_list_all_falls_back_to_bare_status_string() {
        let json = r#"[
            {
                "id": "pall8t-x-1",
                "configuration": { "id": "pall8t-x-1" },
                "status": "running"
            }
        ]"#;
        let items = parse_list_all(json).unwrap();
        assert_eq!(items[0].state, State::Running);
    }

    #[test]
    fn parse_list_all_empty_output_is_empty() {
        assert!(parse_list_all("").unwrap().is_empty());
        assert!(parse_list_all("   ").unwrap().is_empty());
    }

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

        let qualified_tag = "ghcr.io/org/tool:1";
        assert!(
            ref_matches("ghcr.io/org/tool:1", qualified_tag),
            "exact qualified tag matches itself"
        );
        assert!(
            ref_matches("ghcr.io/org/tool:1@sha256:deadbeef", qualified_tag),
            "digest-pinned inspect ref matches its own qualified tag"
        );
        assert!(
            !ref_matches("ghcr.io/org/other:1", qualified_tag),
            "different qualified image must not match"
        );

        // A bare `tag` matches a qualified inspect ref of the same image...
        assert!(
            ref_matches("docker.io/library/postgres:16", "postgres:16"),
            "bare tag matches its registry-qualified inspect ref"
        );
        assert!(
            ref_matches(
                "docker.io/library/postgres:16@sha256:deadbeef",
                "postgres:16"
            ),
            "bare tag matches its registry- and digest-qualified inspect ref"
        );
        // ...but two DIFFERENT registries/namespaces for the same bare
        // name:tag must not collapse into a match.
        assert!(
            !ref_matches("ghcr.io/myorg/postgres:16", "docker.io/library/postgres:16"),
            "different registries for the same name:tag must not match"
        );
        assert!(
            !ref_matches("xpostgres:16", "postgres:16"),
            "a same-suffix-but-no-slash-boundary string must not match"
        );

        // Pins the inherent limitation documented on `ref_matches`.
        assert!(
            ref_matches("postgres:16", "ghcr.io/myorg/postgres:16"),
            "inherent limitation: a bare ref matches any qualification of the same name:tag"
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
            should_prune("pall8t-x:501-20-oldhash654321", keep_tag, base, 501, 20),
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

        let pruned = filter_prunable(refs.iter().copied(), base, keep_tag, 501, 20, &[]);
        assert_eq!(
            pruned,
            vec![old.to_string()],
            "keeps keep_tag and the other gid's image, dedupes the qualified duplicate"
        );

        let none_in_use = filter_prunable(
            refs.iter().copied(),
            base,
            keep_tag,
            501,
            20,
            &[old.to_string()],
        );
        assert!(
            none_in_use.is_empty(),
            "an in-use image must not be pruned even if it's a superseded sibling"
        );

        let qualified_in_use = filter_prunable(
            refs.iter().copied(),
            base,
            keep_tag,
            501,
            20,
            &[format!("localhost/{old}@sha256:deadbeef")],
        );
        assert!(
            qualified_in_use.is_empty(),
            "in-use matching must be qualification/digest-aware"
        );
    }

    #[test]
    fn run_argv_shape() {
        let spec = RunSpec {
            name: "pall8t-x-abc12345-99".into(),
            image: "pall8t-x:501-20-abc123456789".into(),
            workdir: PathBuf::from("/Users/me/src/x"),
            mounts: vec![
                Mount::identity(PathBuf::from("/Users/me/src/x")),
                Mount {
                    host: PathBuf::from("/Users/me/.pall8t/home"),
                    dest: PathBuf::from("/home/dev"),
                },
            ],
            cpus: 4,
            memory: "8g".into(),
            uid: 501,
            gid: 20,
            tty: true,
            command: vec!["claude".into()],
        };
        let argv = run_argv(&spec);
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"-i".to_string()));
        assert!(argv.contains(&"-t".to_string()), "tty: true requests -t");
        assert!(argv.contains(&"--rm".to_string()));
        assert!(argv.contains(&"/Users/me/src/x:/Users/me/src/x".to_string()));
        assert!(argv.contains(&"/Users/me/.pall8t/home:/home/dev".to_string()));
        assert_eq!(
            argv.last(),
            Some(&"claude".to_string()),
            "the command comes after the image"
        );
        let image_pos = argv
            .iter()
            .position(|a| a == "pall8t-x:501-20-abc123456789")
            .unwrap();
        assert_eq!(image_pos, argv.len() - 2);

        let scripted = run_argv(&RunSpec { tty: false, ..spec });
        assert!(
            !scripted.contains(&"-t".to_string()),
            "no -t without a terminal — apple/container 1.0.0 fails on -t sans TTY"
        );
        assert!(scripted.contains(&"-i".to_string()));
    }

    #[test]
    fn exec_argv_shape() {
        let cmd = vec!["git".to_string(), "status".to_string()];

        let tty = exec_argv("pall8t-x-abc12345-99", &cmd, true, Some("/Users/me/src/x"));
        assert_eq!(tty[0], "exec");
        assert!(tty.contains(&"-i".to_string()));
        assert!(tty.contains(&"-t".to_string()));
        let w = tty.iter().position(|a| a == "-w").unwrap();
        assert_eq!(tty[w + 1], "/Users/me/src/x");
        assert_eq!(tty.last(), Some(&"status".to_string()));
        assert_eq!(
            tty.iter()
                .position(|a| a == "pall8t-x-abc12345-99")
                .unwrap(),
            tty.len() - 3,
            "the command follows the container name"
        );

        let scripted = exec_argv("pall8t-x-abc12345-99", &cmd, false, None);
        assert!(
            !scripted.contains(&"-t".to_string()),
            "no -t without a terminal"
        );
        assert!(
            !scripted.contains(&"-w".to_string()),
            "unknown workdir is omitted"
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
