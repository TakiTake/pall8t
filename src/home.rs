//! Agent-home state compositor (spec: `docs/specs/home-compositor.md`).
//!
//! In `shared` mode nothing here runs — `pall8t run` mounts
//! `~/.pall8t/home` rw exactly as before. In `isolated` mode each run gets
//! a private, instantly-forked copy of that base home; when the run ends,
//! its non-ephemeral changes are harvested (lazily, on a later invocation)
//! into an inbox changeset, and the user folds selected changes back into
//! the base with `pall8t home promote`.
//!
//! Layout under `~/.pall8t` (siblings of `home/`, so none of this is
//! visible to the agent inside its `$HOME`):
//!
//! ```text
//! home/                     the base — a valid, mountable $HOME at every instant
//! home.lock                 per-base advisory lock (FR-6)
//! instances/<run>/          a live/finished fork
//!   root/                   mounted as /home/dev; the run writes here
//!   ancestor/               base snapshot at fork time — the 3-way merge base
//!   meta.toml               run name, workspace, fork time, forker pid
//! inbox/<run>/              one harvested changeset
//!   manifest.toml           entries: path, class, change
//!   theirs/<rel>            the run's version of each staged (knowledge) path
//!   ancestor/<rel>          the fork-point version of each staged path
//! revisions/<seq>/          one base mutation (FR-7), seq zero-padded, oldest first
//!   snapshot/               the base as it was BEFORE this mutation
//!   meta.toml               seq, op (harvest|promote|rollback), run(s), paths, time
//! ```
//!
//! Secrets and durable state never enter the inbox: they are written back
//! to the base at harvest (latest-wins / key-path merge). Only `knowledge`
//! and unclassified paths are staged for explicit promotion (FR-2 table).
//!
//! No revision holds secret content by omission, not filtering: a
//! revision's `snapshot/` is a full `clone_tree` of the base, which DOES
//! include secret files (they live in the base like everything else) — but
//! [`diff`] never renders their bytes, only a redaction marker, and nothing
//! in this module ships a snapshot anywhere off-host. This mirrors the
//! spec's stance that secrets are host-side-only, never leaked to an agent
//! or a diff/log, rather than excluded from the base itself.

use crate::config::PolicyRule;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// How the container home is materialized for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HomeMode {
    /// Today's behavior: mount `~/.pall8t/home` rw. The default.
    #[default]
    Shared,
    /// Fork the base per run, harvest into an inbox, promote explicitly.
    Isolated,
}

/// Path classification driving fork behavior and harvest disposition
/// (spec FR-2 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Credentials — written back to the base latest-wins; never staged,
    /// never in diffs/logs.
    Secret,
    /// Durable structured state (`.claude.json`) — key-path JSON merged
    /// into the base at harvest.
    State,
    /// Skills, memory, `CLAUDE.md`, settings — staged in the inbox, merged
    /// only on explicit promote.
    Knowledge,
    /// Caches, history, locks — discarded, never merged.
    Ephemeral,
}

/// Which text-merge algorithm a path uses, overriding the class default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategy {
    /// The class's built-in merge: key-path JSON for `state`, textual 3-way
    /// (conflict on overlap) for `knowledge`.
    #[default]
    Inherit,
    /// Line-level 3-way that keeps both sides' added lines, never conflicts
    /// (`git merge-file --union`). For append-only formats like
    /// `.claude/history.jsonl` where concatenation is always valid. Lines
    /// identical on both sides collapse to one (git's union behavior), which
    /// is why each history entry carries a unique id/timestamp.
    Union,
}

impl MergeStrategy {
    // `&self`, not `self`: serde's `skip_serializing_if` calls this by
    // reference, so the trivially-copy-by-ref suggestion doesn't apply.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn is_inherit(&self) -> bool {
        matches!(self, MergeStrategy::Inherit)
    }
}

/// A path's class, its merge strategy, and whether an explicit rule matched
/// it. An unclassified path is staged like `knowledge` (conservative
/// default: never silently dropped or leaked) but flagged so the gap is
/// visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub class: Class,
    pub strategy: MergeStrategy,
    pub explicit: bool,
}

/// What a run did to a staged path, relative to its fork-point ancestor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    Added,
    Modified,
    Deleted,
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Built-in classification for Claude Code homes, embedded at compile time
/// from `rules/claude.toml` so the rule set is reviewed and edited as
/// data, not Rust code. The file uses the same rule format as a project's
/// `[[home.policy]]` and documents its own ordering constraints (first
/// match wins). Parsed once, on first use; the unit tests reject an
/// embedded file that fails to parse or bends the rules the comments in it
/// promise (see `tests::embedded_claude_policy_is_well_formed`).
pub fn default_rules() -> &'static [PolicyRule] {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PolicyFile {
        rules: Vec<PolicyRule>,
    }
    static RULES: std::sync::LazyLock<Vec<PolicyRule>> = std::sync::LazyLock::new(|| {
        let file: PolicyFile = toml::from_str(include_str!("../rules/claude.toml"))
            .expect("embedded rules/claude.toml must parse (pinned by unit test)");
        file.rules
    });
    &RULES
}

/// Classifies a `$HOME`-relative path. User `overrides` are tried first, then
/// the built-in [`default_rules`]; first glob match wins. No match ⇒
/// unclassified, which is staged like `knowledge` but reported.
pub fn classify(rel: &str, overrides: &[PolicyRule]) -> Classified {
    for rule in overrides.iter().chain(default_rules()) {
        if !glob_match(&rule.glob, rel) {
            continue;
        }
        let strategy = rule.strategy.unwrap_or(MergeStrategy::Inherit);
        let class = match (rule.class, strategy) {
            (Some(c), _) => c,
            // A strategy-only rule defaults its class to state (the user's
            // `strategy = "union"` intent for `.claude/history.jsonl`).
            (None, MergeStrategy::Union) => Class::State,
            // A rule with neither class nor strategy is meaningless (warned
            // about by `validate_policy`); fall through to the next rule.
            (None, MergeStrategy::Inherit) => continue,
        };
        // `union` is meaningless for secret/ephemeral (never text-merged);
        // ignore it there so a stray rule can't change their disposition.
        let strategy = match class {
            Class::Secret | Class::Ephemeral => MergeStrategy::Inherit,
            _ => strategy,
        };
        return Classified {
            class,
            strategy,
            explicit: true,
        };
    }
    Classified {
        class: Class::Knowledge,
        strategy: MergeStrategy::Inherit,
        explicit: false,
    }
}

/// User-facing warnings about `[[home.policy]]` rules that don't make sense
/// (emitted once by the CLI, not per path). None of these are fatal —
/// [`classify`] handles each defensively — but they signal a config the user
/// probably didn't intend.
pub fn validate_policy(overrides: &[PolicyRule]) -> Vec<String> {
    let mut warnings = Vec::new();
    for r in overrides {
        // A rule that forces no class and no effective strategy (absent, or an
        // explicit `inherit`) does nothing — classify falls through it.
        let effectively_empty =
            r.class.is_none() && matches!(r.strategy, None | Some(MergeStrategy::Inherit));
        match (r.class, r.strategy) {
            _ if effectively_empty => warnings.push(format!(
                "[[home.policy]] rule for glob \"{}\" sets neither class nor a meaningful strategy — ignored",
                r.glob
            )),
            (Some(c @ (Class::Secret | Class::Ephemeral)), Some(MergeStrategy::Union)) => warnings
                .push(format!(
                    "[[home.policy]] strategy=\"union\" is meaningless for {}-class glob \"{}\" — ignored",
                    class_label(c),
                    r.glob
                )),
            _ => {}
        }
    }
    warnings
}

/// Minimal path glob: `*` matches within one segment, `**` matches any run
/// of segments (including none). No new dependency — pall8t adds none
/// beyond git and the `container` CLI (spec NFR).
fn glob_match(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let text: Vec<&str> = path.split('/').collect();
    match_segments(&pat, &text)
}

fn match_segments(pat: &[&str], text: &[&str]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some((&"**", rest)) => (0..=text.len()).any(|i| match_segments(rest, &text[i..])),
        Some((&seg, rest)) => match text.split_first() {
            Some((first, trest)) => {
                wildcard(seg.as_bytes(), first.as_bytes()) && match_segments(rest, trest)
            }
            None => false,
        },
    }
}

/// Within-segment wildcard: `*` matches zero or more non-`/` characters.
fn wildcard(pat: &[u8], s: &[u8]) -> bool {
    match pat.split_first() {
        None => s.is_empty(),
        Some((b'*', rest)) => wildcard(rest, s) || (!s.is_empty() && wildcard(pat, &s[1..])),
        Some((&c, rest)) => !s.is_empty() && s[0] == c && wildcard(rest, &s[1..]),
    }
}

// ---------------------------------------------------------------------------
// Layout & per-base lock
// ---------------------------------------------------------------------------

fn base_dir(root: &Path) -> PathBuf {
    root.join("home")
}

fn instances_root(root: &Path) -> PathBuf {
    root.join("instances")
}

fn inbox_root(root: &Path) -> PathBuf {
    root.join("inbox")
}

fn revisions_root(root: &Path) -> PathBuf {
    root.join("revisions")
}

/// `[home] revisions_keep` default (FR-7): enough history to be useful,
/// bounded so an isolated-mode project doesn't accumulate snapshots forever.
pub const DEFAULT_REVISIONS_KEEP: u32 = 20;

/// `[home] inbox_ttl_days` default (FR-9): changesets older than this warn
/// (never auto-delete — dropping unreviewed knowledge is a user decision).
pub const DEFAULT_INBOX_TTL_DAYS: u32 = 14;

/// How old an instance's `running` status must be before `ls` flags it as
/// suspicious (a real run is a foreground `pall8t run` a human is watching;
/// nothing legitimate stays "running" this long — more likely the forker
/// pid was recycled by an unrelated process after the real run crashed).
const SUSPICIOUS_RUNNING_SECS: u64 = 24 * 3_600;

/// How old an instance `.partial` (fork interrupted before publish) must be,
/// judged by its own mtime, before gc treats it as abandoned rather than a
/// fork that is merely still in progress. `clone_tree` is O(ms) on APFS and
/// O(seconds) on the copy fallback even at the spec's 1 GB target, so
/// anything older than this never legitimately completes.
const STALE_PARTIAL_SECS: u64 = 3_600;

/// Held for the duration of any base mutation (fork snapshot, harvest
/// write-back, promote). A blocking `flock(2)` on `home.lock` (a sibling of
/// the base, not inside it) serializes concurrent harvests/promotes and is
/// released by the kernel on `kill -9` (FR-6). The lock file lives beside
/// the base so it is never visible inside the agent's `$HOME`.
pub struct BaseLock {
    _file: std::fs::File,
}

fn lock_base(root: &Path) -> Result<BaseLock> {
    std::fs::create_dir_all(root)?;
    let path = root.join("home.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("cannot open base lock {}", path.display()))?;
    // SAFETY: a valid fd from the File we own; flock has no other
    // precondition. LOCK_EX blocks until the lock is available.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(anyhow!(io::Error::last_os_error()).context("cannot acquire base lock"));
    }
    repair_base_swap(root).context("repairing an interrupted base swap")?;
    Ok(BaseLock { _file: file })
}

/// Repairs an interrupted [`swap_base`] (rollback's whole-base replace),
/// which crashes between its two renames as `<base>.partial` (the built
/// replacement, ready to publish) and `<base>.discard` (the retired old
/// base). Runs first thing under every base lock acquisition so every
/// base-touching operation always sees a consistent, present base — the
/// same "the base is a valid `$HOME` at every instant" invariant fork and
/// harvest already provide via their own temp+rename discipline.
fn repair_base_swap(root: &Path) -> Result<()> {
    let base = base_dir(root);
    let partial = sibling_suffix(&base, ".partial");
    let discard = sibling_suffix(&base, ".discard");
    if base.exists() {
        // The swap either never ran or completed; a stray partial/discard is
        // leftover cleanup from a completed swap that was interrupted after
        // publishing — safe to drop, nothing references it.
        let _ = std::fs::remove_dir_all(&partial);
        let _ = std::fs::remove_dir_all(&discard);
        return Ok(());
    }
    if partial.exists() {
        // Crashed after retiring the old base but before publishing the new
        // one: finish the publish.
        std::fs::rename(&partial, &base)
            .with_context(|| format!("cannot finish publishing base swap at {}", base.display()))?;
    } else if discard.exists() {
        // Crashed between the two renames is the only way to reach this
        // state with no partial; restore the retired base rather than leave
        // $HOME missing.
        std::fs::rename(&discard, &base)
            .with_context(|| format!("cannot restore base from {}", discard.display()))?;
    }
    // Neither exists: a fresh root — callers create the base lazily.
    Ok(())
}

/// Replaces the base's contents with `new_content` (a revision snapshot),
/// crash-atomically: build `<base>.partial` as a full clone first, then swap
/// by two renames (retire the live base to `<base>.discard`, publish the
/// clone). A crash between the renames leaves the base briefly missing on
/// disk, which [`repair_base_swap`] — run at the top of every subsequent
/// `lock_base` — resolves before any other base operation proceeds. Must be
/// called with the base lock held.
fn swap_base(root: &Path, new_content: &Path) -> Result<()> {
    let base = base_dir(root);
    let partial = sibling_suffix(&base, ".partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial)
            .with_context(|| format!("cannot clear stale swap partial {}", partial.display()))?;
    }
    clone_tree(new_content, &partial)?;
    let discard = sibling_suffix(&base, ".discard");
    if discard.exists() {
        std::fs::remove_dir_all(&discard)
            .with_context(|| format!("cannot clear stale swap tombstone {}", discard.display()))?;
    }
    std::fs::rename(&base, &discard)
        .with_context(|| format!("cannot retire base {}", base.display()))?;
    std::fs::rename(&partial, &base)
        .with_context(|| format!("cannot publish restored base {}", base.display()))?;
    // The swap itself is done the instant the rename above lands — the base
    // is already the new content. A failure removing the retired-base
    // tombstone from here on is NOT a swap failure and must not be reported
    // as one (a caller like `rollback_at` decides whether to record a
    // revision based on this `Result`, and the mutation already happened
    // either way): `repair_base_swap`, run at the top of every future
    // `lock_base`, sweeps a stray `.discard` opportunistically once the base
    // exists again, the same best-effort tombstone discipline `harvest`
    // already uses for its own tombstone.
    if let Err(e) = std::fs::remove_dir_all(&discard) {
        eprintln!(
            "pall8t: warning: could not remove old base tombstone {}: {e}",
            discard.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fork (FR-1)
// ---------------------------------------------------------------------------

/// Metadata pinned at fork, carried into the harvested changeset.
#[derive(Debug, Serialize, Deserialize)]
struct InstanceMeta {
    run: String,
    workspace: String,
    created: u64,
    /// The pid of the forking `pall8t run`. Because `pall8t run`
    /// exec-replaces itself into `container run` (ADR-0006), this pid stays
    /// alive for the whole run and dies (freeing the instance to harvest)
    /// exactly when the run ends — including on `kill -9`. It is the
    /// liveness signal harvest uses (see [`is_forker_alive`]), needing no
    /// `container` CLI call and closing the fork→container-appears window a
    /// container-listing check would leave open.
    forker_pid: u32,
    /// The `[[home.policy]]` overrides active at fork time. `harvest_instance`
    /// classifies this instance's changed paths with THIS policy, not the
    /// invoking cwd's — a run's writes were made under the project it was
    /// forked from, and harvest is lazy (it can run from a completely
    /// different project's `pall8t run`/`home harvest`). Without this, a
    /// project X that declares some path secret via policy would have that
    /// secret staged into the inbox in cleartext (never written back per
    /// FR-10) and its harvest revision redacted with the wrong policy the
    /// moment a project Y lazily harvests X's instance.
    ///
    /// `Option`, not a bare `Vec`, to distinguish two cases an empty `Vec`
    /// would conflate: `None` — an instance forked before this field
    /// existed, so there is nothing pinned and `harvest_instance` falls
    /// back to the caller's overrides (today's pre-fix behavior, `#[serde(
    /// default)]` gives old meta.toml files exactly this). `Some(vec![])`
    /// — this project genuinely has zero `[[home.policy]]` overrides, and
    /// THAT is authoritative: harvest must classify with the built-in
    /// defaults only, not silently fall back to whatever overrides the
    /// harvesting cwd happens to declare (which could reclassify a path
    /// this project never touched specially — the same cross-project bug
    /// in miniature, e.g. the harvesting project marking some glob
    /// `ephemeral` and silently discarding this run's knowledge under it).
    #[serde(default)]
    policy: Option<Vec<PolicyRule>>,
}

/// Forks the base home for `run_name` and returns the instance root to
/// mount at `/home/dev`. Public wrapper; resolves the app dir itself so
/// the binary doesn't need the (crate-internal) root path. `overrides` is
/// the policy active for the project this run belongs to — pinned into the
/// instance's metadata so a later, lazy harvest classifies its writes with
/// THIS policy even if it runs from a different project's cwd.
pub fn fork_instance(
    run_name: &str,
    workspace: &Path,
    overrides: &[PolicyRule],
) -> Result<PathBuf> {
    fork_instance_at(
        &crate::config::pall8t_root()?,
        run_name,
        workspace,
        overrides,
    )
}

fn fork_instance_at(
    root: &Path,
    run_name: &str,
    workspace: &Path,
    overrides: &[PolicyRule],
) -> Result<PathBuf> {
    let base = base_dir(root);
    let inst = instances_root(root).join(run_name);
    if inst.exists() {
        // A previous fork with this exact name never harvested (same cwd +
        // pid recycled). Its unharvested contents would otherwise leak into
        // this run; reuse would also violate the clone-into-fresh-dir
        // contract. Refuse rather than silently discard.
        return Err(anyhow!(
            "instance {} already exists — harvest or remove it first",
            inst.display()
        ));
    }
    // Same guard for a harvested-but-not-yet-promoted changeset: a run
    // reusing this name (recycled pid, same cwd) must not clobber it, or the
    // earlier run's staged knowledge would vanish (FR-9 — changesets persist
    // until promoted or dropped).
    if inbox_root(root).join(run_name).exists() {
        return Err(anyhow!(
            "an un-promoted changeset for {run_name} exists — promote or drop it \
             first (`pall8t home show {run_name}`)"
        ));
    }
    std::fs::create_dir_all(instances_root(root))?;
    let partial = sibling_suffix(&inst, ".partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial)
            .with_context(|| format!("cannot clear stale partial fork {}", partial.display()))?;
    }
    // Snapshot the quiescent base into the instance and its ancestor under
    // the base lock, then publish atomically by rename: a crash before the
    // rename leaves only `<run>.partial`, never a half-instance. `base`'s
    // own creation (the very first fork ever) lives in this same locked
    // section too — every base touch happens under the lock, no exceptions.
    {
        let _lock = lock_base(root)?;
        std::fs::create_dir_all(&base)?;
        clone_into(&partial, "root", &base)?;
        clone_into(&partial, "ancestor", &base)?;
    }
    let meta = InstanceMeta {
        run: run_name.to_string(),
        workspace: workspace.to_string_lossy().into_owned(),
        created: now_secs(),
        forker_pid: std::process::id(),
        // `Some`, always — a fresh fork always pins whatever policy was
        // active, even if it's the empty list (see the field's doc
        // comment: `None` is reserved for pre-field instances only).
        policy: Some(overrides.to_vec()),
    };
    std::fs::write(partial.join("meta.toml"), toml::to_string(&meta)?)?;
    std::fs::rename(&partial, &inst)
        .with_context(|| format!("cannot publish the instance at {}", inst.display()))?;
    Ok(inst.join("root"))
}

/// Copy-on-write clone of a directory hierarchy from `src` to `dst` (which
/// must not exist, though `dst`'s parent must — see [`clone_into`], the
/// only sanctioned way to call this). On macOS this is `clonefile(2)` —
/// O(1) metadata, the spec's primary fork mechanism, requiring `src`/`dst`
/// on the same APFS volume. Phase 1 errors clearly if that fails; the
/// non-APFS recursive-copy fallback is Phase 3.
#[cfg(target_os = "macos")]
fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let src_c = CString::new(src.as_os_str().as_bytes())?;
    let dst_c = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: both pointers are valid NUL-terminated C strings living for
    // the call; flags 0 is the documented default.
    let rc = unsafe { libc::clonefile(src_c.as_ptr(), dst_c.as_ptr(), 0) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // EXDEV (cross-device) and ENOTSUP are `clonefile`'s actual
        // "src/dst aren't on the same APFS volume" signature. Anything else
        // (ENOENT from a missing parent dir, EACCES, ...) is a real bug and
        // must not be misreported as an environment/filesystem problem —
        // that's exactly what sent a prior ENOENT regression down the
        // wrong debugging path (see `clone_into`'s regression test).
        let hint = match err.raw_os_error() {
            Some(libc::EXDEV | libc::ENOTSUP) => {
                " (src/dst must be on the same APFS volume — Phase 1 has no \
                 cross-volume fallback; that's Phase 3)"
            }
            _ => "",
        };
        return Err(anyhow!(err).context(format!(
            "clonefile {} -> {} failed{hint}",
            src.display(),
            dst.display()
        )));
    }
    Ok(())
}

/// Clones `src` into `<dst_parent>/<name>`, first creating `dst_parent` —
/// `clone_tree`'s destination leaf must not exist, but its parent must. The
/// only entry point that should call `clone_tree`: both `fork_instance_at`
/// and `begin_revision_snapshot` clone into a freshly-named `.partial`
/// directory, and this is where that shared "parent must exist, leaf must
/// not" invariant is enforced once instead of at each call site by hand —
/// a prior version left it out of `begin_revision_snapshot` alone, which
/// only broke on the real macOS `clonefile` syscall (the non-macOS
/// `copy_tree` fallback tolerates a missing parent, so it never caught it).
fn clone_into(dst_parent: &Path, name: &str, src: &Path) -> Result<()> {
    std::fs::create_dir_all(dst_parent)?;
    clone_tree(src, &dst_parent.join(name))
}

/// Non-macOS builds have no `clonefile`; fall back to a plain recursive
/// copy so the full fork/harvest/promote flow is exercisable off-APFS (dev
/// containers, CI). Correct but O(bytes) — the production non-APFS path is
/// Phase 3.
#[cfg(not(target_os = "macos"))]
fn clone_tree(src: &Path, dst: &Path) -> Result<()> {
    copy_tree(src, dst)
}

/// Recursive copy preserving symlinks (as symlinks, not their targets). Only
/// the non-macOS `clone_tree` uses it — macOS forks via `clonefile`, so this
/// is not compiled there (Phase 1 has no non-APFS fallback on macOS).
#[cfg(not(target_os = "macos"))]
fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(src).with_context(|| format!("cannot stat {}", src.display()))?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src)?;
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::os::unix::fs::symlink(target, dst)
            .with_context(|| format!("cannot symlink {}", dst.display()))?;
    } else if ft.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src, dst)
            .with_context(|| format!("cannot copy {} -> {}", src.display(), dst.display()))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Revisions (FR-7): every base mutation is a snapshot + manifest, no VCS
// ---------------------------------------------------------------------------

/// What kind of base mutation produced a revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RevisionOp {
    /// Secret/state write-back at harvest (knowledge staging alone — no base
    /// write — never records a revision).
    Harvest,
    /// A promote (direct or via `merge`) that landed at least one path.
    Promote,
    /// `pall8t home rollback` itself — recorded so a rollback is undoable.
    Rollback,
}

impl std::fmt::Display for RevisionOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RevisionOp::Harvest => "harvest",
            RevisionOp::Promote => "promote",
            RevisionOp::Rollback => "rollback",
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RevisionMeta {
    seq: u64,
    op: RevisionOp,
    runs: Vec<String>,
    paths: Vec<String>,
    created: u64,
    /// The `[[home.policy]]` overrides active when this revision was
    /// recorded, so `diff` can redact a path some project's policy declared
    /// secret even when read later from a cwd whose policy doesn't (or no
    /// longer does) — see [`diff_at`]. `#[serde(default)]` so a revision
    /// recorded before this field existed parses as an empty list rather
    /// than failing to load; `diff_at` then falls back to relying on the
    /// CURRENT cwd's policy alone for that older revision, same as before.
    #[serde(default)]
    policy: Vec<PolicyRule>,
}

/// One row of `pall8t home log`.
#[derive(Serialize)]
pub struct RevisionSummary {
    pub seq: u64,
    pub op: RevisionOp,
    pub runs: Vec<String>,
    pub paths: usize,
    pub created: u64,
}

/// Zero-padded revision directory name so `ls`-style sorting matches
/// numeric order; six digits comfortably outlives any project's lifetime at
/// one revision per harvest/promote/rollback.
fn seq_name(seq: u64) -> String {
    format!("{seq:06}")
}

/// Begins a candidate revision: clones the base's CURRENT content (i.e. the
/// state right before whatever mutation the caller is about to apply) into a
/// freshly named `revisions/<tmp>.partial` directory. The caller either
/// [`finalize_revision`]s it (the mutation actually changed something) or
/// [`discard_revision_snapshot`]s it (nothing landed) — so a promote that
/// lands nothing, or a no-op harvest, never burns a sequence number on an
/// empty revision. Must be called with the base lock held, before the
/// mutation, so the snapshot is the pre-mutation state.
fn begin_revision_snapshot(root: &Path) -> Result<PathBuf> {
    let revisions = revisions_root(root);
    std::fs::create_dir_all(&revisions)?;
    let partial = revisions.join(format!(
        "pending-{}-{}.partial",
        std::process::id(),
        next_seq()
    ));
    let base = base_dir(root);
    if base.exists() {
        clone_into(&partial, "snapshot", &base)?;
    } else {
        // No base yet (nothing has ever been forked) — the pre-mutation
        // state is legitimately empty.
        std::fs::create_dir_all(partial.join("snapshot"))?;
    }
    Ok(partial)
}

/// Abandons a candidate snapshot that turned out not to be needed.
fn discard_revision_snapshot(partial: &Path) {
    let _ = std::fs::remove_dir_all(partial);
}

/// Guarantees a [`begin_revision_snapshot`] is never silently leaked when a
/// fallible mutation loop between it and [`finalize_revision`] returns early
/// via a bare `?` (`promote_at`'s `merge_entry` loop, `rollback_at`'s
/// `diff_paths`/`swap_base`): the caller either [`Self::disarm`]s it right
/// before deciding finalize-vs-discard, or, on early return, `Drop` discards
/// the still-owned snapshot instead of leaving it as unreachable litter under
/// `revisions/`. Not used by `harvest_instance` — its mutation loop is
/// wrapped in [`apply_harvest_changes`] specifically so a failure preserves
/// partial progress (recorded as a revision) instead of discarding it, which
/// makes a bare `?`-escapes-the-loop guard moot there. This only covers
/// graceful error propagation either way — a `kill -9` mid-mutation skips
/// `Drop` entirely, which is why `gc` also sweeps stray `pending-*.partial`
/// directories directly (belt and suspenders, matching the same
/// crash-safety story instance tombstones already have).
struct PendingRevisionGuard(Option<PathBuf>);

impl PendingRevisionGuard {
    fn disarm(mut self) -> Option<PathBuf> {
        self.0.take()
    }
}

impl Drop for PendingRevisionGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            discard_revision_snapshot(&p);
        }
    }
}

/// Publishes a candidate snapshot as the next revision, then prunes beyond
/// `keep`. Atomic by rename: a crash before the rename leaves only the
/// (ignored, namespaced) `.partial`; a crash after it leaves the revision
/// published but pruning undone, which the next `finalize_revision` or an
/// explicit `gc` catches. Must be called with the base lock held.
fn finalize_revision(
    root: &Path,
    partial: &Path,
    op: RevisionOp,
    runs: Vec<String>,
    paths: Vec<String>,
    policy: &[PolicyRule],
    keep: u32,
) -> Result<u64> {
    let seq = next_revision_seq(root)?;
    let meta = RevisionMeta {
        seq,
        op,
        runs,
        paths,
        created: now_secs(),
        policy: policy.to_vec(),
    };
    std::fs::write(partial.join("meta.toml"), toml::to_string(&meta)?)?;
    let dest = revisions_root(root).join(seq_name(seq));
    std::fs::rename(partial, &dest).with_context(|| format!("cannot publish revision {seq}"))?;
    prune_revisions(root, keep)?;
    Ok(seq)
}

/// Finalizes a candidate revision if `written` is non-empty, discards it
/// otherwise (never both) — the "did this mutation actually change
/// anything" decision every revision-recording caller (harvest, promote)
/// makes the same way. `written` becomes the finalized revision's `paths`;
/// empty means every classified-touched path turned out not to produce an
/// actual write (e.g. a Secret deletion per FR-10, a UTF-8-skip, or every
/// promoted path was already a byte-for-byte match) — no mutation
/// happened, so no revision (FR-7). `rollback` has no discard branch (a
/// no-op rollback returns early before ever taking a snapshot) so it calls
/// [`finalize_revision`] directly instead of through this helper.
fn finalize_or_discard(
    root: &Path,
    pending: &Path,
    written: Vec<String>,
    op: RevisionOp,
    runs: Vec<String>,
    policy: &[PolicyRule],
    keep: u32,
) -> Result<()> {
    if written.is_empty() {
        discard_revision_snapshot(pending);
    } else {
        finalize_revision(root, pending, op, runs, written, policy, keep)?;
    }
    Ok(())
}

fn read_revision_meta(dir: &Path) -> Result<RevisionMeta> {
    let text = std::fs::read_to_string(dir.join("meta.toml"))
        .with_context(|| format!("revision {} has no meta.toml", dir.display()))?;
    toml::from_str(&text).with_context(|| format!("cannot parse {}/meta.toml", dir.display()))
}

/// Published (non-`.partial`) revision directories, oldest first.
fn list_revision_dirs(root: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let dir = revisions_root(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Ok(seq) = name.parse::<u64>() {
            out.push((seq, entry.path()));
        }
        // Anything else (`pending-*.partial` from a crashed finalize) is
        // transient litter, cleaned up by `gc`, not a revision.
    }
    out.sort_by_key(|(seq, _)| *seq);
    Ok(out)
}

fn next_revision_seq(root: &Path) -> Result<u64> {
    Ok(list_revision_dirs(root)?.last().map_or(1, |(s, _)| s + 1))
}

/// Highest published revision number, if any.
fn latest_revision_seq(root: &Path) -> Result<Option<u64>> {
    Ok(list_revision_dirs(root)?.last().map(|(s, _)| *s))
}

/// Removes the oldest revisions beyond `keep`, returning how many were
/// pruned. Called after every [`finalize_revision`] and again by `gc` (in
/// case a crash skipped the post-finalize prune) — i.e. up to once per
/// recorded revision, so `keep = 0` is silently clamped to 1 here rather
/// than warning every time (that would spam one line per harvest/promote/
/// rollback for the life of a misconfigured project); the CLI layer warns
/// about `revisions_keep = 0` once per invocation instead (next to its
/// `[[home.policy]]` warnings). Clamping matters because otherwise the
/// revision [`finalize_revision`] just published would be pruned
/// immediately after being written, leaving `home log` permanently empty
/// and `next_revision_seq` resetting to 1 forever.
fn prune_revisions(root: &Path, keep: u32) -> Result<usize> {
    let keep = if keep == 0 { 1 } else { keep };
    let dirs = list_revision_dirs(root)?;
    let keep = keep as usize;
    if dirs.len() <= keep {
        return Ok(0);
    }
    let excess = dirs.len() - keep;
    for (_, path) in dirs.into_iter().take(excess) {
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("cannot prune revision {}", path.display()))?;
    }
    Ok(excess)
}

/// Lists revisions newest-first (FR-7 `log`).
pub fn list_revisions() -> Result<Vec<RevisionSummary>> {
    list_revisions_at(&crate::config::pall8t_root()?)
}

fn list_revisions_at(root: &Path) -> Result<Vec<RevisionSummary>> {
    let mut out = Vec::new();
    for (seq, path) in list_revision_dirs(root)? {
        match read_revision_meta(&path) {
            Ok(m) => out.push(RevisionSummary {
                seq,
                op: m.op,
                runs: m.runs,
                paths: m.paths.len(),
                created: m.created,
            }),
            Err(e) => eprintln!("pall8t: warning: skipping revision {seq}: {e:#}"),
        }
    }
    // `list_revision_dirs` is already seq-ascending; newest-first is just
    // the reverse, cheaper than re-sorting.
    out.reverse();
    Ok(out)
}

/// One changed path in a `diff` rendering.
pub struct DiffEntry {
    pub path: String,
    pub change: Change,
    /// Secret content is never rendered, only redacted (never in diffs/logs
    /// — the secrets NFR); this flags the redaction so the CLI can show the
    /// marker instead of a content diff.
    pub secret: bool,
    /// A textual diff (`git diff --no-index`, no repository needed) when
    /// the path is non-secret and both sides are readable UTF-8 text.
    /// `None` for secrets, binaries, or a git failure — the caller then
    /// falls back to just the change label.
    pub text_diff: Option<String>,
}

/// Every path that differs between two directory trees, in path order. A
/// path is redacted as secret if EITHER `recorded_overrides` (the policy
/// active when the revision being diffed was recorded) OR `current_overrides`
/// (the cwd's policy right now) classifies it as secret — never just the
/// current one. Revisions are global (`~/.pall8t/revisions/`) but
/// `[[home.policy]]` is per-project; without this a secret a project
/// declared via policy (the documented pattern for credentials the built-in
/// defaults don't cover) would render in cleartext to `pall8t home diff` run
/// from any OTHER cwd, or from a broken/missing config falling back to
/// defaults. Checking both directions also means editing the policy to
/// protect a path AFTER the fact re-protects old snapshots of it too.
fn diff_entries(
    before: &Path,
    after: &Path,
    recorded_overrides: &[PolicyRule],
    current_overrides: &[PolicyRule],
) -> Result<Vec<DiffEntry>> {
    let mut out = Vec::new();
    for rel in union_paths(after, before)? {
        let before_path = before.join(&rel);
        let after_path = after.join(&rel);
        let b = read_opt(&before_path);
        let a = read_opt(&after_path);
        if a == b {
            continue;
        }
        let secret = classify(&rel, recorded_overrides).class == Class::Secret
            || classify(&rel, current_overrides).class == Class::Secret;
        let text_diff = if secret {
            None
        } else {
            text_diff(&before_path, &after_path)
        };
        out.push(DiffEntry {
            path: rel,
            change: classify_change(b.as_ref(), a.as_ref()),
            secret,
            text_diff,
        });
    }
    out.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(out)
}

/// Just the changed paths between two trees, no classification — the
/// touched-paths list [`rollback`] records for its own revision. Deliberately
/// NOT built on top of [`diff_entries`]: that also classifies every path and
/// shells out to `git diff` per non-secret change for [`text_diff`], which
/// `rollback`'s bookkeeping doesn't need and shouldn't pay for (it runs under
/// the base lock, blocking every other base operation) — and it would need
/// policy overrides threaded in for no benefit.
fn diff_paths(before: &Path, after: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for rel in union_paths(after, before)? {
        if read_opt(&before.join(&rel)) != read_opt(&after.join(&rel)) {
            out.push(rel);
        }
    }
    out.sort();
    Ok(out)
}

/// Unified diff of one path via `git diff --no-index` — works on two
/// arbitrary files with no repository involved, the same trick
/// [`git_merge_file`] uses for merging. `None` on a non-zero-or-one exit
/// (error, or non-UTF-8/binary content `git diff` can't usefully render)
/// so the caller falls back to just the change label rather than fail the
/// whole `diff` command over one unrenderable path.
fn text_diff(before: &Path, after: &Path) -> Option<String> {
    let missing = Path::new("/dev/null");
    let out = std::process::Command::new("git")
        .arg("diff")
        .arg("--no-index")
        .arg("--no-color")
        .arg(if before.exists() { before } else { missing })
        .arg(if after.exists() { after } else { missing })
        .output()
        .ok()?;
    // exit 0 = identical (shouldn't happen, caller already filtered), 1 =
    // differences rendered, >1 = error (e.g. binary content) — don't render.
    match out.status.code() {
        Some(0 | 1) => String::from_utf8(out.stdout).ok(),
        _ => None,
    }
}

/// Renders a revision's diff (FR-7 `diff <seq>`): the newest revision
/// compares its snapshot against the current base; any earlier revision
/// compares against the next revision's snapshot (which IS the base as it
/// stood right after this revision's mutation) — pruning never breaks this
/// since keeping revision `seq` always implies keeping the newer `seq+1`.
pub fn diff(seq: u64, overrides: &[PolicyRule]) -> Result<String> {
    diff_at(&crate::config::pall8t_root()?, seq, overrides)
}

fn diff_at(root: &Path, seq: u64, overrides: &[PolicyRule]) -> Result<String> {
    // Locked from the start: `latest_revision_seq` must be read under the
    // same lock that decides `after`, or a mutation landing between an
    // unlocked read and the decision could make this diff attribute a
    // newer change to `seq` (a stale `latest` making the live base look
    // like `seq`'s own successor, or vice versa).
    let lock = lock_base(root)?;
    let rev_dir = revisions_root(root).join(seq_name(seq));
    let before = rev_dir.join("snapshot");
    if !before.exists() {
        return Err(anyhow!(
            "no revision {seq} (see `pall8t home log`, or it may have been pruned)"
        ));
    }
    // The policy active when THIS revision was recorded (see
    // `diff_entries`'s doc comment) — missing only for a revision recorded
    // before this field existed, which parses as an empty list.
    let recorded_overrides = read_revision_meta(&rev_dir)?.policy;
    let latest = latest_revision_seq(root)?.unwrap_or(seq);
    // `_lock`, paired explicitly with `after`, makes "is the base lock
    // still held across the `diff_entries` walk below" a value visible
    // right here, rather than a fact that depends on which arm happened to
    // run (and whether it called `drop`).
    let (after, _lock): (PathBuf, Option<BaseLock>) = if seq >= latest {
        // Reading the live base races a concurrent rollback's `swap_base`,
        // which briefly renames the base directory away — keep holding the
        // lock through `diff_entries` below so the walk always sees a
        // complete tree, not an ENOENT mid-rename.
        (base_dir(root), Some(lock))
    } else {
        // An older revision's `after` is an immutable, already-published
        // snapshot nothing can still be writing — release the lock before
        // `diff_entries` shells out to `git diff` per changed path, rather
        // than block every concurrent fork/harvest/promote/rollback for
        // that duration.
        drop(lock);
        (
            revisions_root(root)
                .join(seq_name(seq + 1))
                .join("snapshot"),
            None,
        )
    };
    let entries =
        diff_entries(&before, &after, &recorded_overrides, overrides).with_context(|| {
            format!(
                "reading revision {seq}'s diff — it may have been pruned by a concurrent \
                 `gc` while this ran"
            )
        })?;
    let mut s = format!("revision {seq}: {} path(s) changed\n", entries.len());
    for e in &entries {
        if e.secret {
            let _ = writeln!(
                s,
                "  {:<8} {}  [secret — content not shown]",
                change_label(e.change),
                e.path
            );
        } else if let Some(d) = &e.text_diff {
            let _ = writeln!(s, "  {:<8} {}", change_label(e.change), e.path);
            for line in d.lines() {
                s.push_str("    ");
                s.push_str(line);
                s.push('\n');
            }
        } else {
            let _ = writeln!(s, "  {:<8} {}", change_label(e.change), e.path);
        }
    }
    Ok(s)
}

/// Restores the base to revision `seq`'s pre-mutation snapshot (FR-7
/// `rollback`), under the base lock, crash-atomic via [`swap_base`]. The
/// rollback itself is recorded as a new revision (paths = what the swap
/// actually changed) so it is undoable like any other base mutation.
///
/// Unlike promote, rollback's OWN revision can't rely solely on
/// `overrides` (the invoking cwd's policy): a whole-tree swap can
/// reintroduce content byte-for-byte from ANY earlier revision, classified
/// under THAT revision's policy, which may be a different project entirely
/// or simply no longer loaded at rollback time. So the policy recorded for
/// this new revision is the SECRET-classifying rules from `overrides` and
/// every still-existing revision's own recorded policy (see
/// [`accumulated_secret_policy`]) — fail-closed over-redaction is the safe
/// failure mode here (a stray secret rule that matches nothing is a no-op;
/// a missing one is a leak). Bounded by `revisions_keep`: a revision old
/// enough to be pruned already lost its policy record regardless of what
/// this function does.
pub fn rollback(seq: u64, overrides: &[PolicyRule], revisions_keep: u32) -> Result<()> {
    rollback_at(
        &crate::config::pall8t_root()?,
        seq,
        overrides,
        revisions_keep,
    )
}

/// The secret-classifying rules from `overrides` and from every
/// still-existing revision's own recorded policy (FR-7 `rollback`'s
/// redaction composition — see [`rollback`]'s doc comment). Filtered to
/// `class == Some(Secret)` ONLY, never the raw rule lists: `classify` is
/// first-match-wins, so concatenating full policies oldest-first would let
/// a broader, non-secret rule recorded by an EARLIER revision (e.g. a
/// project's own `class = "knowledge"` rule over the same glob) sort
/// before a LATER revision's more specific `secret` rule and mask it —
/// exactly the hazard round 1's `diff_entries` fix exists to avoid, by
/// never merging raw rule lists across policies. A list containing only
/// secret-classifying rules can't mask anything this way: any match means
/// secret, no match falls through to `classify`'s own built-in defaults,
/// same as everywhere else this "is it secret" check is used. The cost is
/// fail-closed: a project's specific NON-secret rule that happened to
/// shadow a broader secret rule within its OWN policy isn't reproduced
/// here (only the secret rule survives the filter) — over-redaction, never
/// a leak, which is the acceptable direction to err.
fn accumulated_secret_policy(root: &Path, overrides: &[PolicyRule]) -> Result<Vec<PolicyRule>> {
    let is_secret = |r: &PolicyRule| r.class == Some(Class::Secret);
    let mut out: Vec<PolicyRule> = Vec::new();
    for (_, path) in list_revision_dirs(root)? {
        if let Ok(meta) = read_revision_meta(&path) {
            out.extend(meta.policy.into_iter().filter(is_secret));
        }
    }
    out.extend(overrides.iter().filter(|r| is_secret(r)).cloned());
    Ok(out)
}

fn rollback_at(root: &Path, seq: u64, overrides: &[PolicyRule], revisions_keep: u32) -> Result<()> {
    let _lock = lock_base(root)?;
    let target = revisions_root(root).join(seq_name(seq)).join("snapshot");
    if !target.exists() {
        return Err(anyhow!(
            "no revision {seq} (see `pall8t home log`, or it may have been pruned)"
        ));
    }
    let base = base_dir(root);
    // Check first, snapshot only if needed: rolling back to a snapshot
    // identical to the current base is a true no-op, and `diff_paths` is a
    // cheap byte comparison that doesn't require a base clone to answer —
    // so skip `begin_revision_snapshot`'s clone-then-immediately-discard
    // entirely rather than pay for it on every no-op rollback.
    let touched = diff_paths(&base, &target)?;
    if touched.is_empty() {
        return Ok(());
    }
    // See `rollback`'s doc comment: this new revision's redaction record
    // must cover whatever policy classified the content being reintroduced,
    // not just the invoking cwd's — gathered before finalizing so it
    // reflects every revision that exists right now, not one this rollback
    // is about to add or prune.
    let policy = accumulated_secret_policy(root, overrides)?;
    let pending = PendingRevisionGuard(Some(begin_revision_snapshot(root)?));
    swap_base(root, &target)?;
    let pending = pending.disarm().expect("armed above");
    finalize_revision(
        root,
        &pending,
        RevisionOp::Rollback,
        Vec::new(),
        touched,
        &policy,
        revisions_keep,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Harvest (FR-3 / FR-8)
// ---------------------------------------------------------------------------

/// Harvests every finished instance — one whose forking `pall8t run` process
/// has exited (see [`InstanceMeta::forker_pid`] / [`is_forker_alive`]). Lazy
/// and decoupled from the run's own process (which exec-replaced itself and
/// is long gone). Best-effort per instance: one failure warns and the rest
/// proceed. Returns the run names harvested. Oldest-fork-first, so among
/// concurrent runs the most recently started one's secret/state writes land
/// last (a better "latest-wins" than readdir order).
pub fn harvest_finished(overrides: &[PolicyRule], revisions_keep: u32) -> Result<Vec<String>> {
    harvest_finished_at(&crate::config::pall8t_root()?, overrides, revisions_keep)
}

fn harvest_finished_at(
    root: &Path,
    overrides: &[PolicyRule],
    revisions_keep: u32,
) -> Result<Vec<String>> {
    let dir = instances_root(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    // Pick the finished instances (fail-closed: a still-running or
    // undeterminable run is left alone), then harvest oldest-first.
    let mut finished: Vec<(PathBuf, InstanceMeta)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() {
            continue;
        }
        // `.partial` (fork in progress) and `.discard` (dispose in progress)
        // are transient, not instances — but a leftover one, from a fork or
        // harvest that crashed, is garbage. Sweep it lazily here (best
        // effort) so crashes don't accumulate litter until someone runs
        // `gc` (FR-9 residual); `sweep_tombstones` applies the same
        // liveness/staleness judgment `gc` does.
        if name.ends_with(".partial") || name.ends_with(".discard") {
            sweep_one_tombstone(&entry.path(), &name);
            continue;
        }
        let meta = match read_meta(&entry.path()) {
            Ok(m) => m,
            // A published instance always has a valid meta.toml (written
            // before the fork's rename); an unreadable one is a partial we
            // can't classify, so skip it rather than risk a bad harvest.
            Err(e) => {
                eprintln!("pall8t: warning: skipping instance {name}: {e:#}");
                continue;
            }
        };
        if is_forker_alive(meta.forker_pid) {
            continue;
        }
        finished.push((entry.path(), meta));
    }
    finished.sort_by(|a, b| a.1.created.cmp(&b.1.created).then(a.1.run.cmp(&b.1.run)));

    let mut harvested = Vec::new();
    for (path, meta) in finished {
        match harvest_instance(root, &path, overrides, revisions_keep) {
            Ok(true) => harvested.push(meta.run),
            // Already harvested (and disposed) by a concurrent process.
            Ok(false) => {}
            Err(e) => eprintln!(
                "pall8t: warning: could not harvest instance {}: {e:#}",
                meta.run
            ),
        }
    }
    Ok(harvested)
}

fn read_meta(inst: &Path) -> Result<InstanceMeta> {
    let text = std::fs::read_to_string(inst.join("meta.toml"))
        .with_context(|| format!("instance {} has no meta.toml", inst.display()))?;
    toml::from_str(&text).with_context(|| format!("cannot parse {}/meta.toml", inst.display()))
}

/// True unless the pid is known to be gone. `kill(pid, 0)` probes existence
/// without signaling: rc 0 or `EPERM` (exists, not ours to signal) ⇒ alive;
/// `ESRCH` ⇒ gone; any other error ⇒ treated as alive (fail-closed — never
/// harvest a run we can't prove finished).
fn is_forker_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill` with signal 0 performs only permission/existence checks
    // and has no memory preconditions.
    if unsafe { libc::kill(pid.cast_signed(), 0) } == 0 {
        return true;
    }
    !matches!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH))
}

/// One changed, non-ephemeral path found by [`harvest_instance`]'s
/// classification pass, carried into [`apply_harvest_changes`].
struct Changed {
    rel: String,
    cls: Classified,
    ancestor: Option<Vec<u8>>,
    theirs: Option<Vec<u8>>,
}

/// Harvests a single finished instance under the base lock: classifies every
/// path that changed since the fork, writes secrets/state back to the base,
/// stages knowledge into an inbox changeset, discards ephemeral, then
/// disposes of the instance. Nothing a run produced is lost until the user
/// drops it: knowledge is only ever staged here, never merged into the base.
/// `Ok(false)` if the instance was already harvested by a concurrent process.
///
/// The whole operation holds the base lock (FR-6), so two processes can't
/// both drain the same instance, and secret/state write-backs (which read the
/// current base) stay consistent with concurrent forks and promotes.
fn harvest_instance(
    root: &Path,
    inst: &Path,
    overrides: &[PolicyRule],
    revisions_keep: u32,
) -> Result<bool> {
    // Everything that touches the shared base runs under the lock; the heavy
    // delete of the drained instance is done afterwards, outside it.
    let tombstone = {
        let _lock = lock_base(root)?;
        if !inst.exists() {
            return Ok(false);
        }
        let meta = read_meta(inst)?;
        let theirs_root = inst.join("root");
        let ancestor_root = inst.join("ancestor");
        let base = base_dir(root);

        // Classify with the policy that was active AT FORK TIME for this
        // instance, not the invoking cwd's: harvest is lazy (FR-8) and can
        // run from a completely different project than the one this run
        // belongs to, and the run's writes were made under ITS project's
        // regime. Falls back to the caller's `overrides` only when the
        // instance predates this field entirely (`None`) — a fork that
        // pinned zero overrides (`Some(vec![])`) is authoritative and used
        // as-is, NOT treated the same as missing; see
        // `InstanceMeta::policy`'s doc comment for why that distinction
        // matters (the harvesting cwd's overrides must not silently apply
        // to a project that never declared them).
        let policy: &[PolicyRule] = meta.policy.as_deref().unwrap_or(overrides);

        // Classify every changed, non-ephemeral path up front (FR-7): only
        // Secret/State writes touch the base — Knowledge is only ever
        // staged — so this tells us before mutating anything whether a
        // revision snapshot is needed, and of what.
        let mut changed = Vec::new();
        for rel in union_paths(&theirs_root, &ancestor_root)? {
            let cls = classify(&rel, policy);
            if cls.class == Class::Ephemeral {
                continue;
            }
            let theirs = read_opt(&theirs_root.join(&rel));
            let ancestor = read_opt(&ancestor_root.join(&rel));
            if theirs == ancestor {
                continue; // unchanged by the run
            }
            changed.push(Changed {
                rel,
                cls,
                ancestor,
                theirs,
            });
        }
        // Whether a revision snapshot is worth taking at all: cheap
        // pre-check on classification alone (a Knowledge-only harvest never
        // writes the base, so never needs one). The revision is ultimately
        // recorded WITH `actually_written` below, not this — classification
        // over-approximates (e.g. a run deleting a Secret is classified
        // Secret but, per FR-10, never triggers a base write at all).
        let maybe_touches_base = changed
            .iter()
            .any(|c| matches!(c.cls.class, Class::Secret | Class::State));
        // A plain `Option`, not a `PendingRevisionGuard`: see the guard's
        // own doc comment for why — its `Drop` always discards, which
        // would lose `apply_harvest_changes`' partial progress on a
        // mid-loop failure instead of preserving it (this function's whole
        // point). The finalize-or-discard decision below is made
        // explicitly instead.
        let pending_revision = if maybe_touches_base {
            Some(begin_revision_snapshot(root)?)
        } else {
            None
        };

        let mut staged: Vec<Entry> = Vec::new();
        let mut actually_written: Vec<String> = Vec::new();
        // `apply_harvest_changes` accumulates into `staged`/`actually_written`
        // as it goes, so a failure partway through (a malformed run's
        // `.claude.json`, a `git merge-file` error, …) still leaves them
        // holding whatever landed before the failure — its `Result` is
        // captured, not `?`-propagated here, precisely so the revision
        // decision below sees that partial progress instead of losing all
        // record of a mutation that already happened.
        let loop_result = apply_harvest_changes(
            root,
            &base,
            &meta.run,
            changed,
            &mut actually_written,
            &mut staged,
        );

        // Same reasoning applies to the manifest write: run it (and capture
        // its own outcome) before deciding the revision's fate, rather than
        // letting a `?` here skip straight past the finalize/discard below.
        let manifest_result: Result<()> = if staged.is_empty() {
            Ok(())
        } else {
            write_manifest(root, &meta, staged)
        };

        // Finalize or discard using what ACTUALLY got written, regardless
        // of whether the loop or the manifest write above failed.
        if let Some(pending) = pending_revision {
            finalize_or_discard(
                root,
                &pending,
                actually_written,
                RevisionOp::Harvest,
                vec![meta.run.clone()],
                policy,
                revisions_keep,
            )?;
        }
        // Now propagate any failure — after the mutation that already
        // happened has a revision recording it. The instance is NOT retired
        // on failure, so harvest can be retried (e.g. after the run's
        // `.claude.json` is fixed, or indefinitely if it can't be).
        loop_result?;
        manifest_result?;
        retire_instance(inst)?
    };
    // The tombstone is namespaced and ignored by scanners, so deleting its
    // (whole-home-sized) tree unlocked keeps that I/O out of the critical
    // section without weakening same-instance drain exclusion. Tolerate it
    // already being gone (same helper as `prune_changeset`): a concurrent
    // `gc` sweep can win this exact race (harvest already fully drained the
    // instance and retired it under the lock above, so the tombstone is
    // safe for `sweep_tombstones`/`sweep_one_tombstone` to remove).
    remove_dir_all_tolerant(&tombstone, "tombstone")?;
    Ok(true)
}

/// Applies each [`Changed`] path to the base or the inbox, per its class:
/// Secret/State write (or merge) into `base`, appending the ones that
/// actually changed a byte on disk to `actually_written`; Knowledge is
/// staged into `run`'s changeset and appended to `staged`. Stops at the
/// first error (a malformed `.claude.json`, a `git merge-file` failure, …),
/// returning it — but whatever was pushed to `actually_written`/`staged`
/// before that point stays in the caller's vectors, so a partial mutation
/// is never silently unaccounted for.
fn apply_harvest_changes(
    root: &Path,
    base: &Path,
    run: &str,
    changed: Vec<Changed>,
    actually_written: &mut Vec<String>,
    staged: &mut Vec<Entry>,
) -> Result<()> {
    for Changed {
        rel,
        cls,
        ancestor,
        theirs,
    } in changed
    {
        match cls.class {
            Class::Ephemeral => unreachable!("filtered out by the caller's classification pass"),
            Class::Secret => {
                // Only a secret the run actually changed propagates, so a
                // run that never touched credentials can't clobber a token
                // another run refreshed. Deletion is not propagated —
                // dropping a base credential is never automatic (FR-10).
                // `write_if_changed`: two runs concurrently refreshing the
                // same credential to the same new token must not each burn
                // a revision — only a byte-level change counts as "wrote".
                if let Some(content) = theirs {
                    if write_if_changed(&base.join(&rel), &content)? {
                        actually_written.push(rel);
                    }
                }
            }
            Class::State => {
                if let Some(theirs) = &theirs {
                    let path = base.join(&rel);
                    let current = read_opt(&path);
                    match cls.strategy {
                        // Append-only formats (history.jsonl): keep both
                        // sides' lines, never conflict.
                        MergeStrategy::Union => match &current {
                            None => {
                                write_atomic(&path, theirs)?;
                                actually_written.push(rel);
                            }
                            Some(cur) => {
                                match union_merge(
                                    ancestor.as_deref().unwrap_or_default(),
                                    cur,
                                    theirs,
                                )? {
                                    // A union merge whose result equals what's
                                    // already there (the run's lines were
                                    // already present, e.g. a concurrent run
                                    // appended the identical entry first) is
                                    // not a mutation. Compared directly
                                    // against `cur` (already read above)
                                    // rather than through `write_if_changed`,
                                    // which would re-read the same file.
                                    Some(merged) if merged == *cur => {}
                                    Some(merged) => {
                                        write_atomic(&path, &merged)?;
                                        actually_written.push(rel);
                                    }
                                    // Non-UTF-8 (e.g. a crash-truncated
                                    // append): keep the base rather than
                                    // overwrite it and lose the lines other
                                    // runs accumulated.
                                    None => eprintln!(
                                        "pall8t: warning: {rel} is not valid UTF-8 — \
                                         left the base copy unchanged"
                                    ),
                                }
                            }
                        },
                        MergeStrategy::Inherit => {
                            let merged =
                                state_merge_bytes(current.as_deref(), ancestor.as_deref(), theirs)
                                    .with_context(|| format!("merging state {rel}"))?;
                            // Compared directly against `current` (already
                            // read above) rather than through
                            // `write_if_changed`, which would re-read.
                            if current.as_deref() != Some(merged.as_slice()) {
                                write_atomic(&path, &merged)?;
                                actually_written.push(rel);
                            }
                        }
                    }
                }
            }
            Class::Knowledge => {
                let change = classify_change(ancestor.as_ref(), theirs.as_ref());
                stage_path(root, run, &rel, ancestor.as_ref(), theirs.as_ref())?;
                staged.push(Entry {
                    path: rel,
                    class: cls.class,
                    change,
                    strategy: cls.strategy,
                    explicit: cls.explicit,
                });
            }
        }
    }
    Ok(())
}

/// Retires a drained instance crash-atomically by renaming it to a `.discard`
/// tombstone (returned for the caller to delete). A `kill -9` between this
/// rename and the delete leaves only the tombstone — skipped by
/// [`harvest_finished_at`] — never a half-deleted instance whose ancestor a
/// later harvest would misread.
fn retire_instance(inst: &Path) -> Result<PathBuf> {
    let tomb = sibling_suffix(inst, ".discard");
    if tomb.exists() {
        std::fs::remove_dir_all(&tomb)
            .with_context(|| format!("cannot clear stale tombstone {}", tomb.display()))?;
    }
    std::fs::rename(inst, &tomb)
        .with_context(|| format!("cannot retire instance {}", inst.display()))?;
    Ok(tomb)
}

/// Removes one `.partial`/`.discard` leftover if it's actually garbage,
/// returning whether it was removed (FR-9 residual: orphan tombstone GC). A
/// `.discard` is always safe to remove — [`retire_instance`] only creates one
/// after the harvest that produced it already finished draining the instance
/// under the lock, so nothing still needs it. A `.partial` is only removed
/// once it's [`STALE_PARTIAL_SECS`] old (or has a `meta.toml` naming a dead
/// forker pid, for the case a fork got far enough to write it): a fresh one
/// may be a fork legitimately still in progress. Best-effort; a failed
/// removal is reported as not-removed rather than erroring the whole sweep,
/// since this is opportunistic cleanup, not the caller's main job.
fn sweep_one_tombstone(path: &Path, name: &str) -> bool {
    let stale = if name.ends_with(".discard") {
        true
    } else if let Ok(meta) = read_meta(path) {
        !is_forker_alive(meta.forker_pid)
    } else {
        older_than(path, STALE_PARTIAL_SECS)
    };
    stale && std::fs::remove_dir_all(path).is_ok()
}

/// True if `path`'s mtime is more than `secs` in the past (fail-closed:
/// unreadable metadata is treated as NOT stale, so a filesystem hiccup
/// never triggers an unwanted delete).
fn older_than(path: &Path, secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(age) => age.as_secs() > secs,
        Err(_) => false, // mtime in the future (clock skew) — not stale
    }
}

/// Sweeps every `.partial`/`.discard` leftover under `instances/` (FR-9
/// `gc`). Returns `(partials removed, discards removed)`. The lazy sweep in
/// [`harvest_finished_at`] does the same per-entry judgment as instances are
/// scanned; this is the explicit, whole-directory version for `gc`.
fn sweep_tombstones(root: &Path) -> Result<(usize, usize)> {
    let dir = instances_root(root);
    if !dir.exists() {
        return Ok((0, 0));
    }
    let (mut partials, mut discards) = (0, 0);
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_partial = name.ends_with(".partial");
        let is_discard = name.ends_with(".discard");
        if !is_partial && !is_discard {
            continue;
        }
        if sweep_one_tombstone(&entry.path(), &name) {
            if is_partial {
                partials += 1;
            } else {
                discards += 1;
            }
        }
    }
    Ok((partials, discards))
}

/// Merges a run's durable-state file into the base's current bytes. When the
/// file didn't exist at fork (`ancestor` is `None`) the merge base is an
/// empty object, so every key the run added is treated as its own change and
/// folded in — a concurrent run's keys in the current base survive. A whole
/// overwrite (the old bug) would drop them.
fn state_merge_bytes(
    current: Option<&[u8]>,
    ancestor: Option<&[u8]>,
    theirs: &[u8],
) -> Result<Vec<u8>> {
    let theirs_val: Value = serde_json::from_slice(theirs).context("state is not valid JSON")?;
    let merged = match current {
        None => theirs_val,
        Some(cur) => {
            let base_val: Value =
                serde_json::from_slice(cur).context("base state is not valid JSON")?;
            let anc_val: Value = ancestor
                .and_then(|b| serde_json::from_slice(b).ok())
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            merge3_json(&anc_val, &base_val, &theirs_val)
        }
    };
    let mut bytes = serde_json::to_vec_pretty(&merged)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Three-way key-path merge of JSON objects. Keys the run left unchanged
/// (relative to the fork-point `ancestor`) keep the current base value, so
/// a concurrent run's distinct keys survive; keys the run changed take the
/// run's value (instance-wins — mechanical, no value judgment, FR-2). Runs
/// are applied oldest-fork-first under the base lock, so this never corrupts.
fn merge3_json(ancestor: &Value, base: &Value, theirs: &Value) -> Value {
    if let (Value::Object(a), Value::Object(b), Value::Object(t)) = (ancestor, base, theirs) {
        let null = Value::Null;
        let mut out = b.clone();
        let keys: BTreeSet<&String> = a.keys().chain(b.keys()).chain(t.keys()).collect();
        for k in keys {
            let av = a.get(k).unwrap_or(&null);
            let bv = b.get(k).unwrap_or(&null);
            let tv = t.get(k).unwrap_or(&null);
            if tv == av {
                // Run didn't touch this key; keep the (possibly newer) base.
                continue;
            }
            let merged = merge3_json(av, bv, tv);
            if merged.is_null() && !t.contains_key(k) {
                // The run deleted the key.
                out.remove(k);
            } else {
                out.insert(k.clone(), merged);
            }
        }
        Value::Object(out)
    } else if theirs == ancestor {
        base.clone()
    } else {
        theirs.clone()
    }
}

/// Copies a staged path's fork-point and run versions into the changeset,
/// so promote is self-contained once the instance is gone.
fn stage_path(
    root: &Path,
    run: &str,
    rel: &str,
    ancestor: Option<&Vec<u8>>,
    theirs: Option<&Vec<u8>>,
) -> Result<()> {
    let cs = inbox_root(root).join(run);
    if let Some(a) = ancestor {
        write_atomic(&cs.join("ancestor").join(rel), a)?;
    }
    if let Some(t) = theirs {
        write_atomic(&cs.join("theirs").join(rel), t)?;
    }
    Ok(())
}

fn classify_change(ancestor: Option<&Vec<u8>>, theirs: Option<&Vec<u8>>) -> Change {
    match (ancestor, theirs) {
        (None, _) => Change::Added,
        (Some(_), None) => Change::Deleted,
        (Some(_), Some(_)) => Change::Modified,
    }
}

// ---------------------------------------------------------------------------
// Inbox / changeset
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    run: String,
    workspace: String,
    created: u64,
    entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    path: String,
    class: Class,
    change: Change,
    /// Merge strategy at promote time. Omitted from the manifest when the
    /// class default (`inherit`) applies.
    #[serde(default, skip_serializing_if = "MergeStrategy::is_inherit")]
    strategy: MergeStrategy,
    /// Whether an explicit policy rule matched (else staged by the
    /// conservative default — surfaced so the gap is visible, FR-2).
    explicit: bool,
}

fn write_manifest(root: &Path, meta: &InstanceMeta, entries: Vec<Entry>) -> Result<()> {
    let manifest = Manifest {
        run: meta.run.clone(),
        workspace: meta.workspace.clone(),
        created: meta.created,
        entries,
    };
    let path = inbox_root(root).join(&meta.run).join("manifest.toml");
    write_atomic(&path, toml::to_string_pretty(&manifest)?.as_bytes())
}

/// Removes `paths` from a changeset: deletes their staged blobs, drops their
/// manifest entries, and then either removes the whole changeset (nothing
/// left) or rewrites the trimmed manifest. Shared by promote and drop so the
/// "an emptied changeset is removed, not left as a husk" invariant lives in
/// one place.
fn prune_changeset(cs: &Path, manifest: &mut Manifest, paths: &[String]) -> Result<()> {
    let removed: BTreeSet<&String> = paths.iter().collect();
    for p in paths {
        let _ = std::fs::remove_file(cs.join("theirs").join(p));
        let _ = std::fs::remove_file(cs.join("ancestor").join(p));
    }
    manifest.entries.retain(|e| !removed.contains(&e.path));
    if manifest.entries.is_empty() {
        // Tolerate an already-removed changeset (a concurrent merge/drop got
        // here first) — the dir being gone is the desired end state.
        remove_dir_all_tolerant(cs, "emptied changeset")
    } else {
        write_atomic(
            &cs.join("manifest.toml"),
            toml::to_string_pretty(manifest)?.as_bytes(),
        )
    }
}

fn read_manifest(root: &Path, run: &str) -> Result<Manifest> {
    let path = inbox_root(root).join(run).join("manifest.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("no changeset for run {run} (looked in {})", path.display()))?;
    toml::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
}

/// One row of `pall8t home inbox`.
#[derive(Serialize)]
pub struct ChangesetSummary {
    pub run: String,
    pub workspace: String,
    pub created: u64,
    pub entries: usize,
}

/// Lists pending changesets, most recent first (FR-4 `inbox`).
pub fn list_changesets() -> Result<Vec<ChangesetSummary>> {
    list_changesets_at(&crate::config::pall8t_root()?)
}

fn list_changesets_at(root: &Path) -> Result<Vec<ChangesetSummary>> {
    let dir = inbox_root(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let run = entry.file_name().to_string_lossy().into_owned();
        // One malformed changeset (e.g. a harvest that crashed before writing
        // the manifest) must not hide every other pending changeset.
        match read_manifest(root, &run) {
            Ok(m) => out.push(ChangesetSummary {
                run: m.run,
                workspace: m.workspace,
                created: m.created,
                entries: m.entries.len(),
            }),
            Err(e) => eprintln!("pall8t: warning: skipping changeset {run}: {e:#}"),
        }
    }
    out.sort_by(|a, b| b.created.cmp(&a.created).then(a.run.cmp(&b.run)));
    Ok(out)
}

/// Renders what a run changed (FR-4 `show`). Secrets/state never appear —
/// they were never staged.
pub fn show(run: &str) -> Result<String> {
    show_at(&crate::config::pall8t_root()?, run)
}

fn show_at(root: &Path, run: &str) -> Result<String> {
    let m = read_manifest(root, run)?;
    let mut s = format!(
        "changeset {}\n  workspace: {}\n  harvested: {}\n  {} staged path(s):\n",
        m.run,
        m.workspace,
        fmt_epoch(m.created),
        m.entries.len()
    );
    for e in &m.entries {
        let flag = if e.explicit { "" } else { " (unclassified)" };
        let _ = writeln!(
            s,
            "    {:<8} {:<9} {}{}",
            change_label(e.change),
            class_label(e.class),
            e.path,
            flag
        );
    }
    Ok(s)
}

/// Outcome of a promote (FR-4 / FR-5): paths that landed and paths that
/// conflicted (left staged, base untouched).
#[derive(Debug)]
pub struct PromoteOutcome {
    pub promoted: Vec<String>,
    pub conflicts: Vec<String>,
}

/// Merges a changeset (or `paths` of it) into the base (FR-4). Each path
/// uses the class-appropriate strategy: directory-union for additions,
/// textual 3-way (`git merge-file`) for modified prose/config. Conflicts
/// arise only here (FR-5): a conflicted path is left staged and the base is
/// not touched, so a re-run after manual resolution completes cleanly. All
/// base writes happen under the base lock (FR-6).
///
/// `overrides` is used ONLY for the revision recorded on a successful
/// promote (so a later `diff` can redact a path the CURRENT cwd's policy
/// calls secret) — never for classification. Unlike harvest, promote never
/// re-classifies a path: every entry's `class`/`strategy` was already
/// pinned into the changeset's manifest at harvest time, under the fork
/// instance's OWN recorded policy (see `InstanceMeta::policy`,
/// `harvest_instance`). A promote invoked from a different project's cwd
/// therefore can't misclassify anything — it only ever applies the class
/// decision harvest already made. (Secrets never reach this path at all:
/// `Class::Secret` entries are written back at harvest, never staged.)
pub fn promote(
    run: &str,
    paths: &[String],
    overrides: &[PolicyRule],
    revisions_keep: u32,
) -> Result<PromoteOutcome> {
    promote_at(
        &crate::config::pall8t_root()?,
        run,
        paths,
        overrides,
        revisions_keep,
    )
}

fn promote_at(
    root: &Path,
    run: &str,
    paths: &[String],
    overrides: &[PolicyRule],
    revisions_keep: u32,
) -> Result<PromoteOutcome> {
    let mut manifest = read_manifest(root, run)?;
    let selected = select_entries(&manifest.entries, paths)?;
    let cs = inbox_root(root).join(run);
    let base = base_dir(root);

    let mut promoted = Vec::new();
    let mut conflicts = Vec::new();
    // Distinct from `promoted`: only entries that actually wrote/removed a
    // base file. A "clean" merge_entry outcome can be a true no-op (base
    // already matches `theirs`, or both sides agree there's nothing there)
    // — such a path is still consumed from the changeset (hence in
    // `promoted`), but recording it as part of a revision would snapshot a
    // mutation that never happened.
    let mut written = Vec::new();
    let lock = lock_base(root)?;
    // Speculative: this promote may land nothing (every entry conflicts or
    // is a no-op), in which case the snapshot is discarded rather than
    // becoming a phantom revision (FR-7). Guarded so a `merge_entry` error
    // partway through the loop (e.g. `git merge-file` failing) discards the
    // snapshot too, instead of leaking it.
    let pending = PendingRevisionGuard(Some(begin_revision_snapshot(root)?));
    for entry in &selected {
        match merge_entry(&base, &cs, entry)? {
            MergeResult::Clean(wrote) => {
                promoted.push(entry.path.clone());
                if wrote {
                    written.push(entry.path.clone());
                }
            }
            MergeResult::Conflict => conflicts.push(entry.path.clone()),
        }
    }
    let pending = pending.disarm().expect("armed above");
    finalize_or_discard(
        root,
        &pending,
        written,
        RevisionOp::Promote,
        vec![run.to_string()],
        overrides,
        revisions_keep,
    )?;
    drop(lock);

    // Drop the promoted paths from the changeset; conflicted ones stay staged.
    prune_changeset(&cs, &mut manifest, &promoted)?;
    Ok(PromoteOutcome {
        promoted,
        conflicts,
    })
}

enum MergeResult {
    /// Landed without a conflict. `true` only if a byte on disk actually
    /// changed (a `write_atomic` or `remove_file` happened) — `false` for a
    /// true no-op (the base already matched, or both sides agree there's
    /// nothing there). The caller still treats the path as consumed from
    /// the changeset either way; only the `true` paths belong in a
    /// recorded revision (FR-7 — a revision must correspond to an actual
    /// mutation).
    Clean(bool),
    Conflict,
}

/// Applies one staged path to the base. `ancestor`/`theirs` presence in the
/// changeset encodes the run's change (added/modified/deleted); the base's
/// current state decides between a clean apply and a conflict. `union`-strategy
/// entries (append-only formats) line-union instead of conflicting on content.
fn merge_entry(base: &Path, cs: &Path, entry: &Entry) -> Result<MergeResult> {
    let rel = &entry.path;
    let ancestor = read_opt(&cs.join("ancestor").join(rel));
    let theirs = read_opt(&cs.join("theirs").join(rel));
    let target = base.join(rel);
    let current = read_opt(&target);
    let union = entry.strategy == MergeStrategy::Union;

    match (&ancestor, &theirs) {
        // Added by the run (no fork-point version).
        (None, Some(t)) => match &current {
            None => {
                write_atomic(&target, t)?;
                Ok(MergeResult::Clean(true))
            }
            Some(c) if c == t => Ok(MergeResult::Clean(false)),
            // Same path, different content: union keeps both sides (a conflict
            // only if the content isn't line-mergeable); otherwise it's a
            // directory-union conflict.
            Some(c) if union => write_or_conflict(&target, union_merge(b"", c, t)?),
            Some(_) => Ok(MergeResult::Conflict),
        },
        // Modified by the run.
        (Some(a), Some(t)) => match &current {
            // Base still matches the fork point: fast-forward.
            Some(c) if c == a => {
                write_atomic(&target, t)?;
                Ok(MergeResult::Clean(true))
            }
            Some(c) if c == t => Ok(MergeResult::Clean(false)),
            Some(c) if union => write_or_conflict(&target, union_merge(a, c, t)?),
            Some(c) => match merge3_text(a, c, t)? {
                // A merge that reconstructs exactly what's already there
                // (the run's change was already reflected in the base, or
                // canceled out by base's own independent change) is not a
                // mutation — `c` is already read, so compare directly
                // rather than pay for another read via `write_if_changed`.
                Some(merged) if merged == *c => Ok(MergeResult::Clean(false)),
                Some(merged) => {
                    write_atomic(&target, &merged)?;
                    Ok(MergeResult::Clean(true))
                }
                None => Ok(MergeResult::Conflict),
            },
            // Base deleted a file the run edited: union re-adds the run's
            // version; otherwise it needs a human.
            None if union => {
                write_atomic(&target, t)?;
                Ok(MergeResult::Clean(true))
            }
            None => Ok(MergeResult::Conflict),
        },
        // Deleted by the run (union doesn't apply to a deletion).
        (Some(a), None) => match &current {
            None => Ok(MergeResult::Clean(false)),
            Some(c) if c == a => {
                std::fs::remove_file(&target)
                    .with_context(|| format!("cannot remove {}", target.display()))?;
                Ok(MergeResult::Clean(true))
            }
            Some(_) => Ok(MergeResult::Conflict),
        },
        (None, None) => Ok(MergeResult::Clean(false)),
    }
}

/// Writes a successful merge to the base, or reports a conflict when the merge
/// couldn't be produced (`None`, e.g. non-UTF-8 union input) — the base is left
/// untouched, so it is never corrupted and the path stays staged (FR-5).
/// `write_if_changed`: a union merge that reconstructs exactly what's
/// already there (the run's lines were already present) is not a mutation.
fn write_or_conflict(target: &Path, merged: Option<Vec<u8>>) -> Result<MergeResult> {
    match merged {
        Some(m) => Ok(MergeResult::Clean(write_if_changed(target, &m)?)),
        None => Ok(MergeResult::Conflict),
    }
}

/// Stateless textual three-way merge via `git merge-file` (git is already a
/// dependency; no repository is involved). `Some(merged)` on a clean merge,
/// `None` on conflict. Non-UTF-8 (binary) content is not line-merged: it's
/// a conflict unless the sides happen to match, so the base is never
/// corrupted by textual markers in a binary file.
fn merge3_text(ancestor: &[u8], current: &[u8], theirs: &[u8]) -> Result<Option<Vec<u8>>> {
    git_merge_file(ancestor, current, theirs, false)
}

/// Line-level three-way union merge (`git merge-file --union`): keeps both
/// sides' added lines with no conflict markers — the append-only strategy.
/// `None` when the content can't be line-merged (non-UTF-8): the caller must
/// then NOT overwrite the base (doing so would silently drop the lines other
/// runs accumulated), leaving it unchanged rather than corrupting it.
fn union_merge(ancestor: &[u8], current: &[u8], theirs: &[u8]) -> Result<Option<Vec<u8>>> {
    git_merge_file(ancestor, current, theirs, true)
}

/// Runs `git merge-file [-p] [--union] current ancestor theirs`, returning the
/// merged bytes. `Some` on a clean merge (always, with `union`, which resolves
/// every hunk by keeping both sides); `None` on a textual conflict or on
/// non-UTF-8 input (never line-merged). The single place that shells out to
/// git for text merges, so the temp-file discipline can't drift.
fn git_merge_file(
    ancestor: &[u8],
    current: &[u8],
    theirs: &[u8],
    union: bool,
) -> Result<Option<Vec<u8>>> {
    if [ancestor, current, theirs]
        .iter()
        .any(|b| std::str::from_utf8(b).is_err())
    {
        return Ok(None);
    }
    let dir = unique_temp_dir("merge");
    std::fs::create_dir_all(&dir)?;
    let ours = dir.join("current");
    let base = dir.join("ancestor");
    let other = dir.join("theirs");
    std::fs::write(&ours, current)?;
    std::fs::write(&base, ancestor)?;
    std::fs::write(&other, theirs)?;
    // `git merge-file -p a b c` folds the b->c change into a and writes the
    // result to stdout; exit 0 = clean, >0 = conflict count, <0 = error.
    // `--union` resolves every conflict hunk by keeping both sides (exit 0).
    let mut cmd = std::process::Command::new("git");
    cmd.arg("merge-file").arg("-p");
    if union {
        cmd.arg("--union");
    }
    let out = cmd
        .arg(&ours)
        .arg(&base)
        .arg(&other)
        .output()
        .context("failed to run: git merge-file")?;
    let _ = std::fs::remove_dir_all(&dir);
    match out.status.code() {
        Some(0) => Ok(Some(out.stdout)),
        Some(n) if n > 0 => Ok(None),
        other => Err(anyhow!(
            "git merge-file failed ({:?}): {}",
            other,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// Discards a changeset, or `paths` of it (FR-4 `drop`).
pub fn drop_changeset(run: &str, paths: &[String]) -> Result<()> {
    drop_changeset_at(&crate::config::pall8t_root()?, run, paths)
}

fn drop_changeset_at(root: &Path, run: &str, paths: &[String]) -> Result<()> {
    let cs = inbox_root(root).join(run);
    if paths.is_empty() {
        return std::fs::remove_dir_all(&cs)
            .with_context(|| format!("cannot drop changeset {}", cs.display()));
    }
    let mut manifest = read_manifest(root, run)?;
    let dropped: Vec<String> = select_entries(&manifest.entries, paths)?
        .into_iter()
        .map(|e| e.path)
        .collect();
    prune_changeset(&cs, &mut manifest, &dropped)
}

/// One changeset processed by [`merge`]: what `show` rendered for it, the
/// paths promoted, and any that conflicted (non-empty ⇒ merge stopped here).
#[derive(Debug)]
pub struct MergeStep {
    pub run: String,
    pub shown: String,
    pub promoted: Vec<String>,
    pub conflicts: Vec<String>,
}

/// Result of a [`merge`]: the runs harvested (their secret/state already
/// written to the base) and the changeset promote `steps`, in processing
/// order (oldest fork first). If the last step has conflicts, processing
/// stopped there and any later changesets were left staged.
#[derive(Debug)]
pub struct MergeReport {
    pub harvested: Vec<String>,
    pub steps: Vec<MergeStep>,
}

/// The `harvest && show && promote-all` convenience composition (FR-4/FR-11).
/// Harvests (all finished runs, or just `run` if given), then for each
/// resulting/pending changeset — oldest fork first, the same order harvest
/// applies secret/state — records its `show` rendering and promotes all its
/// paths. A conflict stops processing at that changeset (its clean paths still
/// land; conflicted ones and every later changeset stay staged) — no rollback,
/// consistent with FR-5/FR-6. Pure composition of the existing harvest/show/
/// promote internals; no new merge logic.
pub fn merge(
    run: Option<&str>,
    overrides: &[PolicyRule],
    revisions_keep: u32,
) -> Result<MergeReport> {
    merge_at(
        &crate::config::pall8t_root()?,
        run,
        overrides,
        revisions_keep,
    )
}

fn merge_at(
    root: &Path,
    run: Option<&str>,
    overrides: &[PolicyRule],
    revisions_keep: u32,
) -> Result<MergeReport> {
    let harvested = match run {
        Some(r) => harvest_run(root, r, overrides, revisions_keep)?,
        None => harvest_finished_at(root, overrides, revisions_keep)?,
    };
    let targets = match run {
        Some(r) if changeset_exists(root, r) => vec![r.to_string()],
        // A named run that harvested (secrets/state only) has nothing to
        // promote; one that neither harvested nor has a changeset is unknown —
        // error, matching promote/drop/show, rather than silently succeed.
        Some(r) if harvested.is_empty() => {
            return Err(anyhow!(
                "no run {r} to merge — no instance or changeset by that name"
            ));
        }
        Some(_) => Vec::new(),
        None => pending_runs_oldest_first(root)?,
    };

    let mut steps = Vec::new();
    for r in targets {
        // A concurrent `merge` may have consumed this changeset between the
        // listing above and here; skip it rather than error.
        if !changeset_exists(root, &r) {
            continue;
        }
        let shown = show_at(root, &r)?;
        let outcome = promote_at(root, &r, &[], overrides, revisions_keep)?;
        let stop = !outcome.conflicts.is_empty();
        steps.push(MergeStep {
            run: r,
            shown,
            promoted: outcome.promoted,
            conflicts: outcome.conflicts,
        });
        if stop {
            break; // leave this changeset's conflicts and all later ones staged
        }
    }
    Ok(MergeReport { harvested, steps })
}

/// Harvests a single finished run's instance for [`merge`], returning its run
/// name if it actually harvested one. Empty if the instance is gone (already
/// harvested, or the run only produced a changeset); errors if the run is
/// still live.
fn harvest_run(
    root: &Path,
    run: &str,
    overrides: &[PolicyRule],
    revisions_keep: u32,
) -> Result<Vec<String>> {
    let inst = instances_root(root).join(run);
    if !inst.exists() {
        return Ok(Vec::new());
    }
    let meta = read_meta(&inst)?;
    if is_forker_alive(meta.forker_pid) {
        return Err(anyhow!(
            "run {run} is still running — cannot merge a live run"
        ));
    }
    if harvest_instance(root, &inst, overrides, revisions_keep)? {
        Ok(vec![run.to_string()])
    } else {
        Ok(Vec::new()) // concurrently harvested by another process
    }
}

fn changeset_exists(root: &Path, run: &str) -> bool {
    inbox_root(root).join(run).join("manifest.toml").exists()
}

/// Pending changeset run names, oldest fork first — the order `merge` folds
/// them in, matching harvest's oldest-first secret/state application.
fn pending_runs_oldest_first(root: &Path) -> Result<Vec<String>> {
    let mut cs = list_changesets_at(root)?;
    cs.sort_by(|a, b| a.created.cmp(&b.created).then(a.run.cmp(&b.run)));
    Ok(cs.into_iter().map(|c| c.run).collect())
}

/// Resolves the caller's `paths` (empty = all) against the changeset's
/// entries. An unknown path is an error rather than a silent no-op, so a
/// typo in `promote <run> .claude/skils/x` is caught.
fn select_entries(entries: &[Entry], paths: &[String]) -> Result<Vec<Entry>> {
    if paths.is_empty() {
        return Ok(entries.to_vec());
    }
    let mut out = Vec::new();
    for p in paths {
        let matched: Vec<Entry> = entries
            .iter()
            .filter(|e| &e.path == p || e.path.starts_with(&format!("{p}/")))
            .cloned()
            .collect();
        if matched.is_empty() {
            return Err(anyhow!("no staged path matching {p} in this changeset"));
        }
        out.extend(matched);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Instance & inbox lifecycle (FR-9): ls, rm, gc
// ---------------------------------------------------------------------------

/// An instance's liveness as `ls` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    /// The forking `pall8t run` is still alive.
    Running,
    /// The forker has exited; awaiting lazy or explicit harvest.
    Finished,
}

impl std::fmt::Display for InstanceStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            InstanceStatus::Running => "running",
            InstanceStatus::Finished => "finished",
        })
    }
}

/// One row of `pall8t home ls`.
#[derive(Serialize)]
pub struct InstanceSummary {
    pub run: String,
    pub workspace: String,
    pub created: u64,
    pub status: InstanceStatus,
    pub age_secs: u64,
    /// A `running` instance old enough that a real foreground run is
    /// implausible — more likely the forker pid was recycled by an
    /// unrelated process after the actual run crashed (the FR-9 stall
    /// residual), leaving the instance stuck as seemingly-live forever.
    /// Surfaced, not auto-resolved: `rm --force` is the user's call.
    pub suspicious: bool,
}

/// Lists live/finished instances (FR-9 `ls`), newest fork first.
pub fn list_instances() -> Result<Vec<InstanceSummary>> {
    list_instances_at(&crate::config::pall8t_root()?)
}

fn list_instances_at(root: &Path) -> Result<Vec<InstanceSummary>> {
    let dir = instances_root(root);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let now = now_secs();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_dir() || name.ends_with(".partial") || name.ends_with(".discard")
        {
            // `.partial`/`.discard` are transient litter, not instances —
            // `gc` (or the lazy sweep in harvest) is what clears them.
            continue;
        }
        let meta = match read_meta(&entry.path()) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("pall8t: warning: skipping instance {name}: {e:#}");
                continue;
            }
        };
        let age_secs = now.saturating_sub(meta.created);
        let running = is_forker_alive(meta.forker_pid);
        out.push(InstanceSummary {
            run: meta.run,
            workspace: meta.workspace,
            created: meta.created,
            status: if running {
                InstanceStatus::Running
            } else {
                InstanceStatus::Finished
            },
            age_secs,
            suspicious: running && age_secs > SUSPICIOUS_RUNNING_SECS,
        });
    }
    out.sort_by(|a, b| b.created.cmp(&a.created).then(a.run.cmp(&b.run)));
    Ok(out)
}

/// Removes an instance without harvesting it (FR-9 `rm`) — the escape hatch
/// when harvest can never happen, e.g. the pid-recycling stall (`ls` shows
/// `suspicious`) or a run the user simply wants to abandon. Refuses a run
/// whose forker pid looks alive unless `force`, since that discards whatever
/// the run produced with no chance to harvest it first.
pub fn rm(run: &str, force: bool) -> Result<()> {
    rm_at(&crate::config::pall8t_root()?, run, force)
}

fn rm_at(root: &Path, run: &str, force: bool) -> Result<()> {
    // Under the base lock: `rm` and a concurrent `harvest` must not both act
    // on the same instance (harvest drains-then-retires it under this same
    // lock; `rm` deletes it outright).
    let _lock = lock_base(root)?;
    let inst = instances_root(root).join(run);
    if !inst.exists() {
        return Err(anyhow!("no instance named {run} (see `pall8t home ls`)"));
    }
    let meta = read_meta(&inst)?;
    if is_forker_alive(meta.forker_pid) && !force {
        return Err(anyhow!(
            "run {run}'s forker (pid {}) appears to still be running — pass --force to \
             remove it anyway. If the pid was recycled by an unrelated process after the \
             real run ended (see `pall8t home ls`), that's exactly what --force is for and \
             this just discards the run's unharvested changes. But if the run IS still \
             live, its instance root may be bind-mounted as /home/dev inside a running \
             container right now — removing it out from under that container corrupts \
             the live agent's home, not just its unharvested changes.",
            meta.forker_pid
        ));
    }
    std::fs::remove_dir_all(&inst)
        .with_context(|| format!("cannot remove instance {}", inst.display()))?;
    Ok(())
}

/// A pending changeset older than the configured TTL — surfaced by `gc` as a
/// warning, never auto-deleted (FR-9: dropping unreviewed knowledge is a
/// user decision).
#[derive(Serialize)]
pub struct StaleChangeset {
    pub run: String,
    pub age_days: u64,
}

/// Result of `pall8t home gc` (FR-9).
#[derive(Serialize)]
pub struct GcReport {
    pub removed_partials: usize,
    pub removed_discards: usize,
    /// Orphaned `revisions/pending-*.partial` snapshots (a `kill -9` between
    /// [`begin_revision_snapshot`] and [`finalize_revision`]/
    /// [`discard_revision_snapshot`] — the in-process [`PendingRevisionGuard`]
    /// can't help against a signal that skips `Drop`, so this is the actual
    /// crash-safety net for that window, per spec acceptance #7).
    pub removed_revision_snapshots: usize,
    pub revisions_pruned: usize,
    pub stale_changesets: Vec<StaleChangeset>,
}

/// Removes stray `revisions/pending-*.partial` directories: a
/// [`begin_revision_snapshot`] whose caller was killed before
/// [`finalize_revision`] or [`discard_revision_snapshot`] ran. Always safe
/// to remove once found — a pending snapshot is only ever created inside a
/// `lock_base` critical section, and this function is only ever called
/// inside one too, so nothing else can still be writing one.
fn sweep_revision_partials(root: &Path) -> Result<usize> {
    let dir = revisions_root(root);
    if !dir.exists() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && name.starts_with("pending-") && name.ends_with(".partial")
        {
            std::fs::remove_dir_all(entry.path())
                .with_context(|| format!("cannot remove stray revision snapshot {name}"))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Cleans up (FR-9 `gc`): orphaned `.partial`/`.discard` instance
/// tombstones, orphaned revision snapshots, revisions beyond
/// `revisions_keep`, and warns (never deletes) about inbox changesets older
/// than `inbox_ttl_days`.
pub fn gc(revisions_keep: u32, inbox_ttl_days: u32) -> Result<GcReport> {
    gc_at(
        &crate::config::pall8t_root()?,
        revisions_keep,
        inbox_ttl_days,
    )
}

fn gc_at(root: &Path, revisions_keep: u32, inbox_ttl_days: u32) -> Result<GcReport> {
    let (removed_partials, removed_discards) = sweep_tombstones(root)?;
    let (removed_revision_snapshots, revisions_pruned) = {
        // Both read/delete under `revisions/`; serialize with any concurrent
        // finalize (which also prunes) and with a fork/harvest/promote/
        // rollback that might be mid-way through its own pending snapshot.
        let _lock = lock_base(root)?;
        let swept = sweep_revision_partials(root)?;
        let pruned = prune_revisions(root, revisions_keep)?;
        (swept, pruned)
    };
    let ttl_secs = u64::from(inbox_ttl_days) * 86_400;
    let now = now_secs();
    let mut stale_changesets = Vec::new();
    for cs in list_changesets_at(root)? {
        let age_secs = now.saturating_sub(cs.created);
        if age_secs > ttl_secs {
            stale_changesets.push(StaleChangeset {
                run: cs.run,
                age_days: age_secs / 86_400,
            });
        }
    }
    Ok(GcReport {
        removed_partials,
        removed_discards,
        removed_revision_snapshots,
        revisions_pruned,
        stale_changesets,
    })
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Removes a directory tree, tolerating it already being gone: a concurrent
/// process (`gc`'s sweep, another `promote`/`drop`, …) can legitimately win
/// the exact same removal race once the caller's own critical section
/// already made the removal safe and desired — that must not fail an
/// operation which, from the caller's perspective, already succeeded. Any
/// OTHER error still fails normally. `what` names the thing being removed,
/// for the error message.
fn remove_dir_all_tolerant(path: &Path, what: &str) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!(e).context(format!("cannot remove {what} {}", path.display()))),
    }
}

/// Sorted union of the relative file paths under `theirs` and `ancestor` —
/// every path that could have been added, modified, or deleted by the run.
/// The caller classifies (cheaply skipping ephemeral) before reading bytes.
fn union_paths(theirs: &Path, ancestor: &Path) -> Result<Vec<String>> {
    let mut all: BTreeSet<String> = BTreeSet::new();
    for p in walk_files(theirs)? {
        all.insert(p);
    }
    for p in walk_files(ancestor)? {
        all.insert(p);
    }
    Ok(all.into_iter().collect())
}

/// Relative paths of every file/symlink under `root` (directories are
/// recursed, not emitted). Empty vec if `root` doesn't exist.
fn walk_files(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if root.exists() {
        walk_rec(root, &PathBuf::new(), &mut out)?;
    }
    Ok(out)
}

fn walk_rec(base: &Path, rel: &Path, out: &mut Vec<String>) -> Result<()> {
    let dir = base.join(rel);
    for entry in
        std::fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))?
    {
        let entry = entry?;
        let child = rel.join(entry.file_name());
        // `file_type` does not follow symlinks, so a symlink to a directory
        // is emitted as an entry, never recursed into.
        if entry.file_type()?.is_dir() {
            walk_rec(base, &child, out)?;
        } else {
            out.push(child.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

fn read_opt(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

/// Writes `bytes` to `path` atomically: a temp sibling written and renamed
/// over the target, so a reader (or a `kill -9`) never sees a half-written
/// file. Creates parent directories as needed.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = sibling_suffix(path, &format!(".tmp-{}-{}", std::process::id(), next_seq()));
    std::fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("cannot move into place {}", path.display()))?;
    Ok(())
}

/// Writes `bytes` to `path` only if they differ from what's already there,
/// returning whether it actually wrote. A merge/write-back whose result
/// happens to be byte-identical to the current content (e.g. two runs
/// concurrently refreshing a secret to the same token, or a union merge
/// whose new lines were already present) must not count as a "wrote" for
/// revision-recording purposes — FR-7 says a revision corresponds to an
/// actual mutation, not merely an attempted one.
///
/// Callers that don't already have the current content in hand (the Secret
/// write-back here, and `write_or_conflict`'s union path, which has no
/// caller-side read to reuse) should call this. A caller that already read
/// the current bytes for its own merge logic (the State branches just
/// below; `merge_entry`'s 3-way-text-merge branch) should compare directly
/// against what it already has instead — same "skip if unchanged" rule,
/// without paying for a second read of the same file.
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool> {
    if read_opt(path).as_deref() == Some(bytes) {
        return Ok(false);
    }
    write_atomic(path, bytes)?;
    Ok(true)
}

/// `path` with `suffix` appended to its final component (same parent).
fn sibling_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

fn unique_temp_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pall8t-{tag}-{}-{}",
        std::process::id(),
        next_seq()
    ))
}

/// Process-unique counter so temp names never collide within one process.
fn next_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn class_label(c: Class) -> &'static str {
    match c {
        Class::Secret => "secret",
        Class::State => "state",
        Class::Knowledge => "knowledge",
        Class::Ephemeral => "ephemeral",
    }
}

fn change_label(c: Change) -> &'static str {
    match c {
        Change::Added => "added",
        Change::Modified => "modified",
        Change::Deleted => "deleted",
    }
}

/// UTC `YYYY-MM-DD HH:MM:SSZ` from a Unix timestamp, without pulling in a
/// date crate (Howard Hinnant's days-from-civil, inverted). `pub` so the CLI
/// can render `log`/`ls` timestamps consistently with `show`'s.
// The single-letter bindings (y/m/d, z, h/mi/s) mirror Howard Hinnant's
// days-from-civil reference algorithm; renaming them would only obscure the
// correspondence with the source.
#[allow(clippy::many_single_char_names)]
pub fn fmt_epoch(secs: u64) -> String {
    let days = (secs / 86_400).cast_signed();
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // days since 1970-01-01 -> civil date
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}Z")
}

#[cfg(test)]
mod tests;
