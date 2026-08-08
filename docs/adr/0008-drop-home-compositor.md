# ADR-0008: Drop the home compositor — one shared container home

- Status: Accepted
- Date: 2026-08-08
- Supersedes: [docs/specs/home-compositor.md](../specs/home-compositor.md) (Accepted 2026-07-07), its [evaluation](../specs/home-compositor-evaluation.md) and [design](../design/home-compositor.md)
- Does **not** affect: home-directory isolation from the *host* (`~/.pall8t/home` mounted as the container home) — that is requirements §1.2/NFR-3 and stays exactly as ADR-0006 left it

## Context

The home compositor shipped in 0.3.0 as an experimental, off-by-default `[home] mode = "isolated"`: each run got a CoW fork of `~/.pall8t/home`, its changes were harvested into an inbox when the run ended, and the user folded selected paths back into the base with `pall8t home promote` — plus revision history, rollback, and instance lifecycle management (`pall8t home log|diff|rollback|ls|rm|gc`).

It answered a real question (requirements §8 roadmap item 1: parallel runs sharing one home). But in practice it went unused. The one config that ever opted in was this repo's own `.pall8t/config.toml` (`mode = "isolated"`, removed by this change) — pall8t developing pall8t. No other project enabled it, and the default stayed `shared` everywhere else. What it cost meanwhile was not small:

- ~4,700 lines — `src/home.rs` plus its test module — in a 10,200-line `src/`, i.e. **46%** of the code serving a feature essentially nobody ran.
- A 12-subcommand CLI surface (`pall8t home …`) with its own exit-code contract (`2` = unresolved conflict), JSON output shapes, and locking/crash-recovery semantics, all of which every future change had to keep working.
- A `[home]` config section with a path-classification policy language (`secret | state | knowledge | ephemeral`, merge strategies, glob overrides) that leaked into `config.rs`'s types.
- Genuinely subtle machinery to keep correct: base swap under an advisory lock with crash repair, 3-way JSON merge, secret redaction in diffs, tombstone sweeping, pid-liveness heuristics.

Unused code is not free: it is reviewed, linted, mutation-tested, refactored, and — worst — it shapes the design of everything near it.

## Decision

Remove the home compositor entirely. `~/.pall8t/home` is mounted rw as `/home/dev` for every run, unconditionally — byte-for-byte what `mode = "shared"` always did.

- `src/home.rs` and its tests are deleted; `pall8t home` is gone as a command.
- The `[home]` config section is still **parsed and ignored**, and a section that *sets something* prints a one-line warning naming the file to clean up. A stale `mode = "isolated"` must not silently become a no-op while the user believes runs are still isolated — but it also must not fail a run, since it only ever selected an off-by-default experiment. A bare `[home]` header with everything commented out — what 0.3.0's `init` skeletons wrote, and `init` never rewrites an existing file — sets nothing and stays quiet; warning on the header alone would nag every `init` user forever about a feature they never enabled.
- On-disk leftovers (`~/.pall8t/instances`, `inbox`, `revisions`) are not touched by pall8t, and the warning tells the user to copy out what they want before deleting them rather than calling them safe to remove (see Consequences). The base `~/.pall8t/home` is untouched and remains the container home.
- The exit-code `2` conflict contract disappears with the commands that produced it. `0`/`1` are unaffected. Note that `2` does not vanish from the CLI: clap exits `2` on a usage error, so a script still calling `pall8t home merge` now gets `2` meaning "no such subcommand" rather than "unresolved conflict". A script that branched on `2` therefore misreads the new failure, which is one more reason the removal is a minor-version bump with a CHANGELOG entry rather than a silent drop.

The spec, evaluation, and design documents are kept with a deprecation banner rather than deleted — the same treatment ADR-0006 gave the TUI design docs. The problem they analyze is still open (requirements §8 roadmap item 1), and the evaluation of off-the-shelf tools is the most reusable part of the work.

## Alternatives considered

- **Keep it, off by default** — rejected: that is exactly the status quo whose cost this ADR is about. An unused feature that is never exercised also rots quietly; the first user to enable it would be the one to find the rot.
- **Keep the module, drop only the CLI** — rejected: the CLI *is* the feature. Forking without promote/harvest leaves each run's knowledge stranded, which is worse than sharing.
- **Delete the `[home]` section outright (hard parse error)** — rejected: it breaks every config that ever set the section, for an experiment that was never dangerous. Warning is the honest middle.
- **Delete the spec/design docs too** — rejected: the underlying problem is unsolved and the evaluation (which tools were considered and why each failed the requirements) is worth more than the implementation was.

## Consequences

- The crate roughly halves. `config.rs` loses the policy types; `main.rs` loses the whole `home` subcommand tree and the `ConflictError` exit-code mapping.
- Parallel runs share one home again, with the `~/.claude.json` corruption caveat that requirements §7 already documented as a known v1 limitation. That limitation is now unqualified — there is no opt-out. This repo's own workflow is the first to feel it: agent teams developing pall8t run several sandboxes at once against one `~/.pall8t/home`. Accepted knowingly — the corruption is an upstream Claude Code issue that parallel agents hit on the host just as readily, and it never bit here badly enough to make anyone reach for the compositor, which is itself part of why it went unused.
- Requirements §8 roadmap item 1 returns to being open, with this ADR as the record that one implementation was built, shipped, and withdrawn for lack of use. A future attempt should start from a user actually asking for it.
- Users who ran `isolated` mode and never harvested a run keep whatever is in `~/.pall8t/instances/<run>/root/`, and any changeset they never promoted stays in `~/.pall8t/inbox/` — pall8t no longer reads either, but nothing is deleted, so both can be copied out by hand. The deprecation warning says exactly this and deliberately stops short of calling the directories safe to delete: the old `gc` never removed an unreviewed changeset on the grounds that "dropping unreviewed knowledge is always a user decision", and losing the feature is no reason to reverse that. `~/.pall8t/revisions/*/snapshot/` is a full copy of the base home, so it can contain credential files — worth reviewing before deleting, and worth deleting rather than leaving indefinitely.
