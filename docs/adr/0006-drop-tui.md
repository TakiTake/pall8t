# ADR-0006: Drop the TUI — pall8t becomes a headless sandbox runner

- Status: Accepted
- Date: 2026-07-06
- Supersedes: ADR-0003 (embedded multiplexer), ADR-0005 (per-tab session holders)
- Amends: ADR-0004 (project-workspace model replaced; hardlink-clone protection retained)

## Context

pall8t's original goal was narrow: run AI agents inside apple/container sandboxes without the redundant Docker Desktop stack. Feature by feature — filer, embedded multiplexer (ADR-0003), detach/reattach via session holders (ADR-0005) — it drifted toward reimplementing herdr, and each addition made it a worse herdr rather than a better sandbox runner.

Stepping back, every session-management feature pall8t grew already has a mature owner:

- Multiplexing, agent-status visibility → herdr
- Session persistence, attach/detach → tmux / herdr
- Workspace isolation for parallel tasks → git worktrees, cut by the user or by herdr

None of these need to live inside pall8t if pall8t is a well-behaved foreground CLI that herdr or tmux can spawn. Meanwhile the two things no other tool provides remain unowned: an apple/container abstraction purpose-built for running AI agents, and Containerfile-change detection with automatic rebuild.

Session-holder architecture (ADR-0005) also carried real cost: a second frozen binary, a byte protocol with compatibility guarantees, a flock-protected registry, and file-based coordination — all serving capabilities the outer multiplexer already has.

## Decision

Remove the TUI and all session management. pall8t becomes a headless CLI specialized in running AI agents on apple/container:

- **Foreground process model.** `pall8t run` executes the agent in a container with TTY passthrough, signal forwarding, and correct exit codes. Session lifetime equals process lifetime; persistence is the caller's business (tmux / herdr).
- **No daemons, no holders, no registry.** `pall8t-tab`, `state.json`, and the attach protocol are deleted. Automatic rebuild is implemented as a Containerfile hash check at `run` time, not a watch process.
- **Workspace = cwd.** The project/workspace concept of ADR-0004 is dropped. pall8t mounts the current directory rw; if cwd is a git worktree, the main repository's `.git` is auto-mounted so git works inside the container. Cutting worktrees is the caller's responsibility.
- **Reference repos keep ADR-0004's protection insight.** Repos listed in `pall8t.toml` are duplicated via `git clone --local` (hardlinked objects) and the copy is mounted — protection by duplication, still compensating for apple/container's missing RO mounts ([apple/container#990](https://github.com/apple/container/issues/990)).
- **Home isolation stays.** `~/.pall8t/home` is mounted rw as the container home, keeping host `~/.claude` untouched.

Full requirements in [docs/requirements.md](../requirements.md).

## Alternatives considered

- **Keep the TUI, integrate with herdr anyway** — rejected: two overlapping UIs, and every TUI feature competes with herdr's roadmap from behind.
- **Keep session holders without the TUI** (headless CLI + detach) — rejected: tmux/herdr already hold sessions; the holder layer's cost (frozen binary, protocol compatibility, registry locking) buys nothing the outer mux doesn't provide.
- **Keep the ADR-0004 workspace model** — rejected: it existed so the TUI could own multi-repo task setup. With worktree creation delegated to the caller, cwd-as-workspace plus worktree-aware mounting covers the same workflow with far less machinery.

## Consequences

- Codebase shrinks to one binary. ratatui, vt100, PTY management, and the holder protocol are removed.
- P1–P3 from ADR-0005 dissolve rather than being solved: no shared TUI state exists. The remaining shared-state risk is `~/.pall8t/home` under parallel runs (same risk as parallel agents on the host; accepted, see requirements §7).
- tmux/herdr affinity becomes a testable requirement: TTY passthrough, signal forwarding, exit codes, `ls --json` for machine consumption.
- The knowledge learned building v0–v3 (identity-path mounts, hardlink clones, uid/gid mapping) carries into the runner.

## Revisit triggers

- herdr or tmux integration proves insufficient for agent-status awareness → consider emitting structured status events for the outer mux to consume (still no TUI).
- apple/container#990 (RO mounts) resolved → mount reference repos ro directly (roadmap item 2).
