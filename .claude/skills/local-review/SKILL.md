---
name: local-review
description: Review the current branch's diff locally BEFORE opening or updating a PR, so external reviewers (CodeRabbit, Codex, humans) find nothing you could have caught yourself. Use when asked to review local changes, before commit-push-pr, or when about to open a PR. Applies a lens checklist grown from real review findings; report findings with concrete failure scenarios, fix them, and re-run until clean.
---

# Local review

Goal: the external reviewers on the PR should come back empty-handed.
Every finding a bot catches after push is one this review should have
caught before it. When one slips through anyway, this file learns it —
see "Grow the lenses" at the bottom.

## 0. Scope and mechanical gates first

```sh
git diff --stat origin/main...HEAD        # committed changes
git status --short                        # uncommitted / untracked too
```

Review everything that will end up in the PR, not just the last commit.
Before reading anything, run the machine's share — style and lint
findings are its job, not this review's:

```sh
scripts/lint.sh && cargo test
```

For nontrivial logic changes, also spot-run mutation testing on the
changed files (`cargo mutants -f src/<changed>.rs`) — a test that
wouldn't go red is a finding.

## 1. Review through each lens

Read the full diff once for orientation, then pass through the lenses
below. A finding must name a **concrete failure scenario** (inputs/state
→ wrong outcome); "this looks risky" without one is not a finding.
These lenses are generalized from real external-review findings that
local review failed to catch first (source PRs noted).

**Policy and trust boundaries** (PR #38)
- Any allow/deny decision: is the *default* direction safe when the
  input is unknown or novel? An enumerated denylist silently allows the
  next thing an upstream adds — prefer default-deny by namespace/class
  with explicit carve-outs, or document why transparency wins.
- New listeners/endpoints: bound as narrowly as possible (specific
  interface, not `0.0.0.0`)? Who can reach it besides the intended peer,
  and what gates them?
- Anything downloaded and later executed or mounted: integrity-verified
  on every use, not just at fetch time? Where does the verification
  record live — can the thing being verified overwrite its own record?
- Fail open vs fail closed: when setup fails partway, which side does
  the feature land on, and does the user find out?

**Silent fallbacks** (PR #38, #43)
- Config parsing: does a misspelled key/value fall back silently to a
  default — and is that default the *permissive* one? Security-relevant
  settings must fail the parse (`deny_unknown_fields`), not degrade.
- Any `unwrap_or(default)` / `.ok()` on user intent: same question.
- **Type-narrowed presence checks** (PR #43): a check that asks "is this
  setting present?" through a type filter (`as_table()`, `as_str()`,
  `downcast`, a match on one variant) answers "absent" for every value of
  the wrong shape. `home = "isolated"` is a plausible miswrite of
  `[home] mode = "isolated"`; narrowing with `as_table()` made it invisible.
  Enumerate what the *user* could plausibly have typed, not what the happy
  path expects — and when the branch exists to tell the user something,
  the wrong-shape case usually needs telling too.
- **A deprecation/ignored-setting warning is itself user intent**: check
  which commands reach the print. A warning only wired into the main path
  is silent exactly where a confused user looks first (a `doctor`-style
  diagnostic).

**Fallback to a secondary tool** (PR #50)
- A fallback that invokes a manager/helper tool (`rustup`, `nvm`, a
  package manager) to repair the active environment must first check the
  tool actually *governs* that environment — presence on PATH is not
  governance. Enumerate the split-brain states: manager present but
  managing nothing (its commands fail, and under `set -e` that aborts the
  script on exactly the path meant to be forgiving), and manager managing
  a *different* copy than the one on PATH (the "fix" lands where nothing
  reads it). lint.sh hit both with a leftover rustup next to the nix
  toolchain; the check that ties them together is whether the active
  tool's root (`rustc --print sysroot`) lives under the manager's home.

**Docs the diff touches** (PR #43)
- Markdown lint is part of the review even when CI doesn't run it:
  adjacent blockquotes separated by a blank line (MD028) is the one this
  repo has actually hit, from prepending a banner to a doc that already
  opened with a `>` note. Continue with `>` rather than deleting the blank
  line — deleting it merges two distinct notes into one paragraph.
- A banner or status line added to a doc must not contradict a claim
  further down the same doc, or the ADR/CHANGELOG describing the change.

**Guards that infer ownership from current state** (PR #72)
- A guard that decides "may I overwrite this?" by reading the value as it
  stands now cannot recognize the writes it made *itself* on an earlier
  run — so it protects only the first pass, and every run after it takes
  the wrong branch. pall8t skipped renaming a tab whose label wasn't
  herdr's auto label; after its own first run the label was no longer the
  auto one, so run 2 read its own handiwork as a human's and left the tab
  pointing at a name that by then belonged to a different agent.
  Enumerate the second run explicitly, not just the first.
- Where the actor can't tell its own past writes apart from a third
  party's (the derivation changed between runs, so the old value is
  unreachable), the residual case doesn't disappear — decide what the
  user is told. Silence there means the divergence is created without
  anyone seeing it.

**Two sites claiming the same bound** (PR #72)
- When a limit constant is spelled out in more than one walk, check they
  actually agree — off-by-one between "the list we offer" and "the list
  we try" is invisible in review and in normal runs. `candidates` yielded
  `name` plus `2..=N` (N names); the retry loop ran `for counter in
  2..=N` around a mutable accumulator and tried N-1. Prefer one iterator
  both sites share; where they can't share, a test asserting equal length
  is the pin.

**Probes that decide destructive actions** (PR #62)
- A health/liveness probe whose "no" triggers a delete, kill, or unlink
  must distinguish *the peer answered no* from *the probe failed to ask*.
  `connect(…).is_ok()`, `exists()`, `ping == 0`, a nonzero exit — each
  collapses both into one bit. Enumerate the errnos: `ECONNREFUSED` and
  "not found" mean gone; `EMFILE`, `EACCES`, a timeout mean the prober is
  having a bad day, and acting on those destroys live state (a running
  sandbox's bridge socket, in the case that named this lens). Unknown
  must fall on the keep side, the way an unreadable mtime already does.
- The same rule scales down: any `unwrap_or(false)` on an IO probe is a
  policy decision about the failure case, so state which case it is.

**Interpolating into a delimited wire format** (PR #62)
- Building `a:b`, `k=v`, `x,y` for another process? The delimiter must be
  rejected (or escaped) at the **boundary that constructs the value**, not
  asserted in a comment. Two real ones in one PR: a `:` in `$HOME` turns a
  two-field `-v host:guest` into a three-field one whose third field is
  parsed as mount options, and an `=` in a project path makes
  apple/container's label parser throw and fail the whole run.
- Read the consumer's parser before deciding what is safe (`split(":")`,
  `maxSplits`, whether extra fields error or are silently accepted). "This
  form has no third field to mistype" is a claim about *inputs*, and only
  becomes true when something enforces it.
- Values that can't be escaped (paths) get a guard that fails the
  feature; values that can be sanitized (labels, display strings) get
  sanitized — never let provenance metadata break a launch.

**Internal and hidden entry points** (PR #62)
- `#[command(hide = true)]` hides a subcommand from `--help`; it does not
  stop anyone from typing it. If the command chmods, unlinks, or sweeps a
  directory, validate that the directory is the one it is supposed to own
  before doing any of it — `--listen /tmp/x.sock` should not chmod `/tmp`
  to 0700 on its way to failing.
- Spawned children on error paths: a `bail!` between `spawn` and `wait`
  leaks the child. Kill it if it is still doing something (it may be
  holding a socket nothing will read), and `wait` it either way so it
  isn't left a zombie under whatever this process execs into.

**Concurrency and shared state** (PR #38)
- Fixed-name temp files, caches, or locks that two concurrent runs can
  collide on (pall8t runs in parallel panes by design). Per-pid names +
  atomic rename; define what the second writer publishing means.
- State shared across a mount/process boundary: who can mutate it
  between your check and your use?

**Claims vs code** (PR #38)
- Do ADR/README/skill statements match what the code does *in the
  degraded paths* — missing optional dependency, failed setup, mode
  switched off? Docs written before the code hardened are the usual
  drift site. Read the doc diff against the code diff, not against
  memory.
- Preconditions stated as absolutes ("X works inside the sandbox")
  that are actually conditional — qualify them.

**CI workflows** (PR #39)
- `actions/checkout`: `persist-credentials: false` unless the job pushes.
- Event triggers: does the trigger set cover the *loop*, not just the
  first pass (e.g. `synchronize` for re-reviews)? If a trigger is
  omitted deliberately (cost), say so inline or it reads as a bug.
- Listing APIs (`gh api`): `--paginate` or the first page silently
  truncates.
- Report-only jobs: `continue-on-error` where a report must not gate.

**Impact beyond the diff** (standing — use the LSP tool)
- For every public function, type, or constant whose signature, contract,
  or semantics the diff changes: enumerate ALL call/use sites with the
  LSP tool (`incomingCalls` for functions, `findReferences` otherwise) —
  not grep, which misses renames, method syntax, and re-exports. Then
  check each site the diff does *not* touch: does it still hold under the
  new behavior? An updated callee with an unexamined caller is a finding.
- Changed enum/match-heavy types: `findReferences` on the type, looking
  for match sites that a new variant or changed default now reaches.
- Position gotcha: `documentSymbol` reports a symbol's range *including
  its doc comment*; position-based operations (`hover`, `incomingCalls`,
  `findReferences`) need the line/character of the identifier itself, so
  read the declaration line first. If the LSP server is unavailable
  (e.g. it needs a session restart after a crash loop), say so in the
  review output and fall back to `rg` — silently degraded coverage reads
  as full coverage.

**What the commit actually contains** (PR #46)
- `git status --short` is step 0 for a reason: read the staged file list
  before committing, not just the diff you meant to write. `git add -A`
  after running a tool sweeps in whatever that tool wrote —
  `cargo mutants` leaves `mutants.out/` and `mutants.out.old/`, and
  `mutants.out/lock.json` carries the developer's hostname and username.
  An *incidental* local output like that belongs in `.gitignore` before
  it belongs in a commit — which is not the same as "never commit
  generated files": plenty of repos version lockfiles, schemas, or
  generated sources on purpose, and those stay in review like any other
  code. The question is whether the file is a deliberate artifact of the
  project or a byproduct of the tool you happened to run. On an unmerged
  branch, amend rather than adding a removal commit — a "remove the
  artifact" commit still merges the artifact's contents into `main`'s
  history forever.
- A committed report also *attracts* review findings about its own
  contents (bots read `missed.txt` as if it were source), which buries
  the real findings.

**Changing a default** (PR #46)
- A default change is not one behavior change, it is one per input shape
  the old default used to handle. Enumerate them: which layouts, formats,
  or states did the old path absorb that the new one meets directly?
  Read-only mounts broke *linked git worktrees* as reference repos —
  their `.git` is a pointer file to a path outside the source — precisely
  because the old path laundered them through `git clone --local` and the
  new one does not.
- A new mechanism may not inherit the properties of the one it replaces.
  Check identity, permissions, and ownership across the boundary, not
  just the behavior you were aiming at: apple/container applies its
  uid/gid remapping to writable mounts only, so read-only mounts arrive
  root-owned and git refuses them — the feature "worked" while being
  useless for its main purpose.
- Verify the new default on the real runtime with the tool the users use
  (`git`, not `cat`). A read check that only opens a file will not notice
  that every VCS operation fails.

**Lifecycle claims in docs** (PR #46)
- Before writing what happens to something "at the end of the run" —
  discarded, cleaned up, disposable, temporary — find the code that
  removes it. If there is no such code, it persists. The clone under
  `~/.pall8t/repos` was called disposable in four documents while
  `prepare`'s own doc comment two lines away said an existing one is
  reused as-is.

**Tests** (standing)
- Per docs/testing.md: new tests use table form with reasoned assertion
  messages; each bug fix and each refuted review finding gets a pin;
  contracts (argv shapes, wire formats, exit codes) are asserted
  structurally.

## 2. Fix, don't file

This is a pre-PR review: findings are yours to fix now, in the same
branch, each with its pinning test where applicable. Re-run the gates and
re-pass the lenses over what changed. Declining a lens's advice is fine —
but then the code or doc must carry the rationale, because the external
reviewers will raise it otherwise and the PR reply will just repeat what
should have been written down.

## 3. Grow the lenses

When an external reviewer (bot or human) later finds something real that
this review missed, don't just fix it — generalize it into a lens above
(or sharpen an existing one) with the source PR noted, in the same PR
that fixes the finding. That's the ratchet: each external catch should be
a category caught locally forever after. Findings that were refuted don't
become lenses; they become clearer code (see the review-loop skill).
