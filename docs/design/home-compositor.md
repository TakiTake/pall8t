# Design: home compositor (`src/home.rs`), Phase 1

> Implements Phase 1 (MVP) of [docs/specs/home-compositor.md](../specs/home-compositor.md).
> The spec is the decision record; this document describes the module as built.
> (Note: [DESIGN.md](DESIGN.md) is the deprecated TUI architecture — unrelated.)

pall8t's `[home] mode` selects how the container's `/home/dev` is materialized.
`shared` (the default) is byte-for-byte today's behavior: mount `~/.pall8t/home`
rw. `isolated` gives each run a private, instantly-forked copy of that base
home, harvests what the run produced when it ends, and lets the user fold
selected changes back with `pall8t home promote`. Everything lives in the
`pall8t` crate (`src/home.rs`), reusing the existing config merge and the
`git`/subprocess plumbing; no new dependency is added.

## On-disk layout

Everything is a sibling of the base under `~/.pall8t`, so **none of it is
visible inside the agent's `$HOME`** — there is no `.git` or metadata in or
beside the base (evaluation doc, table C).

```
~/.pall8t/
  home/                     the base — a valid, mountable $HOME at every instant
  home.lock                 per-base advisory lock (flock), FR-6
  instances/<run>/          one fork
    root/                   bind-mounted as /home/dev; the run writes here
    ancestor/               base snapshot at fork time — the 3-way merge base
    meta.toml               run name, workspace, fork time, forker pid
  inbox/<run>/              one harvested changeset
    manifest.toml           entries: path, class, change, explicit
    theirs/<rel>            the run's version of each staged (knowledge) path
    ancestor/<rel>          the fork-point version of each staged path
```

A changeset is self-contained (it carries both merge sides), so promote works
after the instance is gone.

## Policy (FR-2)

`classify(rel, overrides)` maps a `$HOME`-relative path to a `Class` — first
match wins, user `[[home.policy]]` rules before the built-in `DEFAULT_RULES`.
No match ⇒ unclassified, treated as staged `knowledge` but flagged, so a policy
gap can neither silently drop nor leak data. Globs are matched by a small
in-crate matcher (`*` within a segment, `**` across segments) — no glob crate.

Disposition by class, per the spec table:

| Class | At harvest |
|---|---|
| `secret` | latest-wins write-back to the base (only if the run changed it); never staged, never in a diff |
| `state` | key-path 3-way JSON merge into the base (or line-union for `strategy = "union"`) |
| `knowledge` | staged in the inbox; merged only on explicit promote |
| `ephemeral` | discarded |

A policy rule may carry an optional `strategy` (`inherit` — the class default —
or `union`). `union` is a line-level 3-way (`git merge-file --union`) that keeps
both sides' added lines and never conflicts — the right thing for append-only
formats, so `.claude/history.jsonl` is `state` + `union` by default (auto-merged
at harvest, never reaching the inbox). A strategy-only rule defaults its class to
`state`; `union` is ignored (with a warning) on `secret`/`ephemeral`. Union input
that isn't line-mergeable (non-UTF-8, e.g. a crash-truncated append) is never
written over the base — the base is kept intact (a warning at harvest, a conflict
at promote) rather than silently dropping the lines other runs accumulated.

## Flow

**Fork (FR-1).** Under the base lock (source quiescent), the base is cloned
twice — into `root/` (the run's writable home) and `ancestor/` (the merge base)
— inside `<run>.partial/`, then published by a single `rename`. A crash before
the rename leaves only `<run>.partial`, never a half-instance. On macOS the
clone is `clonefile(2)` (`#[cfg(target_os = "macos")]`, O(1) CoW, APFS
required — clear error otherwise, per Phase 1). Off macOS it is a recursive
copy, so the whole flow runs on Linux/CI.

**Harvest (FR-3/FR-8).** Lazy and process-decoupled: on the next `pall8t run`
(isolated) or `pall8t home harvest`, every *finished* instance is drained.
"Finished" is decided by the forking process's pid, recorded in `meta.toml`:
because `pall8t run` exec-replaces itself into `container run` (ADR-0006), that
pid stays alive for the whole run and dies exactly when the run ends —
including on `kill -9` — so a `kill(pid, 0)` probe is the liveness signal. This
needs no `container` CLI call and, unlike a container-name lookup, has no
fork→container-appears window in which a concurrent harvest could delete a
just-forked instance. The check is fail-closed: an undeterminable run is left
alone. Paths that changed vs. the fork-point ancestor are classified (ephemeral
skipped before its bytes are even read); secrets/state are written back to the
base; knowledge is staged; ephemeral is dropped. The whole per-instance harvest
holds the base lock, so two processes can't drain the same instance and the
write-backs stay consistent with concurrent forks/promotes. The drained
instance is then disposed crash-atomically (rename to a `.discard` tombstone,
then delete). Nothing a run produced is merged into the base's knowledge
automatically — only ever staged.

**Promote / drop (FR-4).** `inbox` lists changesets, `show` renders what
changed, `promote <run> [paths…]` merges all or selected paths into the base,
`drop <run> [paths…]` discards them. Per-path granularity is real: one run's
keeper skill can promote while its PoC scraps stay staged for a later drop.

**Merge (FR-4/FR-11 convenience).** `pall8t home merge [<run>]` is the
`harvest && show && promote-all` chain in one command: it harvests (all
finished runs, or one named run), then for each pending changeset — oldest fork
first, the order harvest applied secret/state — prints what `show` would and
promotes all its paths. No confirmation prompt (the command is itself the
explicit action; the printed `show` output is the record). A conflict stops
processing at that changeset (its clean paths still land; conflicted ones and
every later changeset stay staged — no rollback, consistent with FR-5/FR-6) and
exits non-zero pointing at per-path `promote`/`drop`. It is a thin composition
of the existing harvest/show/promote internals — no new merge logic — and
tolerates a changeset a concurrent `merge` already consumed.

## Merge strategies

- **Directory-union** for additive knowledge: a path the base lacks is added; a
  same path with identical content is a no-op; **same path, different content is
  the only conflict** (spec FR-4).
- **Textual 3-way** for modified prose/config via stateless `git merge-file`
  (no repository). Non-UTF-8 content is never line-merged — it conflicts unless
  the sides match, so the base is never corrupted with markers in a binary file.
- **Key-path JSON** for `state`: keys the run left unchanged relative to the
  fork point keep the current base value (so a concurrent run's distinct keys
  survive); keys the run changed take the run's value (mechanical, instance-wins).
- **Latest-wins** for `secret`.

## Serialization & atomicity (FR-5/FR-6)

A blocking `flock(2)` on `home.lock` serializes every base mutation (fork
snapshot, harvest write-back, promote) across processes and is released by the
kernel on `kill -9`. Individual base files are written temp-then-`rename`, so a
reader or a crash never sees a half-written file, and the base is a valid `$HOME`
at every instant. **Conflicts arise only at promote**, under user attention: a
conflicted path is left staged and the base untouched, and `pall8t home promote`
exits non-zero listing them — a re-run after manual resolution completes cleanly.

## Testability

All platform-independent logic (classification, glob matching, key-path JSON
merge, harvest diffing, inbox/promote/drop, atomic writes) is unit-tested on
Linux, where `clone_tree` is the recursive-copy fallback, so fork → harvest →
inbox → promote runs end-to-end without APFS or the `container` CLI. Internal
functions take an explicit app-dir root; the public entry points resolve
`~/.pall8t` and delegate.

## Known limitations (Phase 1)

Accepted residuals, all in the safe direction (never merge a live run into the
base) and mostly dissolved by Phase 2's registry/`gc`:

- **Forker-pid liveness assumes the exec'd `container run` process lives exactly
  as long as the container** (true under ADR-0006's foreground/attached model).
  If that process is `SIGKILL`ed while the runtime leaves the container running
  orphaned, harvest could drain a still-live instance — a far narrower window
  than a container-listing check's fail-open, but not zero.
- **Pid recycling can make a finished run look alive forever** (its pid reused
  by an unrelated long-lived process), so its changeset never harvests. Safe
  (never drains a live run) but an unbounded-disk risk on low-`pid_max` hosts;
  Phase 2 orphan detection (FR-9) closes it.
- **A recycled pid + same cwd can block a new run**: because the run name is
  `pall8t-<cwd-key>-<pid>`, a new run whose name collides with a lingering
  un-promoted changeset is refused (rather than silently clobbering it). Correct
  and safe, but surprising; inherent to pid-derived naming until Phase 2 gives
  changesets pid-independent identity.
- **Secret latest-wins is content-blind**: harvest order (oldest-fork-first) is
  deterministic but not token-recency based, so among same-second concurrent
  refreshes an older token can win. A robust fix compares the credential's
  `expiresAt` (Phase 2/3 resolver).
- **A crash mid-tombstone-delete leaks a `.discard` dir** (skipped by scanners,
  reclaimed by Phase 2 `gc`).
- The **macOS `clonefile` fork path** is unexercised by the Linux test suite;
  the recursive-copy fallback is what tests run.

## Deferred (Phase 2/3)

Versioned base with `log`/`diff`/`rollback`, instance/inbox registry with
`ls`/`rm`/`gc` and orphan detection, inbox TTL warnings, `--json` output, a
`resolve`/`conflicts` command pair, the pluggable (local-Claude) merge resolver,
and the production non-APFS fork. Union-merge could also gain optional
dedup / timestamp-sort of the combined lines (deliberately omitted now — git's
plain union ordering is enough for append-only history, and sorting/deduping is
format-specific over-engineering for Phase 1). The Phase 1 interfaces don't
preclude any of these.
