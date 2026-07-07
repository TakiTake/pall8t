# Spec: agent-home state compositor (working name: `hearth`)

Status: draft, 2026-07-07. Addresses [issue #9](https://github.com/TakiTake/pall8t/issues/9)
(shared home under parallel runs — requirements §8 roadmap item 1). Background: the
[bit-vcs/bit evaluation](https://github.com/TakiTake/pall8t/issues/9#issuecomment-4904121268)
on that issue, whose conclusions (no third-party APIs near the home dir, selective state
management rather than version-controlling `$HOME`) this spec bakes in.

**One-liner:** Give each sandboxed agent run a private, instantly-forked copy of a shared
home directory, capture everything it produced when it ends, and let the user decide what
folds back into the shared home — even from many parallel runs.

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
| `state` | `.claude.json` durable keys | copied | auto structural (key-path JSON) merge — mechanical, no value judgment |
| `knowledge` | `.claude/skills/`, memory, `CLAUDE.md`, `settings.json` | copied | **staged in inbox; merged only on explicit promote** |
| `ephemeral` | `.npm`, `.cache`, `.bash_history`, locks | fresh or copied | discarded, never merged |

Unclassified paths default to a conservative class (staged, reported) so a policy gap
can't silently lose or leak anything. A policy override can mark specific globs
auto-promote, but staged-by-default is the rule for knowledge: a run editing
`.claude/skills/xxx` may be a throwaway PoC or a keeper — only the user can judge, so
the data always flows back, and the merge is always optional.

## Functional requirements

- **FR-1 Fork.** `hearth fork` creates an instance via CoW (`clonefile(2)` on APFS;
  rsync fallback). Atomic: a crash mid-fork leaves no half-instance. Target <100 ms at
  1 GB.
- **FR-2 Policy manifest.** TOML, glob → class rules with defaults; changes on
  unclassified paths are surfaced in the harvest report, never silently handled.
- **FR-3 Harvest (automatic, lossless).** On run end (lazily, per FR-8), the instance's
  non-ephemeral changes are captured into an inbox changeset — diffed against the
  fork-point ancestor, labeled with run name/workspace/time. The instance itself can then
  be disposed. Harvest never touches knowledge paths in the base.
- **FR-4 Promote (explicit, optional).** `hearth inbox` lists pending changesets;
  `hearth show <run>` displays what changed (optionally with a local-Claude summary —
  "this run added a skill that does X"); `hearth promote <run> [paths…]` three-way-merges
  all or *selected paths* of a changeset into the base (fork-point ancestor / current
  base / changeset); `hearth drop <run> [paths…]` discards. Per-path granularity is
  required: one run may produce one keeper skill and three PoC scraps. Merge strategies
  per class: directory-union for additive trees like skills (conflict only on
  same-path-different-content), textual 3-way for prose/config, key-path JSON merge for
  structured state, latest-wins for secrets, and a pluggable resolver hook — which may
  invoke a **local** Claude as semantic merge agent, never a third-party API.
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
  ends. Instances persist after exit and harvesting runs lazily — on the next
  `hearth`/`pall8t` invocation, or an explicit `harvest` call. An optional supervisor
  mode (spawn+wait) may harvest eagerly, but lazy harvest is the required baseline.
- **FR-9 Instance & inbox lifecycle.** Registry of live instances (`ls`, `rm`, `gc`)
  with orphan detection for dead runs; bounded disk. Inbox changesets persist until
  promoted or dropped; a configurable TTL/GC *warns* rather than silently expiring —
  dropping unreviewed knowledge is a user decision too.
- **FR-10 Credential coherence.** A token refreshed inside one run (Claude Code
  refreshes OAuth tokens) propagates to the base at harvest so later runs don't
  re-login; concurrent refreshes resolve latest-wins without corruption.
- **FR-11 Interface.** CLI-first: stable exit codes, `--json` output, no daemon;
  embeddable as a Rust library is a plus (pall8t is Rust).

## Non-functional requirements

- macOS/APFS first; fork cost O(1)-ish metadata, harvest/promote cost O(changed files).
- **Zero network, zero telemetry.** Secrets never appear in logs, diffs, or any external
  process's input.
- Single static binary (or a pall8t module); no runtime beyond optionally git.
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

`pall8t run`: fork instance → bind-mount the *instance* as `/home/dev` (replacing
today's shared `home_mount()` in `src/container.rs`) → exec as today. The next pall8t
invocation harvests finished instances. Policy lives in the existing two-layer config
(`~/.pall8t/config.toml` / `pall8t.toml`).

## Next step

Use this spec as the yardstick to evaluate candidates — jj/git-based approaches,
overlayfs-in-guest, dotfile managers (chezmoi), Nix home-manager — versus a custom
pall8t module. Expectation to test: no off-the-shelf tool covers class-aware staging
(FR-3/FR-4) together with lazy harvest (FR-8), which would make `clonefile` + policy
manifest + git-backed versioning inside pall8t the realistic outcome.
