use anyhow::{Context, Result};
use serde_json::Value;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// pall8t-<path key of cwd>-<pid> (see [`crate::util::path_key`]). The
/// pid keeps parallel runs from the same directory from colliding on
/// `--name`.
pub fn run_name(workspace: &Path) -> String {
    format!(
        "pall8t-{}-{}",
        crate::util::path_key(workspace),
        std::process::id()
    )
}

pub fn image_tag(base: &str, uid: u32, gid: u32) -> String {
    format!("{base}:{uid}-{gid}")
}

/// Like [`image_tag`], but suffixed with a content hash (see
/// [`crate::image::combined_hash`]) of the Containerfile — and, if
/// `container.watch` lists any, those files too — so a change to any of
/// their contents resolves to a new tag. Hashing the working-tree contents
/// (rather than, say, the last commit that touched a file) means
/// uncommitted edits are detected too, and a rebuild can never poison a
/// tag: the same content always resolves to the same tag, so the tag
/// always corresponds to the image built from it.
pub fn image_tag_hashed(base: &str, uid: u32, gid: u32, hash: &str) -> String {
    format!("{base}:{uid}-{gid}-{hash}")
}

fn run_ok<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();
    crate::util::run_ok("container", &argv)
}

fn run_streaming<I, S>(args: I, stdin: Stdio) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let argv: Vec<String> = args.into_iter().map(Into::into).collect();
    crate::util::run_streaming("container", &argv, stdin)
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

/// A parsed `major.minor.patch` from apple/container's version banner.
type Version = (u32, u32, u32);

/// Oldest apple/container pall8t asks for. 1.2.0 is where
/// apple/container#2027 fixed `Parser.allEnv`: before it, a bare env name
/// (no `=`) in an *image config* was expanded from the host process's
/// environment and injected into the container. That is precisely the
/// boundary [`RunSpec::env`] promises — pall8t forwards nothing from the
/// host by default — so on an older runtime a base image could quietly
/// pull a host value (a token, a path, anything) into the sandbox, and no
/// amount of care on pall8t's side would stop it.
const MIN_VERSION: Version = (1, 2, 0);

/// `major.minor.patch` out of a `container --version` banner, or `None`
/// when no token in it looks like a version. Scans tokens rather than
/// matching the banner's wording, which is not a stable interface
/// (ADR-0001) — the version is the first dotted numeric triple, and
/// anything after the patch number (`-beta`, `_1`) is ignored.
fn parse_cli_version(stdout: &str) -> Option<Version> {
    stdout.split_whitespace().find_map(|token| {
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts
            .next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        Some((major, minor, patch))
    })
}

/// The warning to print for an installed version, or `None` when there is
/// nothing to say.
///
/// An unreadable banner says nothing on purpose. pall8t cannot tell "very
/// old" from "newer than this build knows about" once parsing fails, and a
/// false alarm about a security boundary is worse than silence: a warning
/// users learn to scroll past stops working on the day it is right.
fn version_warning(installed: Option<Version>) -> Option<String> {
    let v = installed?;
    if v >= MIN_VERSION {
        return None;
    }
    let (major, minor, patch) = v;
    let (min_major, min_minor, min_patch) = MIN_VERSION;
    Some(format!(
        "pall8t: warning: apple/container {major}.{minor}.{patch} is older than \
         {min_major}.{min_minor}.{min_patch}. On this version an image that \
         declares a bare `ENV NAME` (no value) has it filled in from *your* \
         host environment and injected into the container \
         (apple/container#2027), so the sandbox can receive host values pall8t \
         never passed it. Upgrade to keep the boundary pall8t documents."
    ))
}

/// One `container --version` probe, parsed by [`parse_cli_version`].
/// Failure to spawn or read is indistinguishable from an unparseable
/// banner here, and both mean the same thing to the caller: nothing
/// trustworthy to say about the version.
///
/// Both streams are scanned. 1.2.2 prints the banner on stdout (verified),
/// but the CLI's output shapes are pre-1.0 (ADR-0001) and a build that
/// moved it to stderr would silently switch this warning off forever —
/// the one failure mode nobody would notice, since its whole symptom is
/// the absence of a message.
pub fn cli_version() -> Option<Version> {
    let out = Command::new("container").arg("--version").output().ok()?;
    parse_cli_version(&String::from_utf8_lossy(&out.stdout))
        .or_else(|| parse_cli_version(&String::from_utf8_lossy(&out.stderr)))
}

/// The version warning for the installed CLI, if it earns one. One extra
/// subprocess on the startup path (NFR-1) — paid once per invocation,
/// alongside the `container system status` probe that already runs there,
/// and only worth it because the thing it warns about is silent by
/// construction: nothing in a run's output would ever reveal that a host
/// value leaked in.
pub fn version_warning_for_installed() -> Option<String> {
    version_warning(cli_version())
}

/// Starts the apple/container system service (`container system start`).
/// Streams its progress live to stderr rather than inheriting stdout
/// outright — `ensure_container_system` calls this from `pall8t ls`'s path
/// too, and `ls --json`'s stdout must stay clean JSON even on a first run
/// right after a reboot, when the service is still stopped. Stdin is
/// inherited only when it's actually a TTY (unlike [`build_image`]'s use
/// of the same [`run_streaming`], which always closes it): on a fresh
/// machine this can prompt for the default-kernel install, and a TTY is
/// the only case where that prompt is answerable — piping pall8t's own
/// stdin through unconditionally would instead hand the prompt someone
/// else's data (e.g. `echo prompt | pall8t run`), so a non-TTY stdin stays
/// closed exactly like `build_image`'s.
pub fn system_start() -> Result<()> {
    let stdin = if std::io::stdin().is_terminal() {
        Stdio::inherit()
    } else {
        Stdio::null()
    };
    run_streaming(["system", "start"], stdin)
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

/// Runs `container build`, streaming its output to stderr live (issue #13:
/// a silent multi-minute build looks hung). Unlike most calls in this
/// module, this doesn't go through [`run_ok`] — there's nothing here to
/// parse, only progress to show. Stdin is always closed: a build has no
/// business reading it (contrast [`system_start`], the other
/// [`run_streaming`] caller, which conditionally inherits it).
pub fn build_image(
    containerfile: &Path,
    ctx_dir: &Path,
    tag: &str,
    uid: u32,
    gid: u32,
    no_cache: bool,
) -> Result<()> {
    run_streaming(
        build_argv(containerfile, ctx_dir, tag, uid, gid, no_cache),
        Stdio::null(),
    )?;
    Ok(())
}

/// argv (after `container`) for `container build`. `no_cache` forwards
/// `--no-cache` to the builder, re-running every `RUN` step — the escape
/// hatch for steps that fetch "latest" (an npm install, `apt-get update`),
/// which an unchanged instruction line would otherwise serve from the
/// layer cache forever. Flags stay ahead of the positional context dir.
pub fn build_argv(
    containerfile: &Path,
    ctx_dir: &Path,
    tag: &str,
    uid: u32,
    gid: u32,
    no_cache: bool,
) -> Vec<String> {
    let mut argv = vec![
        "build".to_string(),
        "-f".to_string(),
        containerfile.to_string_lossy().into_owned(),
        "-t".to_string(),
        tag.to_string(),
        "--build-arg".to_string(),
        format!("UID={uid}"),
        "--build-arg".to_string(),
        format!("GID={gid}"),
    ];
    if no_cache {
        argv.push("--no-cache".to_string());
    }
    argv.push(ctx_dir.to_string_lossy().into_owned());
    argv
}

/// One mount of a [`RunSpec`], rendered by [`run_argv`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub dest: PathBuf,
    /// Mount it read-only. Enforced by the guest kernel — a write inside
    /// the container fails with `EROFS` — verified on apple/container
    /// 1.2.2 and pinned by ADR-0009. Meaningless for [`Kind::Socket`],
    /// which forwards connections rather than mounting a filesystem.
    pub readonly: bool,
    pub kind: Kind,
}

/// What the runtime does with a mount source. Directories become
/// filesystems; a Unix socket becomes a forwarded socket the guest can
/// connect to — the herdr bridge's transport (ADR-0007 amendment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Directory,
    Socket,
}

impl Mount {
    /// Explicit constructor for callers that already know all three.
    pub fn new(host: PathBuf, dest: PathBuf, readonly: bool) -> Self {
        Mount {
            host,
            dest,
            readonly,
            kind: Kind::Directory,
        }
    }

    /// Forwards the host Unix socket at `host` into the container at
    /// `dest`, where it is a live socket the guest can connect to — not a
    /// virtiofs view of it (connecting through virtiofs is what fails
    /// with `ENOTSUP`). Emitted as `-v`, see [`Mount::spec`].
    ///
    /// Fails when either path contains a `:`. That is what keeps
    /// [`Mount::spec`]'s "no third field" claim true: `-v` splits on `:`,
    /// so a single colon in the host path would hand the *container* path
    /// to apple/container as unvalidated mount options, and a second one
    /// would fail the whole `container run` rather than just the bridge.
    /// A `:` is legal in a macOS path, so this is a guard, not a
    /// formality — the caller is best-effort and degrades to "no bridge"
    /// with the reason.
    pub fn socket(host: PathBuf, dest: PathBuf) -> Result<Self> {
        for path in [&host, &dest] {
            if path.to_string_lossy().contains(':') {
                anyhow::bail!(
                    "a forwarded socket path may not contain `:` ({}) — `-v` \
                     splits on it, and the third field is parsed as mount options",
                    path.display()
                );
            }
        }
        Ok(Mount {
            host,
            dest,
            readonly: false,
            kind: Kind::Socket,
        })
    }

    /// Identity-path mount, writable: `host` is visible at the same
    /// absolute path inside the container, so git metadata and path
    /// references stay valid on both sides (ADR-0004's insight, retained
    /// by ADR-0006).
    pub fn identity(path: PathBuf) -> Self {
        Mount {
            host: path.clone(),
            dest: path,
            readonly: false,
            kind: Kind::Directory,
        }
    }

    /// Writable mount of `host` at `dest`.
    pub fn rw(host: PathBuf, dest: PathBuf) -> Self {
        Mount {
            host,
            dest,
            readonly: false,
            kind: Kind::Directory,
        }
    }

    /// Read-only mount of `host` at `dest` — the sandbox can read it and
    /// cannot change it (ADR-0009).
    pub fn ro(host: PathBuf, dest: PathBuf) -> Self {
        Mount {
            host,
            dest,
            readonly: true,
            kind: Kind::Directory,
        }
    }

    /// The flag and value this mount goes out as.
    ///
    /// A directory is `--mount`, not `-v host:dest[:opts]`, and that is a
    /// safety choice rather than a style one: apple/container validates
    /// `--mount` directives and rejects an unknown one outright, while
    /// `-v` passes its third field through to the filesystem options
    /// unchecked. On 1.2.2, `-v src:dst:readonlyy` mounts **read-write in
    /// silence**, where `--mount …,readonlyy` fails the run before it
    /// starts. A typo in a protection flag must not be the quiet outcome
    /// (ADR-0009).
    ///
    /// A socket has to be `-v` anyway: 1.2.2's `--mount` parser accepts
    /// only a directory source (`path '…' is not a directory`), while the
    /// runtime behind both spellings forwards a socket source into the
    /// guest. `-v` is the one form that reaches it. The ADR-0009 hazard
    /// doesn't ride along, because this form has no third field to
    /// mistype — `readonly` is meaningless for a forwarded socket, and
    /// [`Mount::socket`] is the only constructor that produces one.
    /// (Tracked upstream in TakiTake/pall8t#52; if apple/container lifts
    /// the parser restriction, this collapses back to `--mount`.)
    fn spec(&self) -> (&'static str, String) {
        match self.kind {
            Kind::Socket => (
                "-v",
                format!("{}:{}", self.host.display(), self.dest.display()),
            ),
            Kind::Directory => {
                let mut s = format!(
                    "type=virtiofs,source={},target={}",
                    self.host.display(),
                    self.dest.display()
                );
                if self.readonly {
                    s.push_str(",ro");
                }
                ("--mount", s)
            }
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
    /// `-e KEY=VALUE` environment for the container process. pall8t
    /// forwards nothing from the host environment by default; the only
    /// producer today is the herdr bridge (`HERDR_*` identity — see
    /// [`crate::herdr`]).
    pub env: Vec<(String, String)>,
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
        let (flag, value) = m.spec();
        argv.push(flag.into());
        argv.push(value);
    }
    for (k, v) in &spec.env {
        argv.push("-e".into());
        argv.push(format!("{k}={v}"));
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

/// What to exec for the `container` CLI when argv[0] matters, plus env
/// assignments to carry over. Homebrew installs `container` as a bash
/// wrapper (`VAR="…" exec "<cellar>/libexec/container" "$@"`), and that
/// inner `exec` rewrites argv[0] to the full target path — silently
/// destroying the herdr agent hint one hop after pall8t set it (observed
/// live: `ps` shows the Cellar path, herdr never identifies the pane). So
/// when the PATH-resolved `container` is such a wrapper, exec its target
/// binary directly, keeping the wrapper's env assignments
/// (`CONTAINER_INSTALL_ROOT` — the CLI locates its helpers through it).
/// Anything unparseable falls back to plain `container`: argv[0] is then
/// lost, but the run still works.
pub fn client_exec_target() -> (PathBuf, Vec<(String, String)>) {
    let fallback = (PathBuf::from("container"), Vec::new());
    let Some(path) = find_in_path("container") else {
        return fallback;
    };
    // Bounded read: a direct-binary install puts a multi-megabyte Mach-O
    // here, and a wrapper script fits in the first kilobyte.
    let Some(head) = read_head_utf8(&path, 1024) else {
        return fallback;
    };
    match parse_exec_wrapper(&head) {
        Some((target, env)) if is_executable_file(Path::new(&target)) => {
            (PathBuf::from(target), env)
        }
        _ => fallback,
    }
}

/// First `limit` bytes of `path`, lossily decoded. Binary content (a
/// Mach-O) decodes to replacement-character soup that
/// [`parse_exec_wrapper`] rejects at its `#!` check, so no UTF-8
/// validation is needed here.
fn read_head_utf8(path: &Path, limit: u64) -> Option<String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(limit)
        .read_to_end(&mut buf)
        .ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn find_in_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(bin))
        .find(|p| is_executable_file(p))
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .is_ok_and(|x| x)
}

/// Parses a Homebrew-style exec wrapper script: a `#!` script whose one
/// action line is `[VAR=VAL ...] exec "<target>" "$@"`. Returns the target
/// and the env assignments, or `None` for anything else (including a real
/// binary, whose content won't start with `#!`). Deliberately strict — a
/// shape this parser doesn't recognize must fall back to the wrapper
/// itself rather than exec a misparsed path.
fn parse_exec_wrapper(script: &str) -> Option<(String, Vec<(String, String)>)> {
    script.strip_prefix("#!")?;
    for line in script.lines().skip(1) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut tokens = shell_tokens(line).into_iter();
        let mut env = Vec::new();
        let mut token = tokens.next()?;
        while token != "exec" {
            let (var, val) = token.split_once('=')?;
            if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return None;
            }
            env.push((var.to_string(), val.to_string()));
            token = tokens.next()?;
        }
        let target = tokens.next()?;
        // "$@" must follow, or the wrapper isn't a pure pass-through and
        // exec'ing its target directly would change behavior.
        if tokens.next().as_deref() != Some("$@") || tokens.next().is_some() {
            return None;
        }
        return Some((target, env));
    }
    None
}

/// Whitespace tokenizer with double-quote grouping — just enough for the
/// one-line Homebrew wrapper shape; no escapes, no single quotes.
fn shell_tokens(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
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

    /// apple/container's own rule, transcribed from
    /// `ManagedContainer.nameValid` (Sources/ContainerResource/Container/
    /// ManagedContainer.swift, 1.2.x):
    ///
    /// ```swift
    /// guard name.count <= 63 else { return false }   // DNS label length
    /// let pattern = #"^[a-zA-Z0-9][a-zA-Z0-9_.-]+$"#
    /// ```
    ///
    /// `container run` checks this client-side before it reaches the API
    /// server, so a name that fails here fails the run outright with
    /// "container ID ... is not a valid container ID". A test oracle, not
    /// production logic — pall8t constructs names that satisfy it rather
    /// than validating them.
    fn upstream_name_valid(name: &str) -> bool {
        let mut chars = name.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        // `[...]+` after the leading class: a one-character name never matches.
        let rest: Vec<char> = chars.collect();
        name.chars().count() <= 63
            && first.is_ascii_alphanumeric()
            && !rest.is_empty()
            && rest
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    }

    /// Regression pin for the 1.2.0 name cap: a workspace whose basename is
    /// far longer than the whole budget still has to produce a runnable
    /// `--name`. Before the [`crate::util`] slug cap this ran on 1.0.0 and
    /// died on 1.2.0.
    #[test]
    fn run_name_stays_within_the_container_name_cap() {
        let long = "a-very-long-workspace-directory-name-".repeat(6);
        for (label, path) in [
            (
                "a pathological basename",
                PathBuf::from("/Users/me").join(&long),
            ),
            ("an ordinary one", PathBuf::from("/Users/me/src/pall8t")),
            (
                "a basename that is all separators",
                PathBuf::from("/Users/me").join("-".repeat(80)),
            ),
        ] {
            let name = run_name(&path);
            assert!(
                name.chars().count() <= 63,
                "{label}: apple/container 1.2.0 rejects a name over 63 chars \
                 (ManagedContainer.nameValid), and `container run` checks it \
                 before launching, so an over-long name is a dead run: \
                 {name:?} is {} chars",
                name.chars().count()
            );
            assert!(
                upstream_name_valid(&name),
                "{label}: {name:?} must satisfy apple/container's whole name rule, \
                 not just its length"
            );
        }
    }

    #[test]
    fn parse_cli_version_table() {
        // Verbatim from a live Homebrew install of apple/container 1.2.2
        // (the truncated `commit: unspeci` is what the binary prints).
        assert_eq!(
            parse_cli_version("container CLI version 1.2.2 (build: release, commit: unspeci)\n"),
            Some((1, 2, 2)),
            "the shipped banner is the shape that has to work"
        );
        assert_eq!(
            parse_cli_version("container CLI version 1.0.0 (build: release, commit: abc1234)\n"),
            Some((1, 0, 0))
        );
        assert_eq!(
            parse_cli_version("container CLI version 1.2.0-beta.1 (build: dev)\n"),
            Some((1, 2, 0)),
            "a pre-release suffix must not defeat the parse — it would make an \
             unreadable banner out of a version pall8t can compare"
        );
        assert_eq!(
            parse_cli_version("container CLI version 10.11.12\n"),
            Some((10, 11, 12)),
            "multi-digit components are not the single-character case"
        );
        assert_eq!(
            parse_cli_version("some preamble\ncontainer CLI version 1.2.2\n"),
            Some((1, 2, 2)),
            "the banner need not be the first line — `cli_version` hands over \
             whichever stream carried it, preamble and all"
        );
        for junk in [
            "",
            "container: command not found\n",
            "container CLI version unknown\n",
            "container CLI version 1.2\n",
        ] {
            assert_eq!(
                parse_cli_version(junk),
                None,
                "nothing in {junk:?} is a version triple, and guessing one would \
                 produce a warning about a version nobody is running"
            );
        }
    }

    #[test]
    fn version_warning_table() {
        assert!(
            version_warning(Some((1, 0, 0)))
                .is_some_and(|m| m.contains("2027") && m.contains("1.0.0")),
            "an older runtime must be named in the warning, with the upstream \
             fix to look up — a bare 'please upgrade' is unactionable"
        );
        assert!(
            version_warning(Some((1, 1, 9))).is_some(),
            "1.1.9 precedes 1.2.0: comparison is per component, not lexical"
        );
        for ok in [MIN_VERSION, (1, 2, 2), (2, 0, 0)] {
            assert_eq!(
                version_warning(Some(ok)),
                None,
                "{ok:?} carries the env fix, so there is nothing to warn about"
            );
        }
        assert_eq!(
            version_warning(None),
            None,
            "an unreadable banner is as likely to be a future format as an \
             ancient build; crying wolf about a security boundary is what \
             teaches users to ignore it"
        );
    }

    #[test]
    fn parse_exec_wrapper_homebrew_shape() {
        // Verbatim from a live Homebrew install of apple/container 1.0.0_1.
        let script = "#!/bin/bash\nCONTAINER_INSTALL_ROOT=\"/opt/homebrew/opt/container\" exec \"/opt/homebrew/Cellar/container/1.0.0_1/libexec/container\"  \"$@\"\n";
        let (target, env) = parse_exec_wrapper(script).unwrap();
        assert_eq!(
            target,
            "/opt/homebrew/Cellar/container/1.0.0_1/libexec/container"
        );
        assert_eq!(
            env,
            vec![(
                "CONTAINER_INSTALL_ROOT".to_string(),
                "/opt/homebrew/opt/container".to_string()
            )]
        );
    }

    #[test]
    fn parse_exec_wrapper_no_assignments() {
        let script = "#!/bin/sh\nexec \"/usr/local/libexec/container\" \"$@\"\n";
        let (target, env) = parse_exec_wrapper(script).unwrap();
        assert_eq!(target, "/usr/local/libexec/container");
        assert!(env.is_empty());
    }

    #[test]
    fn parse_exec_wrapper_rejects_non_passthrough_shapes() {
        // A real Mach-O/ELF binary: no shebang.
        assert_eq!(parse_exec_wrapper("\u{7f}ELF\u{2}..."), None);
        // Extra args after "$@" — not a pure pass-through.
        assert_eq!(
            parse_exec_wrapper("#!/bin/sh\nexec \"/x/container\" \"$@\" --extra\n"),
            None,
            "a wrapper that adds arguments must be exec'd as-is"
        );
        // Arbitrary logic before the exec token.
        assert_eq!(
            parse_exec_wrapper("#!/bin/sh\nif true; then exec \"/x/container\" \"$@\"; fi\n"),
            None
        );
        // A target using a shell variable parses to the literal string —
        // the parser is syntax-only. client_exec_target's
        // is_executable_file guard is what rejects it (no such path), so
        // this pins the contract that the guard is load-bearing.
        let (target, _) =
            parse_exec_wrapper("#!/bin/sh\nROOT=\"/opt/x\" exec \"$ROOT/container\" \"$@\"\n")
                .unwrap();
        assert_eq!(target, "$ROOT/container");
        assert!(!is_executable_file(Path::new(&target)));
        // Missing "$@" entirely.
        assert_eq!(
            parse_exec_wrapper("#!/bin/sh\nexec \"/x/container\"\n"),
            None
        );
    }

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

    /// The herdr bridge's socket is the one mount that must go out as
    /// `-v`: 1.2.2's `--mount` parser accepts only a directory source,
    /// while the runtime behind `-v` forwards a socket source into the
    /// guest as a live socket (verified on 1.2.2). Two colon-separated
    /// fields and no third — the unvalidated-options hazard ADR-0009
    /// names has nothing to ride on here.
    #[test]
    fn socket_mount_goes_out_as_two_field_v() {
        let spec = RunSpec {
            name: "pall8t-x-abc12345-99".into(),
            image: "img".into(),
            workdir: PathBuf::from("/Users/me/src/x"),
            mounts: vec![
                Mount::identity(PathBuf::from("/Users/me/src/x")),
                Mount::socket(
                    PathBuf::from("/Users/me/.pall8t/run/pall8t-x-abc12345-99.sock"),
                    PathBuf::from("/tmp/pall8t/herdr.sock"),
                )
                .unwrap(),
            ],
            cpus: 4,
            memory: "8g".into(),
            uid: 501,
            gid: 20,
            tty: false,
            env: vec![],
            command: vec!["claude".into()],
        };
        let argv = run_argv(&spec);
        let v = argv
            .iter()
            .position(|a| a == "-v")
            .expect("the socket mount emits -v");
        assert_eq!(
            argv[v + 1],
            "/Users/me/.pall8t/run/pall8t-x-abc12345-99.sock:/tmp/pall8t/herdr.sock",
            "host:container, and nothing else — a third field would be passed to \
             the filesystem options unchecked (ADR-0009)"
        );
        assert_eq!(
            argv[v + 1].matches(':').count(),
            1,
            "exactly one separator: no options field can exist to be mistyped"
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "-v").count(),
            1,
            "only the socket mount uses -v; the directory mount stays on --mount"
        );
        assert!(
            argv.contains(
                &"type=virtiofs,source=/Users/me/src/x,target=/Users/me/src/x".to_string()
            ),
            "a directory mount is unaffected by a socket mount sharing the run"
        );
    }

    /// `-v` splits on `:`, and apple/container hands the third field to
    /// the filesystem options unchecked (1.2.2 `Parser.volume`). A macOS
    /// path may contain a colon, so the "no third field to mistype"
    /// property `Mount::spec` claims has to be enforced at construction —
    /// otherwise a home directory with a colon in it silently turns the
    /// container path into mount options.
    #[test]
    fn socket_mount_refuses_a_colon_in_either_path() {
        assert!(
            Mount::socket(
                PathBuf::from("/Users/me/.pall8t/run/x.sock"),
                PathBuf::from("/tmp/pall8t/herdr.sock"),
            )
            .is_ok(),
            "the ordinary case is unaffected"
        );
        assert!(
            Mount::socket(
                PathBuf::from("/Users/od:d/.pall8t/run/x.sock"),
                PathBuf::from("/tmp/pall8t/herdr.sock"),
            )
            .is_err(),
            "a colon in the host path would make the container path parse as \
             mount options — the bridge must decline instead"
        );
        assert!(
            Mount::socket(
                PathBuf::from("/Users/me/.pall8t/run/x.sock"),
                PathBuf::from("/tmp/pall8t/her:dr.sock"),
            )
            .is_err(),
            "the container side splits the same way"
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
                Mount::rw(
                    PathBuf::from("/Users/me/.pall8t/home"),
                    PathBuf::from("/home/dev"),
                ),
                Mount::ro(
                    PathBuf::from("/Users/me/src/lib"),
                    PathBuf::from("/Users/me/src/lib"),
                ),
            ],
            cpus: 4,
            memory: "8g".into(),
            uid: 501,
            gid: 20,
            tty: true,
            env: vec![("HERDR_ENV".into(), "1".into())],
            command: vec!["claude".into()],
        };
        let argv = run_argv(&spec);
        let e = argv.iter().position(|a| a == "-e").unwrap();
        assert_eq!(
            argv[e + 1],
            "HERDR_ENV=1",
            "env entries emit as -e KEY=VALUE"
        );
        assert_eq!(argv[0], "run");
        assert!(argv.contains(&"-i".to_string()));
        assert!(argv.contains(&"-t".to_string()), "tty: true requests -t");
        assert!(argv.contains(&"--rm".to_string()));
        // `--mount`, not `-v`: apple/container validates these directives and
        // rejects an unknown one, where `-v src:dst:<opts>` passes the
        // options through unchecked — on 1.2.2 a typo'd `-v src:dst:readonlyy`
        // mounts read-write in silence (ADR-0009). The exact strings are the
        // contract with another process, so they are asserted, not summarized.
        assert!(argv
            .contains(&"type=virtiofs,source=/Users/me/src/x,target=/Users/me/src/x".to_string()));
        assert!(argv
            .contains(&"type=virtiofs,source=/Users/me/.pall8t/home,target=/home/dev".to_string()));
        assert!(
            argv.contains(
                &"type=virtiofs,source=/Users/me/src/lib,target=/Users/me/src/lib,ro".to_string()
            ),
            "a read-only mount carries `,ro` — without it the mount is silently \
             writable, which is the whole failure this flag exists to prevent"
        );
        assert!(
            !argv.contains(&"-v".to_string()),
            "a directory mount may never go out as `-v`: its options are unvalidated"
        );
        assert_eq!(
            argv.iter().filter(|a| *a == "--mount").count(),
            3,
            "every directory mount emits exactly one --mount flag"
        );
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
    fn build_argv_shape() {
        let cf = PathBuf::from("/Users/me/src/x/.pall8t/Containerfile");
        let ctx = PathBuf::from("/Users/me/src/x");
        let cached = build_argv(&cf, &ctx, "pall8t-x:501-20-abc123456789", 501, 20, false);
        assert_eq!(cached[0], "build");
        let f = cached.iter().position(|a| a == "-f").unwrap();
        assert_eq!(
            cached[f + 1],
            "/Users/me/src/x/.pall8t/Containerfile",
            "the Containerfile path rides -f; the context dir is positional (ADR-0010 \
             decides them independently, so they must not be conflated)"
        );
        assert_eq!(
            cached.last(),
            Some(&"/Users/me/src/x".to_string()),
            "the build context is the trailing positional argument"
        );
        assert!(
            cached.contains(&"UID=501".to_string()) && cached.contains(&"GID=20".to_string()),
            "host ids reach the build as --build-arg so the image's dev user matches \
             the mounted workspace's owner"
        );
        assert!(
            !cached.contains(&"--no-cache".to_string()),
            "a default build must NOT pass --no-cache — the layer cache is what \
             keeps routine rebuilds fast"
        );

        let uncached = build_argv(&cf, &ctx, "pall8t-x:501-20-abc123456789", 501, 20, true);
        let pos = uncached
            .iter()
            .position(|a| a == "--no-cache")
            .expect("no_cache: true must emit --no-cache — it is the whole contract of `pall8t build --no-cache`");
        assert!(
            pos < uncached.len() - 1,
            "--no-cache must precede the positional context dir, or the CLI parses \
             it as part of the context path"
        );
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
}
