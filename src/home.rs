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
//! ```
//!
//! Secrets and durable state never enter the inbox: they are written back
//! to the base at harvest (latest-wins / key-path merge). Only `knowledge`
//! and unclassified paths are staged for explicit promotion (FR-2 table).

use crate::config::PolicyRule;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
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

/// A path's class plus whether an explicit rule matched it. An
/// unclassified path is staged like `knowledge` (conservative default:
/// never silently dropped or leaked) but flagged so the gap is visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classified {
    pub class: Class,
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

/// Built-in classification, applied after any user `[[home.policy]]`
/// overrides. Order matters — first match wins — so the specific secret
/// (`.claude/.credentials.json`) precedes any broader rule. Anything
/// unmatched is treated as unclassified/staged by [`classify`].
pub const DEFAULT_RULES: &[(&str, Class)] = &[
    // Secrets — credentials that must reach later runs but never a diff.
    // Kept deliberately broad: an unmatched credential would fall through to
    // staged-knowledge (host-side, not a leak to agents, but no
    // auto-propagation and cleartext in the inbox), so cover the common ones.
    // Users add more via `[[home.policy]]`.
    (".claude/.credentials.json", Class::Secret),
    (".config/gh/hosts.yml", Class::Secret),
    (".config/gh/**", Class::Secret),
    (".gh-token", Class::Secret),
    (".netrc", Class::Secret),
    (".ssh/**", Class::Secret),
    (".git-credentials", Class::Secret),
    (".aws/credentials", Class::Secret),
    (".config/gcloud/**", Class::Secret),
    (".docker/config.json", Class::Secret),
    (".npmrc", Class::Secret),
    // Durable structured state — mechanically key-path merged.
    (".claude.json", Class::State),
    // Persistent agent memory lives UNDER the per-project dir
    // (`.claude/projects/<slug>/memory/`), so it must be reclassified as
    // knowledge BEFORE the broad `.claude/projects/**` ephemeral rule below —
    // otherwise first-match-wins would discard it (the spec lists memory as
    // knowledge).
    (".claude/projects/*/memory/**", Class::Knowledge),
    // Ephemeral — runtime scratch, never worth harvesting.
    (".cache/**", Class::Ephemeral),
    (".npm/**", Class::Ephemeral),
    (".bash_history", Class::Ephemeral),
    (".zsh_history", Class::Ephemeral),
    (".claude/projects/**", Class::Ephemeral),
    (".claude/todos/**", Class::Ephemeral),
    (".claude/statsig/**", Class::Ephemeral),
    (".claude/shell-snapshots/**", Class::Ephemeral),
    // Knowledge — the point of the exercise: staged, promoted on demand. These
    // precede the broad `**/*.lock` below so a `.lock` file a skill
    // legitimately ships is not silently discarded (first match wins).
    (".claude/skills/**", Class::Knowledge),
    (".claude/agents/**", Class::Knowledge),
    (".claude/commands/**", Class::Knowledge),
    (".claude/plugins/**", Class::Knowledge),
    (".claude/memory/**", Class::Knowledge),
    (".claude/CLAUDE.md", Class::Knowledge),
    (".claude/settings.json", Class::Knowledge),
    ("CLAUDE.md", Class::Knowledge),
    // Generic lock files, last so specific knowledge rules win.
    ("**/*.lock", Class::Ephemeral),
];

/// Classifies a `$HOME`-relative path. User `overrides` are tried first, then
/// [`DEFAULT_RULES`]; first glob match wins. No match ⇒ unclassified, which is
/// staged like `knowledge` but reported.
pub fn classify(rel: &str, overrides: &[PolicyRule]) -> Classified {
    for rule in overrides {
        if glob_match(&rule.glob, rel) {
            return Classified {
                class: rule.class,
                explicit: true,
            };
        }
    }
    for (glob, class) in DEFAULT_RULES {
        if glob_match(glob, rel) {
            return Classified {
                class: *class,
                explicit: true,
            };
        }
    }
    Classified {
        class: Class::Knowledge,
        explicit: false,
    }
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
    Ok(BaseLock { _file: file })
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
}

/// Forks the base home for `run_name` and returns the instance root to
/// mount at `/home/dev`. Public wrapper; resolves the app dir itself so
/// the binary doesn't need the (crate-internal) root path.
pub fn fork_instance(run_name: &str, workspace: &Path) -> Result<PathBuf> {
    fork_instance_at(&crate::config::pall8t_root()?, run_name, workspace)
}

fn fork_instance_at(root: &Path, run_name: &str, workspace: &Path) -> Result<PathBuf> {
    let base = base_dir(root);
    std::fs::create_dir_all(&base)?;
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
    std::fs::create_dir_all(&partial)?;
    // Snapshot the quiescent base into the instance and its ancestor under
    // the base lock, then publish atomically by rename: a crash before the
    // rename leaves only `<run>.partial`, never a half-instance.
    {
        let _lock = lock_base(root)?;
        clone_tree(&base, &partial.join("root"))?;
        clone_tree(&base, &partial.join("ancestor"))?;
    }
    let meta = InstanceMeta {
        run: run_name.to_string(),
        workspace: workspace.to_string_lossy().into_owned(),
        created: now_secs(),
        forker_pid: std::process::id(),
    };
    std::fs::write(partial.join("meta.toml"), toml::to_string(&meta)?)?;
    std::fs::rename(&partial, &inst)
        .with_context(|| format!("cannot publish the instance at {}", inst.display()))?;
    Ok(inst.join("root"))
}

/// Copy-on-write clone of a directory hierarchy from `src` to `dst` (which
/// must not exist). On macOS this is `clonefile(2)` — O(1) metadata, the
/// spec's primary fork mechanism, requiring `src`/`dst` on the same APFS
/// volume. Phase 1 errors clearly if that fails; the non-APFS recursive-copy
/// fallback is Phase 3.
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
        return Err(anyhow!(io::Error::last_os_error()).context(format!(
            "clonefile {} -> {} failed — Phase 1 requires the pall8t home on an APFS volume",
            src.display(),
            dst.display()
        )));
    }
    Ok(())
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
// Harvest (FR-3 / FR-8)
// ---------------------------------------------------------------------------

/// Harvests every finished instance — one whose forking `pall8t run` process
/// has exited (see [`InstanceMeta::forker_pid`] / [`is_forker_alive`]). Lazy
/// and decoupled from the run's own process (which exec-replaced itself and
/// is long gone). Best-effort per instance: one failure warns and the rest
/// proceed. Returns the run names harvested. Oldest-fork-first, so among
/// concurrent runs the most recently started one's secret/state writes land
/// last (a better "latest-wins" than readdir order).
pub fn harvest_finished(overrides: &[PolicyRule]) -> Result<Vec<String>> {
    harvest_finished_at(&crate::config::pall8t_root()?, overrides)
}

fn harvest_finished_at(root: &Path, overrides: &[PolicyRule]) -> Result<Vec<String>> {
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
        // `.partial` (fork in progress) and `.discard` (dispose in progress)
        // are transient, not instances.
        if !entry.file_type()?.is_dir() || name.ends_with(".partial") || name.ends_with(".discard")
        {
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
        match harvest_instance(root, &path, overrides) {
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
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    !matches!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH))
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
fn harvest_instance(root: &Path, inst: &Path, overrides: &[PolicyRule]) -> Result<bool> {
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

        let mut staged: Vec<Entry> = Vec::new();
        for rel in union_paths(&theirs_root, &ancestor_root)? {
            // Classify before reading: ephemeral paths (caches etc., the bulk
            // of a home) are skipped without ever loading their bytes.
            let cls = classify(&rel, overrides);
            if cls.class == Class::Ephemeral {
                continue;
            }
            let theirs = read_opt(&theirs_root.join(&rel));
            let ancestor = read_opt(&ancestor_root.join(&rel));
            if theirs == ancestor {
                continue; // unchanged by the run
            }
            match cls.class {
                Class::Ephemeral => unreachable!("handled above"),
                Class::Secret => {
                    // Only a secret the run actually changed propagates, so a
                    // run that never touched credentials can't clobber a token
                    // another run refreshed. Deletion is not propagated —
                    // dropping a base credential is never automatic (FR-10).
                    if let Some(content) = theirs {
                        write_atomic(&base.join(&rel), &content)?;
                    }
                }
                Class::State => {
                    if let Some(theirs) = &theirs {
                        let path = base.join(&rel);
                        let merged = state_merge_bytes(
                            read_opt(&path).as_deref(),
                            ancestor.as_deref(),
                            theirs,
                        )
                        .with_context(|| format!("merging state {rel}"))?;
                        write_atomic(&path, &merged)?;
                    }
                }
                Class::Knowledge => {
                    let change = classify_change(&ancestor, &theirs);
                    stage_path(root, &meta.run, &rel, &ancestor, &theirs)?;
                    staged.push(Entry {
                        path: rel,
                        class: cls.class,
                        change,
                        explicit: cls.explicit,
                    });
                }
            }
        }

        if !staged.is_empty() {
            write_manifest(root, &meta, staged)?;
        }
        retire_instance(inst)?
    };
    // The tombstone is namespaced and ignored by scanners, so deleting its
    // (whole-home-sized) tree unlocked keeps that I/O out of the critical
    // section without weakening same-instance drain exclusion.
    std::fs::remove_dir_all(&tombstone)
        .with_context(|| format!("cannot remove tombstone {}", tombstone.display()))?;
    Ok(true)
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
                .unwrap_or_else(|| Value::Object(Default::default()));
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
    ancestor: &Option<Vec<u8>>,
    theirs: &Option<Vec<u8>>,
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

fn classify_change(ancestor: &Option<Vec<u8>>, theirs: &Option<Vec<u8>>) -> Change {
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
        std::fs::remove_dir_all(cs)
            .with_context(|| format!("cannot remove emptied changeset {}", cs.display()))
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
        s.push_str(&format!(
            "    {:<8} {:<9} {}{}\n",
            change_label(e.change),
            class_label(e.class),
            e.path,
            flag
        ));
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
pub fn promote(run: &str, paths: &[String]) -> Result<PromoteOutcome> {
    promote_at(&crate::config::pall8t_root()?, run, paths)
}

fn promote_at(root: &Path, run: &str, paths: &[String]) -> Result<PromoteOutcome> {
    let mut manifest = read_manifest(root, run)?;
    let selected = select_entries(&manifest.entries, paths)?;
    let cs = inbox_root(root).join(run);
    let base = base_dir(root);

    let mut promoted = Vec::new();
    let mut conflicts = Vec::new();
    let _lock = lock_base(root)?;
    for entry in &selected {
        match merge_entry(&base, &cs, entry)? {
            MergeResult::Clean => promoted.push(entry.path.clone()),
            MergeResult::Conflict => conflicts.push(entry.path.clone()),
        }
    }
    drop(_lock);

    // Drop the promoted paths from the changeset; conflicted ones stay staged.
    prune_changeset(&cs, &mut manifest, &promoted)?;
    Ok(PromoteOutcome {
        promoted,
        conflicts,
    })
}

enum MergeResult {
    Clean,
    Conflict,
}

/// Applies one staged path to the base. `ancestor`/`theirs` presence in the
/// changeset encodes the run's change (added/modified/deleted); the base's
/// current state decides between a clean apply and a conflict.
fn merge_entry(base: &Path, cs: &Path, entry: &Entry) -> Result<MergeResult> {
    let rel = &entry.path;
    let ancestor = read_opt(&cs.join("ancestor").join(rel));
    let theirs = read_opt(&cs.join("theirs").join(rel));
    let target = base.join(rel);
    let current = read_opt(&target);

    match (&ancestor, &theirs) {
        // Added by the run (no fork-point version).
        (None, Some(t)) => match &current {
            None => {
                write_atomic(&target, t)?;
                Ok(MergeResult::Clean)
            }
            Some(c) if c == t => Ok(MergeResult::Clean),
            // Directory-union conflict: same path, different content.
            Some(_) => Ok(MergeResult::Conflict),
        },
        // Modified by the run.
        (Some(a), Some(t)) => match &current {
            // Base still matches the fork point: fast-forward.
            Some(c) if c == a => {
                write_atomic(&target, t)?;
                Ok(MergeResult::Clean)
            }
            Some(c) if c == t => Ok(MergeResult::Clean),
            Some(c) => match merge3_text(a, c, t)? {
                Some(merged) => {
                    write_atomic(&target, &merged)?;
                    Ok(MergeResult::Clean)
                }
                None => Ok(MergeResult::Conflict),
            },
            // Base deleted a file the run edited — needs a human.
            None => Ok(MergeResult::Conflict),
        },
        // Deleted by the run.
        (Some(a), None) => match &current {
            None => Ok(MergeResult::Clean),
            Some(c) if c == a => {
                std::fs::remove_file(&target)
                    .with_context(|| format!("cannot remove {}", target.display()))?;
                Ok(MergeResult::Clean)
            }
            Some(_) => Ok(MergeResult::Conflict),
        },
        (None, None) => Ok(MergeResult::Clean),
    }
}

/// Stateless textual three-way merge via `git merge-file` (git is already a
/// dependency; no repository is involved). `Some(merged)` on a clean merge,
/// `None` on conflict. Non-UTF-8 (binary) content is not line-merged: it's
/// a conflict unless the sides happen to match, so the base is never
/// corrupted by textual markers in a binary file.
fn merge3_text(ancestor: &[u8], current: &[u8], theirs: &[u8]) -> Result<Option<Vec<u8>>> {
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
    let out = std::process::Command::new("git")
        .arg("merge-file")
        .arg("-p")
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
// Small helpers
// ---------------------------------------------------------------------------

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
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
/// date crate (Howard Hinnant's days-from-civil, inverted).
fn fmt_epoch(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
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
