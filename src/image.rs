use crate::{config::Config, container, repos};
use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};

/// The image `pall8t run`/`build` resolves to for a project directory.
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
}

/// Resolves the Containerfile and image tag for `cwd`. Priority: explicit
/// `container.containerfile` config (relative to `cwd`; must exist) >
/// `<cwd>/Containerfile` if present > the embedded default written to
/// `~/.pall8t/Containerfile`. A project Containerfile gets a
/// per-workspace tag base (`pall8t-<slug>-<hash(cwd)>` — the cwd hash
/// keeps two directories that share a basename from pruning each other's
/// builds); the shared default gets `pall8t-base`, so every project on
/// the default image reuses one build.
pub fn resolve(cwd: &Path, cfg: &Config, uid: u32, gid: u32) -> Result<ResolvedImage> {
    let (containerfile, base) = match &cfg.containerfile {
        Some(p) => {
            let p = repos::expand_tilde(p);
            let p = if p.is_absolute() { p } else { cwd.join(p) };
            if !p.is_file() {
                return Err(anyhow!(
                    "configured containerfile {} does not exist",
                    p.display()
                ));
            }
            (p, project_base(cwd))
        }
        None => {
            let local = cwd.join("Containerfile");
            if local.is_file() {
                (local, project_base(cwd))
            } else {
                (
                    container::default_containerfile_path()
                        .context("cannot write the default Containerfile")?,
                    "pall8t-base".to_string(),
                )
            }
        }
    };
    let hash = hash_with_retry(&containerfile)
        .ok_or_else(|| anyhow!("cannot read {}", containerfile.display()))?;
    Ok(ResolvedImage {
        tag: container::image_tag_hashed(&base, uid, gid, &hash),
        base,
        containerfile,
        hash,
    })
}

/// [`container::containerfile_content_hash`], retried briefly: editors
/// with atomic saves replace the file by rename, leaving a window in
/// which the path transiently has nothing readable behind it — a run
/// racing that window should wait it out, not hard-fail.
fn hash_with_retry(containerfile: &Path) -> Option<String> {
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if let Some(h) = container::containerfile_content_hash(containerfile) {
            return Some(h);
        }
    }
    None
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
                BuildAttempt::Poisoned => Err(anyhow!(
                    "{} keeps changing during build — wait for it to settle and try again",
                    retry.containerfile.display()
                )),
            }
        }
    }
}

/// Outcome of one [`try_build`] attempt.
enum BuildAttempt {
    Done,
    /// The Containerfile's content no longer matches what was hashed into
    /// `resolved.tag` — the just-built image was deleted rather than kept
    /// under a misleading tag. See [`ensure_built`] for the retry.
    Poisoned,
}

/// Runs `container build` for `resolved.tag`, then re-hashes the same
/// Containerfile to confirm nothing changed mid-build; a mismatch deletes
/// the mistagged image and reports [`BuildAttempt::Poisoned`]. Otherwise,
/// best-effort prunes superseded builds under `resolved.base`, excluding
/// images any existing container currently runs (parallel `pall8t run`s
/// may still be on an older tag).
fn try_build(resolved: &ResolvedImage, uid: u32, gid: u32) -> Result<BuildAttempt> {
    let ctx_dir = resolved.containerfile.parent().unwrap_or(Path::new("."));
    eprintln!(
        "pall8t: building {} from {} (this can take a few minutes)…",
        resolved.tag,
        resolved.containerfile.display()
    );
    container::build_image(&resolved.containerfile, ctx_dir, &resolved.tag, uid, gid)?;

    match container::containerfile_content_hash(&resolved.containerfile) {
        Some(fresh) if fresh != resolved.hash => {
            delete_poisoned(&resolved.tag);
            return Ok(BuildAttempt::Poisoned);
        }
        Some(_) => {}
        None => {
            eprintln!(
                "pall8t: warning: could not re-read {} after building {} to confirm its tag — continuing",
                resolved.containerfile.display(),
                resolved.tag
            );
        }
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
                        eprintln!("pall8t: warning: could not prune superseded image {old}: {e:#}")
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
