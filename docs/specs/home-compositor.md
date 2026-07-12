# Spec: agent-home state compositor (pall8t `home` module)

Status: **Accepted**, 2026-07-07. Addresses [issue #9](https://github.com/TakiTake/pall8t/issues/9)
(shared home under parallel runs — requirements §8 roadmap item 1). Background: the
[bit-vcs/bit evaluation](https://github.com/TakiTake/pall8t/issues/9#issuecomment-4904121268)
on that issue, whose conclusions (no third-party APIs near the home dir, selective state
management rather than version-controlling `$HOME`) this spec bakes in.

**One-liner:** Give each sandboxed agent run a private, instantly-forked copy of a shared
home directory, capture everything it produced when it ends, and let the user decide what
folds back into the shared home — even from many parallel runs.

**Form:** a pall8t module (`src/home.rs`), not a standalone tool. The behavior is
selected in the existing two-layer config, defaulting to today's shared home:

```toml
[home]
# mode = "shared"    # default: every run mounts ~/.pall8t/home rw (v1 behavior)
# mode = "isolated"  # per-run fork + harvest/promote, per this spec (experimental in v1)
```

Because the config merges per field with the project winning, one project can opt into
`isolated` while everything else stays `shared` — the natural migration path.

## Core model

Four entities:

- **Base** — the canonical shared home (today's `~/.pall8t/home`), with a versioned
  lineage. It must remain a valid, mountable `$HOME` at every instant.
- **Instance** — a per-run writable materialization of the base, stamped with the base
  revision it was forked from (that stamp is the merge ancestor later).
- **Inbox** — where harvested changesets land. One changeset per finished run: the full
  set of non-ephemeral changes the run made, with its fork-point ancestor recorded.
  Lossless — nothing a run produced is dropped until the user says so.
- **Policy manifest** — declarative path classification. `$HOME` is not one kind of
  state; each class has a fork behavior and a *disposition* on harvest:

| Class | Examples | Fork | Disposition on harvest |
|---|---|---|---|
| `secret` | `.claude/.credentials.json`, `.gh-token` | copied | auto write-back to base (latest-wins); **never in diffs/logs** |
| `state` | `.claude.json` durable keys; `.claude/history.jsonl` (append-only, `union`) | copied | auto structural (key-path JSON, or line-union for append-only) merge — mechanical, no value judgment |
| `knowledge` | `.claude/skills/`, memory, `CLAUDE.md`, `settings.json` | copied | **staged in inbox; merged only on explicit promote** |
| `ephemeral` | `.npm`, `.cache`, `.bash_history`, locks | fresh or copied | discarded, never merged |

Unclassified paths default to a conservative class (staged, reported) so a policy gap
can't silently lose or leak anything. A policy override can mark specific globs
auto-promote, but staged-by-default is the rule for knowledge: a run editing
`.claude/skills/xxx` may be a throwaway PoC or a keeper — only the user can judge, so
the data always flows back, and the merge is always optional. An optional `strategy`
on a policy rule selects the text-merge algorithm (see FR-4).

## Functional requirements

- **FR-1 Fork.** `pall8t run` (in `isolated` mode) creates an instance via CoW
  (`clonefile(2)` on APFS; rsync fallback). Atomic: a crash mid-fork leaves no
  half-instance. Target <100 ms at 1 GB.
- **FR-2 Policy manifest.** TOML, glob → class rules with defaults; changes on
  unclassified paths are surfaced in the harvest report, never silently handled.
- **FR-3 Harvest (automatic, lossless).** On run end (lazily, per FR-8), the instance's
  non-ephemeral changes are captured into an inbox changeset — diffed against the
  fork-point ancestor, labeled with run name/workspace/time. The instance itself can then
  be disposed. Harvest never touches knowledge paths in the base.
- **FR-4 Promote (explicit, optional).** `pall8t home inbox` lists pending changesets;
  `pall8t home show <run>` displays what changed (optionally with a local-Claude summary —
  "this run added a skill that does X"); `pall8t home promote <run> [paths…]`
  three-way-merges all or *selected paths* of a changeset into the base (fork-point
  ancestor / current base / changeset); `pall8t home drop <run> [paths…]` discards.
  Per-path granularity is
  required: one run may produce one keeper skill and three PoC scraps. Merge strategies
  per class: directory-union for additive trees like skills (conflict only on
  same-path-different-content), textual 3-way for prose/config, key-path JSON merge for
  structured state, **line-union (`git merge-file --union`) for append-only formats like
  `.claude/history.jsonl` — keeps both sides' lines, never conflicts**, latest-wins for
  secrets, and a pluggable resolver hook — which may invoke a **local** Claude as semantic
  merge agent, never a third-party API. The `union` strategy is selectable on any policy
  rule via `strategy = "union"`.
- **FR-5 Conflicts never poison the base.** Conflicts can only arise at promote time,
  under user attention; parallel runs' changesets sit independently in the inbox
  regardless of overlap, and promotion order determines merge order. Clean merges land;
  conflicted paths are quarantined with `conflicts` / `resolve` commands. The base is
  always consistent.
- **FR-6 Serialized base writes.** Per-base lock; concurrent harvests/promotes queue.
  Each base advance is atomic (rename discipline, crash-safe at any `kill -9` point).
- **FR-7 Versioned base.** Every auto-merge or promote is a revision: `log`, `diff`,
  `rollback`. Git is an acceptable internal engine *if* secret paths are excluded from
  its object store.
- **FR-8 Decoupled harvest.** Harvest must not depend on a supervising process, because
  `pall8t run` **exec(2)-replaces itself** (ADR-0006 / NFR-4) and is gone when the run
  ends. Instances persist after exit and harvesting runs lazily — on the next pall8t
  invocation, or an explicit `pall8t home harvest`. An optional supervisor mode
  (spawn+wait) may harvest eagerly, but lazy harvest is the required baseline.
- **FR-9 Instance & inbox lifecycle.** Registry of live instances (`ls`, `rm`, `gc`)
  with orphan detection for dead runs; bounded disk. Inbox changesets persist until
  promoted or dropped; a configurable TTL/GC *warns* rather than silently expiring —
  dropping unreviewed knowledge is a user decision too.
- **FR-10 Credential coherence.** A token refreshed inside one run (Claude Code
  refreshes OAuth tokens) propagates to the base at harvest so later runs don't
  re-login; concurrent refreshes resolve latest-wins without corruption.
- **FR-11 Interface.** A `pall8t home` subcommand family (`inbox`, `show`, `promote`,
  `drop`, `harvest`, `merge`, `log`, `rollback`, `gc`): stable exit codes, `--json` where
  herdr or scripts consume it, no daemon. `merge [<run>]` is a convenience composition of
  harvest + show + promote-all (fold pending runs into the base, printing what each
  changed). Implemented in the pall8t crate (`src/home.rs`), reusing the existing
  subprocess/`git()` and config plumbing.

## Non-functional requirements

- macOS/APFS first; fork cost O(1)-ish metadata, harvest/promote cost O(changed files).
- **Zero network, zero telemetry.** Secrets never appear in logs, diffs, or any external
  process's input.
- No new runtime dependencies beyond what pall8t already has (git, the `container` CLI).
- **`shared` mode must remain byte-for-byte today's behavior** — the module adds a code
  path, never changes the default one; switching a project back from `isolated` to
  `shared` must always be safe (the base is a valid home at every instant, per the core
  model).
- An alternative materialization strategy must fit the same abstraction: since the
  *guest* is Linux, per-run **overlayfs inside the container** (shared base ro lower +
  per-run rw upper) is a legitimate fork mechanism the interface must not preclude.

## Acceptance scenarios

1. Two parallel runs rewrite `.claude.json` → no corruption; durable keys from both
   merge automatically.
2. Run A adds skill X, run B adds skill Y concurrently → both appear as inbox
   changesets; base unchanged; promoting both lands both without conflict.
3. Run adds a PoC skill → visible in inbox, `drop` removes it; the base never saw it.
4. `promote <run> .claude/skills/xxx` merges only that path; the run's other changes
   stay staged.
5. A and B both edit `CLAUDE.md` → no conflict at harvest; promoting the second
   surfaces the conflict for resolution at that moment.
6. Token refreshed in run A → later run C authenticates without login.
7. `kill -9` during fork, harvest, or promote → base intact and consistent; instance or
   changeset recoverable or GC-able.
8. Caches and shell history never reach the base; 1 GB home forks in <100 ms.

## pall8t integration sketch

- `src/home.rs`: mode dispatch. `shared` → today's `container::home_mount()` unchanged;
  `isolated` → fork instance, bind-mount the *instance* as `/home/dev`, exec as today,
  and lazily harvest finished instances on subsequent invocations.
- `src/main.rs`: `pall8t home` subcommand family (FR-11).
- `src/config.rs`: `[home]` section (`mode`, policy overrides, inbox TTL) in the
  existing two-layer merge — per-project opt-in comes for free.

## Mechanics (evaluated)

See [home-compositor-evaluation.md](home-compositor-evaluation.md) for the comparison
tables (off-the-shelf tools vs. this spec, fork mechanics, versioning/merge engine).
Verdicts: no off-the-shelf tool covers class-aware staging plus lazy harvest; FR-1 uses
`clonefile(2)` (via the existing `libc` dependency) with a recursive-copy fallback for
non-APFS; FR-7 uses clonefile snapshots + a manifest with stateless `git merge-file`
for FR-4 text merges — **no VCS repository inside or beside the base**, since a `.git`
in `$HOME` would be visible to every agent; overlayfs-in-guest is rejected as baseline
(virtiofs upper-layer xattr support is not guaranteed by apple/container).

## Implementation plan (accepted 2026-07-07)

The custom pall8t module — the final recommendation of
[home-compositor-evaluation.md](home-compositor-evaluation.md) — is adopted.
Implementation proceeds in phases; **each phase is developed on its own git
branch** and merged via its own PR.

### Phase 1 — MVP: fork, harvest, inbox/promote

- `[home]` config section with `mode = "shared" | "isolated"` (default
  `shared`, current behavior unchanged) — FR-11 subset
- FR-1 fork via `clonefile(2)` with temp-name-plus-rename atomicity (APFS
  required in this phase; clear error on unsupported filesystems)
- FR-2 policy manifest with the conservative default classification
- FR-3 / FR-8 lazy, lossless harvest into inbox changesets
- FR-4 `pall8t home inbox | show <run> | promote <run> [paths…] | drop <run>
  [paths…]` with merge strategies: directory-union (additive knowledge trees),
  textual 3-way via `git merge-file`, key-path JSON (state), latest-wins
  (secrets)
- FR-5 conflicts surface only at promote; FR-6 serialized base writes
  (per-base lock)
- FR-10 credential write-back (latest-wins)

### Phase 2 — history & lifecycle

- FR-7 versioned base: `log`, `diff`, `rollback`
- FR-9 instance registry and lifecycle: `ls`, `rm`, `gc`, orphan detection,
  inbox TTL with warnings (never silent expiry)
- FR-11 polish: `--json` output and stable exit codes across the subcommand
  family

### Phase 3 — extensions

- Pluggable merge resolver (may invoke local Claude; never third-party APIs)
- Non-APFS fallback via recursive copy
- Revisit overlayfs-in-guest if platform xattr guarantees improve
