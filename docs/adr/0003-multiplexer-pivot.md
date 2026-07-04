# ADR-0003: Pivot to an embedded agent multiplexer (drop the filer)

- Status: Accepted
- Date: 2026-07-04
- Supersedes: the "no embedded terminal emulation" non-goal of DESIGN v1

## Context

Prototype testing of v1 produced two findings:

1. The read-only filer (file tree + preview) went unused. Its job is done better by tools inside the container tab.
2. Spawning *external* terminal tabs (Ghostty/iTerm2/…) works, but pall8t then has zero visibility into them. The core use case — running several AI agents in sandboxed containers concurrently — needs [herdr](https://github.com/ogulcancelik/herdr)-style awareness: which agent is working, which is blocked on an approval prompt, which is done. That requires owning the agents' terminals.

herdr itself is close but is a general multiplexer (server/client detach model, workspaces, panes, mouse-native, socket API) and knows nothing about apple/container. pall8t's value is the container sandbox; it needs only a thin slice of multiplexing.

## Decision

1. **Remove the filer** entirely.
2. **Embed a minimal multiplexer**: tabs only — no panes, splits, workspaces, or detach server. A shortcut opens a new tab running either a shell or an AI agent, always via `container exec` into the project's keep-alive container.
3. **Tech:** one PTY per tab via `portable-pty`, screen state via `vt100`, rendered with `tui-term` (ratatui widget). Input uses a tmux-style prefix key (default `ctrl+b`, configurable).
4. **Agent monitoring:** heuristic state per tab — `Working` / `Waiting` (approval or input needed) / `Idle` / `Done` — from output activity plus regex matching on the bottom rows of the vt100 screen. Patterns are config-driven per agent. Transitions into `Waiting` notify via sidebar badge, status bar, bell, and optional macOS banner; `prefix n` jumps to the next `Waiting` tab.

## Alternatives considered

- **Run herdr/tmux underneath and drive it** — rejected: heavy runtime dependency, AGPL coupling concerns (herdr), and the integration surface (container lifecycle + agent state) is exactly the part we'd still have to build. The multiplexer slice we need is small.
- **Keep external tabs, monitor from outside** — rejected: no reliable way to read another terminal app's screen; approval prompts are invisible; notifications impossible.

## Consequences

- v1's "real terminal tabs are better than embedded emulation" stance is reversed; fidelity now depends on the `vt100` crate. tui-term + vt100 is a proven stack, and herdr demonstrates full-screen agent TUIs render fine in-terminal.
- If pall8t exits, its exec sessions (and the agents in them) die; containers keep running. Detach/persistence is explicitly out of scope.
- The Ghostty/iTerm2/WezTerm/kitty spawn module and the filer are deleted from the codebase.

## Revisit triggers

- Demand for detach/reattach → consider a background server *or* recommend running pall8t inside herdr/tmux (they compose: pall8t handles containers + agent state, the outer mux handles persistence).
- Heuristic detection proving brittle → adopt agent-native signals (e.g. Claude Code hooks writing state to a file in the shared home mount).
