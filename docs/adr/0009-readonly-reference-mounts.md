# ADR-0009: Read-only reference-repo mounts

- Status: Accepted
- Date: 2026-08-08
- Supersedes, in part: the "apple/container has no read-only mounts" premise in [ADR-0004](0004-workspace-model.md) (Constraints, Alternatives) and [ADR-0006](0006-drop-tui.md) (Decision, Follow-ups), and the corresponding entries in [docs/requirements.md](../requirements.md) §7/§8. The identity-path insight both ADRs are built on is unaffected and stays.
- Does **not** affect: the workspace mount, the container home, or the herdr bridge's binary mount (see Consequences)

## Context

ADR-0004 recorded that apple/container had no read-only mount support ([apple/container#990](https://github.com/apple/container/issues/990), then an open feature request) and chose *protection by duplication*: each reference repo is cloned with `git clone --local` and the copy is mounted at the original's absolute path, so an agent that writes damages only a disposable clone. ADR-0006 kept that when the TUI went away, and both left the same follow-up: *"#990 resolved → mount reference repos ro directly."*

The trigger fired, earlier than the docs assumed. #990 closed 2026-01-07, before 1.0.0 was ever tagged — so pall8t had been carrying a workaround for a constraint that no shipped version it supported actually had.

Since the ADRs record the limitation as verified, the fix needed verification of equal weight rather than a reading of upstream. Measured on apple/container 1.2.2, mounting a scratch directory and writing to it from inside an `alpine` container:

| mount flag | result |
| --- | --- |
| `--mount type=virtiofs,source=…,target=…,ro` | `Read-only file system` on both write and create |
| `-v src:dst:ro` | `Read-only file system` |
| `-v src:dst:readonlyy` (typo) | **mounts read-write, silently** |
| `--mount …,readonlyy` (typo) | `Error: unknown directive readonlyy`, before the container starts |
| `-v src:dst` (baseline) | writes through, as expected |

Enforcement is real, and the two flags are not equivalent. `Parser.volume` splits `-v` on `:` and hands the third field to the filesystem options unvalidated, while `Parser.mount` validates each `--mount` directive and throws on an unknown one.

## Decision

**Reference repos are mounted read-only by default**, and every mount pall8t emits goes out as `--mount`, never `-v`.

- `[[repos]] readonly` (default `true`) selects the mode per entry; `pall8t run --repos-readonly[=BOOL]` overrides every entry for one run. Precedence: flag, then entry, then the read-only default.
- `readonly = false` keeps the old behavior exactly — a `git clone --local` copy mounted at the source's path — because it is still the only way for an agent to commit or fetch in a reference repo.
- `~/.pall8t/repos` is created only when some entry actually asks for a copy.

Read-only is the default because a reference repo exists to be read: duplication was an approximation of that, and the runtime now provides the real thing more cheaply. `--mount` over `-v` is a safety choice, not a style one — a typo in a protection flag must fail the run rather than silently produce the unprotected mount, and the table above shows `-v` doing exactly that.

The `--reference` alternates half of ADR-0004's deferred design is **not** adopted, and is now moot: it belonged to the workspace-seeding model that ADR-0006 removed with the TUI. Nothing in today's pall8t clones a repo into a workspace, so there are no alternates to point anywhere.

## Alternatives considered

- **Keep duplication as the default, read-only opt-in** — rejected: it makes the weaker protection the one users get by accident, and keeps a clone-and-manage subsystem on the default path to deliver less.
- **Read-only with no escape hatch, deleting the clone machinery** — rejected: an agent that must `git fetch` or commit in a reference repo has no route left, and the machinery being deleted is small, already written, and already tested.
- **Mount the source read-only *and* keep a clone for writes** — rejected: two mounts of one repo at two paths, and the agent has to know which is which. The whole value of the identity path is that there is one answer for where a repo lives.
- **`-v src:dst:ro`** — rejected on the evidence above. A misspelled option mounting read-write in silence is precisely the failure a protection flag must not have.

## Consequences

- **`git fetch` and commits inside a read-only reference repo now fail** with `EROFS`, where before they silently succeeded against a disposable copy. That is a behavior change in the honest direction — those commits were always thrown away at the end of the run — but a user who relied on the copy needs `readonly = false`. The CHANGELOG says so, and each run prints which protection each repo got.
- **No clone, no clone maintenance.** A read-only entry costs no disk, no `git clone --local` on first use, and cannot go stale against its source — the previous copy was refreshed only by deleting it by hand.
- **The `[[repos]]`-overlaps-the-workspace check still applies**, and its error message now covers both modes: mounted as a copy the agent's commits are swallowed; mounted read-only the live checkout turns read-only underneath the agent.
- **Every mount changed flag**, including the workspace and the container home. They were `-v host:dest` and are now `--mount type=virtiofs,source=…,target=…`. Same semantics, validated parsing, and no dependence on `:` not appearing in a path. `run_argv_shape` pins the exact strings.
- **The herdr bridge is deliberately untouched.** ADR-0007 copies the verified Linux `herdr` binary per run and mounts the copy writable *because* nothing could be mounted read-only; with that constraint gone, the shared verified cache could be mounted directly and the per-run copy and its pruning retired. That changes the bridge's threat model and wants a live herdr session to test against, so it is left as a follow-up rather than changed in passing.
- **A floor under the runtime.** Read-only enforcement was verified on 1.2.2; the `,ro` directive itself parses in 1.0.0 and later, so this does not raise the floor on its own. A separate change (issue #41, part 2) moves the documented baseline to 1.2.0 for an unrelated reason — the image-config env leak fixed by apple/container#2027.
