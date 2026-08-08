use crate::{
    config::{self, Config},
    container, repos,
};
use anyhow::{anyhow, Context, Result};
use std::path::{Component, Path, PathBuf};

/// The image `pall8t run`/`build` resolves to for a project directory.
#[derive(Debug)]
pub struct ResolvedImage {
    /// Hash-suffixed tag: `<base>:<uid>-<gid>-<hash>`. FR-2's "compare the
    /// Containerfile hash against the last build" is stateless: the hash
    /// lives in the tag, so "did it change?" is exactly "does an image
    /// with this tag exist?".
    pub tag: String,
    /// Tag base, scoping the post-build prune of superseded siblings.
    pub base: String,
    pub containerfile: PathBuf,
    /// Content hash embedded in `tag` at resolve time.
    pub hash: String,
    /// `container.watch` entries resolved at resolve time, so the
    /// post-build poison check in `try_build` re-reads the identical set
    /// (issue #35).
    pub watch: Vec<WatchFile>,
}

/// Resolves the Containerfile and image tag for `cwd`. Priority: explicit
/// `container.containerfile` config (relative to `cwd`; must exist) >
/// `<cwd>/.pall8t/Containerfile` if present > the embedded default written
/// to `~/.pall8t/Containerfile`. Note there is no fallback to a root
/// `<cwd>/Containerfile` — that file usually belongs to the project's own
/// app image, and pall8t silently building it as the sandbox image would
/// be a footgun; a project that wants it anyway can still set
/// `container.containerfile = "Containerfile"`. A project Containerfile
/// gets a per-workspace tag base (`pall8t-<slug>-<hash(cwd)>` — the cwd
/// hash keeps two directories that share a basename from pruning each
/// other's builds); the shared default gets `pall8t-base`, so every
/// project on the default image reuses one build. `cfg.container.watch`
/// (issue #35) folds extra project files' paths and contents into the same
/// hash, so a project's own Containerfile is required when it's set — see
/// the error below.
pub fn resolve(cwd: &Path, cfg: &Config, uid: u32, gid: u32) -> Result<ResolvedImage> {
    let watch = resolve_watch_paths(cwd, &cfg.watch)?;
    let (containerfile, base) = match probe_containerfile(cwd, cfg)? {
        Some(found) => found,
        // The default image builds from ~/.pall8t, so project files can't
        // affect it, and giving it a project-specific tag would make
        // `prune_superseded` delete other projects' images sharing
        // `pall8t-base` (decision 5, issue #35).
        None if !watch.is_empty() => {
            return Err(anyhow!(
                "container.watch is set but this project has no Containerfile of its own \
                 — add .pall8t/Containerfile or set container.containerfile"
            ));
        }
        None => (
            container::default_containerfile_path()
                .context("cannot write the default Containerfile")?,
            "pall8t-base".to_string(),
        ),
    };
    let hash = hash_with_retry(&containerfile, &watch)?;
    Ok(ResolvedImage {
        tag: container::image_tag_hashed(&base, uid, gid, &hash),
        base,
        containerfile,
        hash,
        watch,
    })
}

/// The explicit-config and project-local halves of [`resolve`]'s priority
/// order — everything before the embedded-default fallback. `Ok(None)`
/// means neither applies and the caller should fall through to the shared
/// default image.
fn probe_containerfile(cwd: &Path, cfg: &Config) -> Result<Option<(PathBuf, String)>> {
    if let Some(p) = &cfg.containerfile {
        let p = repos::expand_tilde(p);
        let p = if p.is_absolute() { p } else { cwd.join(p) };
        if !p.is_file() {
            return Err(anyhow!(
                "configured containerfile {} does not exist",
                p.display()
            ));
        }
        return Ok(Some((p, project_base(cwd))));
    }
    let local = cwd.join(config::PROJECT_DIR).join("Containerfile");
    if local.is_file() {
        return Ok(Some((local, project_base(cwd))));
    }
    Ok(None)
}

/// One `container.watch` entry, resolved against the project directory
/// (issue #35).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchFile {
    /// Normalized path as declared in config (`.` components dropped,
    /// `/`-joined), relative to the project dir — the identity used for
    /// hashing, sorting, and dedup.
    rel: String,
    /// Absolute path resolved against `cwd`, used to actually read the file.
    abs: PathBuf,
}

/// Hard cap on the number of `container.watch` entries (decision 3, issue
/// #35): watch is for lockfiles a Containerfile builds from, not whole
/// trees. Exceeding it is an error, never silent truncation.
const WATCH_MAX_FILES: usize = 100;

/// Hard cap on the combined size of `container.watch` files (decision 3);
/// the Containerfile itself is exempt, as today. Exceeding it is an error,
/// never silent truncation.
const WATCH_MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024;

/// Validates and resolves `cfg.container.watch` against `cwd` (decision 2,
/// issue #35): literal relative paths only, no globs, no
/// canonicalization/symlink chasing (not a privilege boundary here, and
/// canonicalizing would fight [`hash_with_retry`]'s atomic-save retry).
/// Rejects an empty path, an absolute path, a leading `~`, and any `..`
/// component; drops `.` components. Sorted and deduped by the normalized
/// relative path so TOML list order is irrelevant, then checked against
/// [`WATCH_MAX_FILES`]. Existence and "must be a regular file" are
/// deliberately NOT checked here — they're enforced per read attempt, in
/// [`try_read_watched`], since a one-time check here couldn't catch a
/// symlink swapped in between resolve time and the post-build re-check.
pub fn resolve_watch_paths(cwd: &Path, watch: &[PathBuf]) -> Result<Vec<WatchFile>> {
    let mut files = Vec::with_capacity(watch.len());
    for p in watch {
        let rel = normalize_watch_path(p)?;
        let abs = cwd.join(&rel);
        files.push(WatchFile { rel, abs });
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    files.dedup_by(|a, b| a.rel == b.rel);
    if files.len() > WATCH_MAX_FILES {
        return Err(anyhow!(
            "container.watch lists {} files, over the {WATCH_MAX_FILES}-file cap — \
             watch is meant for lockfiles a Containerfile builds from, not whole trees",
            files.len()
        ));
    }
    Ok(files)
}

/// Lexical validation for one `container.watch` entry (see
/// [`resolve_watch_paths`]); returns the normalized `/`-joined relative
/// path.
fn normalize_watch_path(p: &Path) -> Result<String> {
    let raw = p.as_os_str().to_string_lossy();
    if raw.is_empty() {
        return Err(anyhow!("container.watch entry must not be empty"));
    }
    if raw.starts_with('~') {
        return Err(anyhow!(
            "container.watch entry {raw} must not start with '~' — paths are relative to the \
             project directory"
        ));
    }
    let mut parts: Vec<&str> = Vec::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(s) => parts.push(
                s.to_str()
                    .ok_or_else(|| anyhow!("container.watch entry {raw} is not valid UTF-8"))?,
            ),
            Component::ParentDir => {
                return Err(anyhow!(
                    "container.watch entry {raw} must not contain '..' components"
                ));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!(
                    "container.watch entry {raw} must be a relative path, not absolute"
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(anyhow!("container.watch entry must not be empty"));
    }
    Ok(parts.join("/"))
}

/// Hashes the Containerfile together with every watched file (decision 1,
/// issue #35). An empty `watched` delegates verbatim to
/// [`container::sha256_hex_prefix`] on the Containerfile bytes alone —
/// byte-identical to the pre-#35 hash, so upgrading pall8t with no
/// `container.watch` set triggers no rebuild. Otherwise, hashes a
/// length-prefixed encoding (kills concatenation ambiguity) of the
/// Containerfile followed by each `(rel_path, bytes)` pair sorted by
/// `rel_path` (kills TOML list order and caller-order sensitivity):
/// a versioned domain tag, then `u64_le(len) ++ bytes` for the
/// Containerfile, then for each watched file (sorted) `u64_le(len) ++
/// path_utf8` and `u64_le(len) ++ bytes`.
pub(crate) fn combined_hash(containerfile_bytes: &[u8], watched: &[(String, Vec<u8>)]) -> String {
    if watched.is_empty() {
        return container::sha256_hex_prefix(containerfile_bytes, 6);
    }
    let mut sorted: Vec<&(String, Vec<u8>)> = watched.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let capacity = 16
        + 8
        + containerfile_bytes.len()
        + sorted
            .iter()
            .map(|(rel, bytes)| 8 + rel.len() + 8 + bytes.len())
            .sum::<usize>();
    let mut buf = Vec::with_capacity(capacity);
    buf.extend_from_slice(b"pall8t-watch-v1\0");
    buf.extend_from_slice(&(containerfile_bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(containerfile_bytes);
    for (rel, bytes) in sorted {
        let rel_bytes = rel.as_bytes();
        buf.extend_from_slice(&(rel_bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(rel_bytes);
        buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        buf.extend_from_slice(bytes);
    }
    container::sha256_hex_prefix(&buf, 6)
}

/// Outcome of one [`try_read_watched`] attempt.
#[derive(Debug)]
enum ReadOutcome {
    Ready {
        containerfile_bytes: Vec<u8>,
        watched: Vec<(String, Vec<u8>)>,
    },
    /// `path` (the Containerfile or one watched file) couldn't be read —
    /// a transient candidate, see [`hash_with_retry`].
    Unreadable(PathBuf),
}

/// One whole-set read attempt for hashing: the Containerfile plus every
/// watched file, in that order. `max_total_bytes` bounds the watched
/// files' combined size only (the Containerfile is exempt, as today) and
/// is a parameter so tests can use tiny values. Exceeding it is a terminal
/// `Err` — unlike a missing file, an oversized watch list isn't a race
/// that a retry could resolve.
fn try_read_watched(
    containerfile: &Path,
    watch: &[WatchFile],
    max_total_bytes: u64,
) -> Result<ReadOutcome> {
    use std::io::Read;
    let Ok(containerfile_bytes) = std::fs::read(containerfile) else {
        return Ok(ReadOutcome::Unreadable(containerfile.to_path_buf()));
    };
    let mut watched = Vec::with_capacity(watch.len());
    let mut total: u64 = 0;
    for wf in watch {
        // Open once and read the size and content off the same handle
        // (rather than a separate `std::fs::metadata` stat before
        // `std::fs::read`'s own open+stat): fewer syscalls, and — since
        // `metadata` follows symlinks — still catches a watch entry that's
        // (or resolves through a symlink to) a device/FIFO/socket before
        // reading it. That check matters because reading one of those can
        // block forever (e.g. `/dev/zero` reports size 0 and never hits
        // EOF) or misbehave outright, and lexical-only path validation
        // (decision 2) doesn't rule that out.
        let Ok(mut file) = std::fs::File::open(&wf.abs) else {
            return Ok(ReadOutcome::Unreadable(wf.abs.clone()));
        };
        let Ok(meta) = file.metadata() else {
            return Ok(ReadOutcome::Unreadable(wf.abs.clone()));
        };
        if !meta.is_file() {
            return Err(anyhow!(
                "container.watch entry {} is not a regular file",
                wf.abs.display()
            ));
        }
        total += meta.len();
        if total > max_total_bytes {
            return Err(anyhow!(
                "container.watch files total more than {max_total_bytes} bytes — \
                 watch is meant for lockfiles, not whole trees"
            ));
        }
        // Capacity is only a hint (`read_to_end` grows as needed), so a
        // size that doesn't fit `usize` (32-bit targets) just means no hint.
        let mut bytes = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
        if file.read_to_end(&mut bytes).is_err() {
            return Ok(ReadOutcome::Unreadable(wf.abs.clone()));
        }
        watched.push((wf.rel.clone(), bytes));
    }
    Ok(ReadOutcome::Ready {
        containerfile_bytes,
        watched,
    })
}

/// Reads the Containerfile and every watched file and combines them into
/// one hash (see [`combined_hash`]), retried briefly: editors with atomic
/// saves replace a file by rename, leaving a window in which a path
/// transiently has nothing readable behind it — a run racing that window
/// should wait it out, not hard-fail. A persistent miss becomes "cannot
/// read {path}", naming whichever file (Containerfile or a watched file)
/// never became readable — never hashed as empty (decision 4).
fn hash_with_retry(containerfile: &Path, watch: &[WatchFile]) -> Result<String> {
    const ATTEMPTS: u32 = 5;
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        match try_read_watched(containerfile, watch, WATCH_MAX_TOTAL_BYTES)? {
            ReadOutcome::Ready {
                containerfile_bytes,
                watched,
            } => return Ok(combined_hash(&containerfile_bytes, &watched)),
            ReadOutcome::Unreadable(path) if attempt == ATTEMPTS - 1 => {
                return Err(anyhow!("cannot read {}", path.display()));
            }
            ReadOutcome::Unreadable(_) => {}
        }
    }
    unreachable!("the loop above always returns on its last attempt")
}

fn project_base(cwd: &Path) -> String {
    format!("pall8t-{}", repos::path_key(cwd))
}

/// Resolves and, if no image for the current Containerfile content exists,
/// builds before returning (FR-2). Set `force` to build unconditionally
/// (`pall8t build` — e.g. to pick up updated base images or packages the
/// hash can't see). On build failure the error propagates and nothing is
/// launched.
pub fn ensure_built(
    cwd: &Path,
    cfg: &Config,
    uid: u32,
    gid: u32,
    force: bool,
) -> Result<ResolvedImage> {
    let resolved = resolve(cwd, cfg, uid, gid)?;
    if !force && container::image_exists(&resolved.tag) {
        return Ok(resolved);
    }
    match try_build(&resolved, uid, gid)? {
        BuildAttempt::Done => Ok(resolved),
        BuildAttempt::Poisoned => {
            // The Containerfile changed while building. Retry ONCE against
            // freshly re-resolved content — bounded, so a file edited
            // faster than it can be built fails loudly instead of looping.
            let retry = resolve(cwd, cfg, uid, gid)?;
            match try_build(&retry, uid, gid)? {
                BuildAttempt::Done => Ok(retry),
                BuildAttempt::Poisoned => {
                    let what = if retry.watch.is_empty() {
                        "keeps changing"
                    } else {
                        "or a watched file keeps changing"
                    };
                    Err(anyhow!(
                        "{} {what} during build — wait for it to settle and try again",
                        retry.containerfile.display()
                    ))
                }
            }
        }
    }
}

/// Outcome of one [`try_build`] attempt.
enum BuildAttempt {
    Done,
    /// The Containerfile's (or a watched file's) content no longer matches
    /// what was hashed into `resolved.tag` — the just-built image was
    /// deleted rather than kept under a misleading tag. See [`ensure_built`]
    /// for the retry.
    Poisoned,
}

/// Runs `container build` for `resolved.tag`, then re-reads the
/// Containerfile and every watched file to confirm nothing changed
/// mid-build; a mismatch deletes the mistagged image and reports
/// [`BuildAttempt::Poisoned`]. Otherwise, best-effort prunes superseded
/// builds under `resolved.base`, excluding images any existing container
/// currently runs (parallel `pall8t run`s may still be on an older tag).
fn try_build(resolved: &ResolvedImage, uid: u32, gid: u32) -> Result<BuildAttempt> {
    let ctx_dir = resolved.containerfile.parent().unwrap_or(Path::new("."));
    eprintln!(
        "pall8t: building {} from {} (this can take a few minutes):",
        resolved.tag,
        resolved.containerfile.display()
    );
    container::build_image(&resolved.containerfile, ctx_dir, &resolved.tag, uid, gid)?;

    // A byte-cap-exceeded `Err` here is folded into "poisoned" alongside a
    // hash mismatch: the watched files fit under the cap when `resolve`
    // last read them (that's how `resolved.hash` got computed), so
    // exceeding it now can only mean one of them grew mid-build — exactly
    // the content-changed case this re-check exists to catch.
    let poisoned = match try_read_watched(
        &resolved.containerfile,
        &resolved.watch,
        WATCH_MAX_TOTAL_BYTES,
    ) {
        Ok(ReadOutcome::Ready {
            containerfile_bytes,
            watched,
        }) => combined_hash(&containerfile_bytes, &watched) != resolved.hash,
        Ok(ReadOutcome::Unreadable(path)) => {
            eprintln!(
                "pall8t: warning: could not re-read {} after building {} to confirm its tag — continuing",
                path.display(),
                resolved.tag
            );
            false
        }
        Err(_) => true,
    };
    if poisoned {
        delete_poisoned(&resolved.tag);
        return Ok(BuildAttempt::Poisoned);
    }

    prune_superseded(resolved, uid, gid);
    Ok(BuildAttempt::Done)
}

/// Deletes the tag a poisoned build was published under — unless an
/// existing container runs that exact image (reachable via a forced
/// `pall8t build` racing a mid-build edit), or the in-use refs can't be
/// determined: deleting an image out from under a live container breaks
/// it, so those cases warn and keep the tag. A kept poisoned tag means a
/// later resolve of the same content would trust the wrong image — hence
/// the instruction to rebuild once the container is gone.
fn delete_poisoned(tag: &str) {
    let in_use = match in_use_refs() {
        Some(refs) => container::in_use_contains(&refs, tag),
        None => true, // indeterminate — same safe posture as pruning
    };
    if in_use {
        eprintln!(
            "pall8t: warning: image {tag} no longer matches its Containerfile but is \
             (or may be) in use by an existing container — keeping it; run \
             `pall8t build` once that container is gone"
        );
        return;
    }
    if let Err(e) = container::image_delete(tag) {
        eprintln!("pall8t: warning: could not delete poisoned tag {tag}: {e:#}");
    }
}

/// Image references every existing container currently runs, from one
/// `container list`. `None` when they can't all be determined (the list
/// failed, or an entry carried no reference) — the caller must then skip
/// pruning rather than risk deleting an image out from under a live
/// container.
fn in_use_refs() -> Option<Vec<String>> {
    container::list_all()
        .ok()?
        .into_iter()
        .map(|c| c.image)
        .collect()
}

/// Deletes superseded builds under `resolved.base` for this uid/gid,
/// keeping `resolved.tag` and anything an existing container runs.
/// Best-effort: failures are warnings, never an error for the build that
/// just succeeded.
fn prune_superseded(resolved: &ResolvedImage, uid: u32, gid: u32) {
    let Some(in_use) = in_use_refs() else {
        eprintln!(
            "pall8t: warning: could not determine which images existing containers use — \
             skipping prune of superseded images"
        );
        return;
    };
    match container::prunable_images(&resolved.base, &resolved.tag, uid, gid, &in_use) {
        Ok(tags) => {
            for old in tags {
                match container::image_delete(&old) {
                    Ok(()) => eprintln!("pall8t: pruned superseded image {old}"),
                    Err(e) => {
                        eprintln!("pall8t: warning: could not prune superseded image {old}: {e:#}");
                    }
                }
            }
        }
        Err(e) => eprintln!(
            "pall8t: warning: could not list images to prune under {}: {e:#}",
            resolved.base
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_cfg(containerfile: Option<PathBuf>) -> Config {
        Config {
            cpus: 4,
            memory: "8g".to_string(),
            containerfile,
            watch: vec![],
            command: vec!["claude".to_string()],
            repos: vec![],
            deprecations: vec![],
            herdr: crate::config::HerdrConfig::default(),
        }
    }

    fn test_cfg_with_watch(containerfile: Option<PathBuf>, watch: Vec<PathBuf>) -> Config {
        Config {
            watch,
            ..test_cfg(containerfile)
        }
    }

    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pall8t-test-image-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn probe_picks_up_dot_pall8t_containerfile() {
        let cwd = tmp_dir("dot-pall8t");
        let dot_pall8t = cwd.join(".pall8t");
        fs::create_dir_all(&dot_pall8t).unwrap();
        fs::write(dot_pall8t.join("Containerfile"), "FROM scratch\n").unwrap();

        let (containerfile, base) = probe_containerfile(&cwd, &test_cfg(None))
            .unwrap()
            .expect("a .pall8t/Containerfile must be found");
        assert_eq!(containerfile, dot_pall8t.join("Containerfile"));
        assert_eq!(base, project_base(&cwd));

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn probe_ignores_root_containerfile_and_falls_through() {
        // The pre-issue-24 `<cwd>/Containerfile` probe is gone: a root
        // Containerfile with no `.pall8t/Containerfile` must not be picked
        // up, leaving `resolve` to fall through to the embedded default.
        let cwd = tmp_dir("root-containerfile");
        fs::write(cwd.join("Containerfile"), "FROM scratch\n").unwrap();

        let found = probe_containerfile(&cwd, &test_cfg(None)).unwrap();
        assert!(
            found.is_none(),
            "a root Containerfile without .pall8t/Containerfile must not be probed"
        );

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn probe_prefers_explicit_config_over_dot_pall8t() {
        let cwd = tmp_dir("explicit-config");
        let dot_pall8t = cwd.join(".pall8t");
        fs::create_dir_all(&dot_pall8t).unwrap();
        fs::write(dot_pall8t.join("Containerfile"), "FROM scratch\n").unwrap();
        fs::write(cwd.join("Custom.containerfile"), "FROM scratch\n").unwrap();

        let cfg = test_cfg(Some(PathBuf::from("Custom.containerfile")));
        let (containerfile, base) = probe_containerfile(&cwd, &cfg).unwrap().unwrap();
        assert_eq!(containerfile, cwd.join("Custom.containerfile"));
        assert_eq!(base, project_base(&cwd));

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn probe_errors_when_explicit_config_missing() {
        let cwd = tmp_dir("missing-explicit-config");
        let cfg = test_cfg(Some(PathBuf::from("does-not-exist")));
        assert!(probe_containerfile(&cwd, &cfg).is_err());

        let _ = fs::remove_dir_all(&cwd);
    }

    // -- combined_hash (issue #35) --------------------------------------

    #[test]
    fn combined_hash_empty_watch_equals_plain_sha256_prefix() {
        // Backward-compat guarantee (decision 1): no `container.watch` set
        // must hash byte-identically to the pre-#35 Containerfile-only hash,
        // so upgrading pall8t triggers no fleet-wide rebuild.
        let cf = b"FROM scratch\n";
        assert_eq!(combined_hash(cf, &[]), container::sha256_hex_prefix(cf, 6));
    }

    #[test]
    fn combined_hash_is_order_invariant() {
        let cf = b"FROM scratch\n";
        let a = vec![
            ("a.lock".to_string(), b"1".to_vec()),
            ("b.lock".to_string(), b"2".to_vec()),
        ];
        let b = vec![
            ("b.lock".to_string(), b"2".to_vec()),
            ("a.lock".to_string(), b"1".to_vec()),
        ];
        assert_eq!(
            combined_hash(cf, &a),
            combined_hash(cf, &b),
            "TOML list order must not affect the hash"
        );
    }

    #[test]
    fn combined_hash_detects_content_swap_between_files() {
        let cf = b"FROM scratch\n";
        let original = vec![
            ("a.lock".to_string(), b"1".to_vec()),
            ("b.lock".to_string(), b"2".to_vec()),
        ];
        let swapped = vec![
            ("a.lock".to_string(), b"2".to_vec()),
            ("b.lock".to_string(), b"1".to_vec()),
        ];
        assert_ne!(
            combined_hash(cf, &original),
            combined_hash(cf, &swapped),
            "swapping content between two watched files must change the hash"
        );
    }

    #[test]
    fn combined_hash_has_no_concatenation_ambiguity_at_path_content_boundary() {
        // Without length-prefixing, ("ab", "c") and ("a", "bc") would both
        // concatenate to the same bytes.
        let cf = b"FROM scratch\n";
        let a = vec![("ab".to_string(), b"c".to_vec())];
        let b = vec![("a".to_string(), b"bc".to_vec())];
        assert_ne!(combined_hash(cf, &a), combined_hash(cf, &b));
    }

    #[test]
    fn combined_hash_non_empty_differs_from_empty() {
        let cf = b"FROM scratch\n";
        let watched = vec![("a.lock".to_string(), b"1".to_vec())];
        assert_ne!(combined_hash(cf, &[]), combined_hash(cf, &watched));
    }

    // -- resolve_watch_paths (issue #35) ---------------------------------

    #[test]
    fn resolve_watch_paths_rejects_invalid_entries() {
        let cwd = Path::new("/some/project");
        for bad in ["/abs", "~/x", "../x", "a/../../x", ""] {
            assert!(
                resolve_watch_paths(cwd, &[PathBuf::from(bad)]).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn resolve_watch_paths_normalizes_dot_components() {
        let cwd = Path::new("/some/project");
        let files = resolve_watch_paths(cwd, &[PathBuf::from("./flake.nix")]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].rel, "flake.nix");
        assert_eq!(files[0].abs, cwd.join("flake.nix"));
    }

    #[test]
    fn resolve_watch_paths_sorts_and_dedups() {
        let cwd = Path::new("/some/project");
        let files = resolve_watch_paths(
            cwd,
            &[
                PathBuf::from("b.lock"),
                PathBuf::from("a.lock"),
                PathBuf::from("a.lock"),
            ],
        )
        .unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["a.lock", "b.lock"], "sorted and deduped");
    }

    #[test]
    fn resolve_watch_paths_errors_over_file_count_cap() {
        let cwd = Path::new("/some/project");
        let watch: Vec<PathBuf> = (0..101).map(|i| PathBuf::from(format!("f{i}"))).collect();
        assert!(resolve_watch_paths(cwd, &watch).is_err());
    }

    // -- read/retry (issue #35) ------------------------------------------

    #[test]
    fn try_read_watched_reports_missing_file_as_unreadable() {
        let cwd = tmp_dir("watch-missing-unreadable");
        fs::write(cwd.join("Containerfile"), "FROM scratch\n").unwrap();
        let watch = resolve_watch_paths(&cwd, &[PathBuf::from("flake.lock")]).unwrap();

        match try_read_watched(&cwd.join("Containerfile"), &watch, WATCH_MAX_TOTAL_BYTES).unwrap() {
            ReadOutcome::Unreadable(path) => assert_eq!(path, cwd.join("flake.lock")),
            ReadOutcome::Ready { .. } => panic!("expected Unreadable for a missing watch file"),
        }

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn hash_with_retry_hard_errors_naming_persistently_missing_watch_file() {
        let cwd = tmp_dir("watch-missing-hard-error");
        let containerfile = cwd.join("Containerfile");
        fs::write(&containerfile, "FROM scratch\n").unwrap();
        let watch = resolve_watch_paths(&cwd, &[PathBuf::from("flake.lock")]).unwrap();

        let err = hash_with_retry(&containerfile, &watch).unwrap_err();
        assert!(
            err.to_string().contains("flake.lock"),
            "error must name the missing watched file: {err}"
        );

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn try_read_watched_errors_when_total_bytes_exceed_cap() {
        let cwd = tmp_dir("watch-byte-cap");
        fs::write(cwd.join("Containerfile"), "FROM scratch\n").unwrap();
        fs::write(cwd.join("big.lock"), "0123456789").unwrap();
        let watch = resolve_watch_paths(&cwd, &[PathBuf::from("big.lock")]).unwrap();

        let err = try_read_watched(&cwd.join("Containerfile"), &watch, 5).unwrap_err();
        assert!(
            err.to_string().contains("watch"),
            "byte-cap error must mention watch: {err}"
        );

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn try_read_watched_rejects_non_regular_files() {
        // A watch entry that's (or resolves through a symlink to) a
        // device/FIFO/socket must be rejected outright: `metadata().len()`
        // can lie about such a file's size (e.g. `/dev/zero` reports 0),
        // letting it sail past the byte cap, and reading it can then block
        // forever instead of erroring. `/dev/null` is used here because,
        // unlike `/dev/zero`, reading it always returns EOF immediately —
        // so this test can't hang even if the guard under test regresses.
        let cwd = tmp_dir("watch-non-regular-file");
        fs::write(cwd.join("Containerfile"), "FROM scratch\n").unwrap();
        std::os::unix::fs::symlink("/dev/null", cwd.join("flake.lock")).unwrap();
        let watch = resolve_watch_paths(&cwd, &[PathBuf::from("flake.lock")]).unwrap();

        let err = try_read_watched(&cwd.join("Containerfile"), &watch, WATCH_MAX_TOTAL_BYTES)
            .unwrap_err();
        assert!(
            err.to_string().contains("not a regular file"),
            "a watch entry resolving to a device file must be rejected: {err}"
        );

        let _ = fs::remove_dir_all(&cwd);
    }

    // -- resolve integration (issue #35) ----------------------------------

    #[test]
    fn resolve_errors_when_watch_set_without_project_containerfile() {
        let cwd = tmp_dir("watch-no-project-containerfile");
        let cfg = test_cfg_with_watch(None, vec![PathBuf::from("flake.lock")]);

        let err = resolve(&cwd, &cfg, 501, 20).unwrap_err();
        assert!(
            err.to_string().contains("Containerfile"),
            "must explain the fix: {err}"
        );

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn resolve_editing_watched_file_changes_tag_not_base() {
        let cwd = tmp_dir("watch-edit-changes-tag");
        let dot_pall8t = cwd.join(".pall8t");
        fs::create_dir_all(&dot_pall8t).unwrap();
        fs::write(dot_pall8t.join("Containerfile"), "FROM scratch\n").unwrap();
        fs::write(cwd.join("flake.lock"), "v1").unwrap();

        let cfg = test_cfg_with_watch(None, vec![PathBuf::from("flake.lock")]);
        let first = resolve(&cwd, &cfg, 501, 20).unwrap();

        fs::write(cwd.join("flake.lock"), "v2").unwrap();
        let second = resolve(&cwd, &cfg, 501, 20).unwrap();

        assert_eq!(
            first.base, second.base,
            "editing a watched file must not change the tag base"
        );
        assert_ne!(
            first.tag, second.tag,
            "editing a watched file must change the tag"
        );

        // Stateless round-trip: reverting the edit reproduces the original
        // tag exactly.
        fs::write(cwd.join("flake.lock"), "v1").unwrap();
        let reverted = resolve(&cwd, &cfg, 501, 20).unwrap();
        assert_eq!(reverted.tag, first.tag);

        let _ = fs::remove_dir_all(&cwd);
    }

    #[test]
    fn resolve_empty_watch_matches_plain_containerfile_hash() {
        let cwd = tmp_dir("watch-empty-matches-plain");
        let dot_pall8t = cwd.join(".pall8t");
        fs::create_dir_all(&dot_pall8t).unwrap();
        let containerfile = dot_pall8t.join("Containerfile");
        fs::write(&containerfile, "FROM scratch\n").unwrap();

        let cfg = test_cfg(None);
        let resolved = resolve(&cwd, &cfg, 501, 20).unwrap();

        let bytes = fs::read(&containerfile).unwrap();
        assert_eq!(resolved.hash, container::sha256_hex_prefix(&bytes, 6));

        let _ = fs::remove_dir_all(&cwd);
    }
}
