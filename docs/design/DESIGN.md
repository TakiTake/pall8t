# pall8t — Claude in an apple/container

> **⚠️ DEPRECATED (2026-07-06).** This document describes the TUI architecture abandoned by ADR-0006. pall8t is now a headless CLI; see [docs/requirements.md](../requirements.md). Kept for historical reference.

> Formerly named "cabin" (see ADR-0002). This is **DESIGN v3**: v2 replaced the filer + external terminal tabs with an embedded multiplexer (ADR-0003); v3 re-architects state into per-tab session holders so agents survive TUI exit and multiple pall8t instances stay consistent (ADR-0005, prompted by [#2](https://github.com/TakiTake/pall8t/issues/2)).

A Rust + ratatui TUI that runs AI coding agents (and shells) inside per-project sandboxed dev containers on macOS using [apple/container](https://github.com/apple/container) — with correct host↔container file ownership — and multiplexes them as tabs, herdr-style: you always see which agent is working, which is waiting for your approval, and which is done.

## Why

The workflow pall8t serves: **several tasks in parallel, one AI agent session per task**, so sessions never mix. Tasks often span repositories — one shared platform repo plus per-service repos, with PRs split across them — so a task works in git worktrees cut from several repos at once. Previously this ran on Podman + DevContainer (one DevContainer per project) — a redundant stack on macOS, where apple/container provides lightweight per-container VMs natively.

The barrier to switching is that apple/container's CLI is completely different from docker's, so none of the existing devcontainer tooling can drive it. Docker-CLI-compatibility wrappers over apple/container exist, but emulating docker semantics on top of a different engine is a leaky extra layer; a small purpose-built tool for this one workflow is simpler. pall8t wraps apple/container natively (see ADR-0001) and gives up docker compatibility on purpose.

**Sales point:** pall8t is just a TUI on stdin/stdout — it runs identically in a standalone terminal (Ghostty, iTerm2, …) and inside an IDE's integrated terminal (VS Code etc.). The embedded multiplexer (ADR-0003) is what makes this portable; spawning external terminal tabs could not offer it.

## 1. Goals / Non-goals

**Goals**

- Run AI agents (`claude`, later others) and arbitrary shells inside an apple/container VM, never on the host. This is the core of the tool.
- Run anywhere a terminal runs: standalone or from an IDE's integrated terminal (VS Code etc.). No dependency on a specific terminal app.
- One agent session per task, many tasks per project: tabs are independent `container exec` sessions sharing the project's container.
- A project references **multiple repos**; agents create git worktrees freely in a **host-persistent workspace** that survives container restarts. Canonical checkouts are never writable by agents (see §3, ADR-0004).
- Files created in the mounted project dir are owned by the host user — never root.
- One keep-alive container per project, created lazily, shared by all of that project's tabs.
- Minimal multiplexer: a shortcut creates a new tab running a terminal or an AI agent *inside the TUI*. No panes, no splits, no mouse requirements.
- Agent awareness: each tab's agent is monitored; tabs waiting for approval/input are surfaced in the sidebar, the status bar, and (optionally) a macOS notification. One key jumps to the next tab that needs you.
- **Sessions outlive the TUI:** quitting pall8t (or losing the terminal/IDE window) detaches; agents keep running and reattach on next launch (ADR-0005).
- **Multi-instance safe:** several pall8t processes (IDE terminal + standalone, etc.) share one source of truth; config writes and container lifecycle transitions never race.

**Non-goals**

- File browser / preview (v1 had one; removed — unused in practice).
- Rich multiplexing: panes, splits, workspaces, mouse-driven layout (use herdr/tmux if you need that).
- A central daemon / control-plane RPC. Persistence comes from per-tab holders, coordination from a locked registry file — see ADR-0005 for why this beats a tmux-style server here, and its revisit triggers.
- Notifications while **zero** TUIs are attached (accepted gap; would require a daemon).
- devcontainer.json compatibility (may come later; see Roadmap).

## 2. The UID problem and the strategy

apple/container runs each container in a lightweight VM; bind mounts go through virtiofs. There is no Podman-style `--userns keep-id` yet ([apple/container#165](https://github.com/apple/container/issues/165)). Two failure modes:

1. Container process runs as root → new files in the bind mount show up root-owned.
2. Container process runs as an arbitrary non-root UID → can't write files owned by your host UID inside the mount.

**Strategy: bake the host UID into the image at build time.**

- pall8t builds a per-user image (`pall8t-base:<uid>-<gid>`) from its Containerfile with `--build-arg UID=$(id -u) --build-arg GID=$(id -g)`, creating user `dev` with exactly your host UID/GID, a real home dir, and passwordless sudo.
- Containers are started with `--user dev`, so every process — shell, claude, compilers — creates files as your UID.
- Belt-and-suspenders: `container run/exec --uid/--gid` are also passed.

## 3. Project workspaces and container lifecycle

A project = a name + a list of source repos + a **workspace**: a unique host directory that holds everything agents produce, mounted into the container **at the identical absolute path** (identity-path mount), so git metadata and IDE file links are valid on both sides. Full rationale in ADR-0004.

```
workspace = <workspace_root>/<slug(name)>-<sha256(name)[..8]>/   default root: ~/.pall8t/workspaces
  repos/<repo>/    seeded clone of each source repo — the worktree parent
  wt/              worktrees the agents cut per task

name  = pall8t-<slug(name)>-<sha256(workspace)[..8]>
mounts:
  <workspace>      -> <workspace>   (rw)  the only writable project surface
  ~/.pall8t/home   -> /home/dev     (rw)  persistent agent home: claude auth,
                                          shell history, dotfiles
run:
  container run -d --name <name> \
    -v <workspace>:<workspace> -v ~/.pall8t/home:/home/dev \
    -w <workspace> --user dev --uid <uid> --gid <gid> \
    --cpus <n> --memory <m> \
    pall8t-base:<uid>-<gid> sleep infinity
```

**Seeding.** At project creation pall8t clones each source repo host-side: `git clone <repo> <ws>/repos/<name>`. Same-filesystem clones hardlink objects (fast, near-zero extra space); the origin URL is copied from the source repo so fetch/push work inside the container. Agents then work with plain git: `git -C <ws>/repos/A worktree add ../../wt/task1-A -b task1`. Worktrees persist across container restarts because the whole workspace is a host directory. Seeding also generates a `CLAUDE.md` at the workspace root (only if missing) explaining the layout and worktree workflow to agents — tabs start at the workspace root, outside any repo, where per-repo `.claude/` files are not loaded.

**Why the canonical repos are not mounted:** apple/container has no read-only mounts yet ([#990](https://github.com/apple/container/issues/990)), and `git worktree add` needs to write in the parent repo anyway. Not mounting them protects the real checkouts absolutely. When #990 ships, canonical repos get RO identity-path mounts and seeding switches to `--reference` alternates (ADR-0004).

State machine per project: `Absent → Created/Stopped → Running`. pall8t reconciles on a 2s tick via `container list --all --format json` (absolute CLI path — spawned environments may have a minimal PATH), warns if the `container` system service is not running, and lazily creates/starts on demand: opening a tab on a project with no container triggers build image (if missing) → run → attach, with progress in the status bar. Stopped containers are restarted with `container start`, not recreated.

**Claude auth persistence.** `~/.pall8t/home` is the container-side `$HOME`, so claude login state persists across containers and rebuilds, and is *isolated from the host's* claude credentials. Log in once inside any pall8t tab; done.

## 4. Base image (Containerfile)

```dockerfile
FROM ubuntu:24.04
ARG UID=501
ARG GID=501
# node + claude CLI + common tools; dev user with host UID/GID
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git sudo ripgrep less vim openssh-client tmux && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y nodejs && npm i -g @anthropic-ai/claude-code && \
    (getent group ${GID} || groupadd -g ${GID} dev) && \
    useradd -m -u ${UID} -g ${GID} -s /bin/bash dev && \
    echo 'dev ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/dev
RUN printf '%s\n' \
      '# pall8t: keep the tmux chrome minimal inside agent tabs.' \
      '# Users can override in ~/.tmux.conf (persistent home).' \
      'set -g status off' \
      > /etc/tmux.conf
USER dev
WORKDIR /work
```

**Per-project Containerfile (auto-detected).** If any source repo contains `.pall8t/Containerfile`, pall8t builds the project's image from it instead of the default — tag `pall8t-<project>:<uid>-<gid>` (so the shared base image is not overwritten), build context = that `.pall8t/` directory, same UID/GID build args. Explicit config (`image` / `containerfile`) takes priority over auto-detection. This is how dogfooding works: pall8t's own repo ships a `.pall8t/Containerfile` with a Rust toolchain, so agents inside the container can build pall8t itself.

**Caveat for custom Containerfiles:** the persistent home mount shadows `/home/dev` at runtime, so toolchains must be installed *outside* the home directory (e.g. `RUSTUP_HOME=/usr/local/rustup`, `CARGO_HOME=/usr/local/cargo`, plus an `/etc/profile.d` PATH entry for login shells).

**tmux.** Both images install tmux and ship a minimal `/etc/tmux.conf` (`status off`) so Claude Code's agent-teams split-pane mode (`teammateMode: "auto"`/`"tmux"`) has somewhere to create panes — it only does so when already running inside a tmux session. See README: "Claude Code agent teams (split panes)" for the `agent_command` and settings needed to opt in.

## 5. Multiplexer architecture (session holders)

Two binaries (ADR-0005). A tab is a real PTY running `container exec`, but the PTY is owned by a tiny detached **holder** process, not by the TUI — so tabs survive any TUI's exit.

```
pall8t-tab (holder, one per tab; ~300 lines, treated as frozen)
  - spawns: container exec -it --user dev -w <ws> <container> <cmd> on a PTY
  - keeps a raw-output ring buffer (256 KiB)
  - serves ~/.pall8t/tabs/<tab-id>.sock:
      on attach  -> replay ring buffer, then broadcast live output
      from client -> input bytes, resize (rows, cols)
  - on child exit: marks the registry entry exited, keeps final screen
    available until the tab is closed

pall8t (TUI/attacher, iterates freely)
  - discovers tabs in the registry, connects to their sockets
  - client-side vt100 per tab (fed by replay + live bytes), rendering,
    agent-state detection (§6), terminal-query answering — as in v2
  - key input -> active tab's socket; prefix key (^b) stays local

~/.pall8t/state.json (registry, guarded by flock)
  - tabs: { id, project, kind, title, pid, socket, exited }
  - every mutation (config.toml too): take lock -> re-read -> apply -> write
```

- **Crates:** `portable-pty` + `vt100` as in v2; holders and TUI share the byte-oriented socket protocol (1-byte frame tag + length; versioned, backward compatible — holders from old releases must keep working).
- **Detach/reattach:** `q` or a killed terminal just drops the socket connections. Next `pall8t` reads the registry, reconnects, replays each ring buffer into a fresh vt100, then sends a resize nudge so full-screen apps repaint. Multiple TUIs may attach to the same tab simultaneously (output is broadcast; last resize wins).
- **Input routing:** unchanged — all keys to the active tab, prefix key (default `ctrl+b`) for commands.
- **Tab lifecycle:** `x` kills the holder (and its child). Child exit → `Done` (tab visible until closed). Closing a project's last tab stops its container **only if the registry shows zero live tabs for that project across all instances**; same lock guards recreate-on-image-change.
- **Failure isolation:** a TUI crash detaches (agents unaffected). A holder crash kills only its own tab. Stale registry entries (dead pid / connrefused socket) are pruned on TUI startup under the lock.
- **Config consistency:** the TUI re-reads `config.toml`/registry on mtime change, so a project added in one instance appears in the others.
- **Scrollback:** mouse wheel scrolls the active tab's history (vt100 scrollback, 2000 rows, view-only). Any key snaps back to live; the header shows an indicator while scrolled. If the app inside enabled mouse reporting (vim etc.), wheel events are forwarded to it (SGR encoding) instead. Mouse capture is on by default (`mouse = false` to disable — note it takes over terminal-native text selection, the usual mux trade-off). Detection (§6) always reads the live screen, never the scrolled view. No copy-mode yet.

## 6. Agent status detection

herdr-style awareness, minimal implementation. Two signals, no hooks required:

1. **Output activity:** output within the last ~2s → candidate `Working`.
2. **Screen-bottom pattern matching:** the last N rows of the vt100 screen are matched against per-agent patterns.

| State | Meaning | claude code heuristic |
| :---- | :---- | :---- |
| `Working` | agent is running | spinner row / "esc to interrupt" visible, or recent output |
| `Waiting` ⚠ | **needs you**: approval or input prompt | "Do you want", "❯ 1. Yes", "waiting for your input" at screen bottom |
| `Idle` | at rest, prompt shown | input box with no pending question; shell prompt for Shell tabs |
| `Done` | process exited | child exited |

- Patterns live in config (`[agents.claude]` regex list) so new agents or prompt-format changes don't need a release. Shell tabs only use activity + exit.
- Detection is per-tab: if `agent_command` runs claude under tmux for agent-teams split panes (README: "Claude Code agent teams (split panes)"), the vt100 only sees whichever tmux pane is currently displayed, not the teammates in other panes.
- **Notification surfaces:** sidebar badge per tab, aggregate in the status bar ("⚠ 2 tabs waiting — ^b n"), terminal bell, and optional macOS banner via `osascript -e 'display notification …'` (off by default, `notify = "banner"` to enable). Only fired on transition *into* `Waiting`.
- **`prefix n` jumps to the next `Waiting` tab** — the single most important binding in the tool.

## 7. TUI layout

![pall8t TUI mockup](tui-mockup.svg)

Left sidebar: projects with container state, and their tabs with agent state. Main area: the active tab's terminal, full fidelity. Bottom: alert/status line + keybar. The sidebar can be hidden (`prefix z`) to give the terminal the full width.

**Keymap** (prefix = `ctrl+b` by default, configurable; press prefix, release, then the key)

| Key | Action |
| :---- | :---- |
| `a` | New **agent** tab (claude) in current project — builds/starts the container as needed |
| `c` | New **shell** tab in current project |
| `n` | Jump to next tab in `Waiting` state |
| `j` / `k`, `1`–`9` | Next / previous tab, jump to tab N |
| `p` / `P` | Next project / add project (path prompt) |
| `x` | Close tab (confirm if child still running); stops the container if it was the project's last tab |
| `s` / `b` / `L` | Start/stop container / rebuild image / container logs |
| `z` | Toggle sidebar |
| `[` | Scrollback view (q to exit) |
| `?` | Help overlay |
| `q` | Detach: quit the TUI, agents and containers keep running (reattach = relaunch `pall8t`) |

## 8. Config

`~/.config/pall8t/config.toml`:

```toml
default_image = "pall8t-base"    # tag suffix :<uid>-<gid> is appended
cpus = 4
memory = "4G"
prefix = "ctrl+b"
notify = "bell"                  # off | bell | banner (macOS notification)
mouse = true                     # wheel = scrollback (false: leave mouse to the terminal)
agent_command = "claude"         # what `prefix a` runs
# agent_command = "tmux new -A -s claude claude"  # Claude Code agent-teams split panes (README)
workspace_root = "~/.pall8t/workspaces"

[[projects]]
name = "checkout-flow"
repos = [
  "/Users/you/src/platform",
  "/Users/you/src/svc-payment",
]
# image = "my-custom:dev"        # optional per-project override
# containerfile = "Containerfile"  # relative to the workspace
# (without these, <repo>/.pall8t/Containerfile is auto-detected — see §4)

[agents.claude]
waiting_patterns = ["Do you want", "❯ 1\\. Yes"]
working_patterns = ["esc to interrupt"]
```

`pall8t .` adds the cwd as a single-repo project (named after the directory), seeds its workspace, selects it, and opens an agent tab.

## 9. Security notes

- Container has no access to host beyond the two mounts (project workspace, pall8t home). Canonical repo checkouts are not mounted at all. SSH agent (`--ssh`) is opt-in per project, off by default.
- Host claude credentials never enter the container; the sandboxed claude has its own login.
- `sudo` inside the container is convenience only — root in the VM guest, not on the host; virtiofs writes still land as the mapped host-side owner.
- YOLO-mode claude (`--dangerously-skip-permissions`) becomes reasonable here: blast radius is the project dir + throwaway VM. With `Waiting` detection you can also run permission-mode claude across many tabs without babysitting each one.

## 10. Roadmap

1. **v2.0 (shipped):** tab multiplexer (PTY + vt100), agent/shell tabs, claude `Waiting`/`Working` detection, sidebar + status bar + bell/banner, `prefix n`, multi-repo workspaces with seeding, `.pall8t/Containerfile` auto-detection, terminal-query answering.
2. **v3.0 (next):** session-holder re-architecture (ADR-0005): `pall8t-tab` holder binary, registry + flock coordination, detach/reattach, multi-instance safety, `q` = detach.
3. **v3.1:** detection patterns for more agents (codex etc.), workspace repo sync/refresh command, container stats in sidebar; RO identity-path mounts of canonical repos once [apple/container#990](https://github.com/apple/container/issues/990) ships.
4. **v3.2:** copy-mode in scrollback, port publish UI, `--ssh` toggle.
5. **Later:** minimal devcontainer.json subset, socket API for scripting (revisit trigger for a central daemon, ADR-0005).

## 11. Architecture decision records

Decisions live in [`docs/adr/`](../adr/); this section only summarizes them.

**[ADR-0001: Implementation language — Rust over Swift](../adr/0001-implementation-language.md)** (Accepted, 2026-07-04). pall8t stays in Rust and integrates with apple/container by wrapping the `container` CLI rather than linking the Swift `ContainerClient` library. Key reasons: the XPC API is pre-1.0 and unstable (v0 compatibility was already removed in 0.12.x), so linking it creates a client/apiserver version-skew problem that CLI wrapping avoids entirely; the CLI provides JSON output (`container ls --format json`, `inspect`) for robust parsing; and the Rust TUI ecosystem (ratatui/crossterm/clap) has no Swift equivalent. Revisit if apple/container 1.0 ships a versioned XPC API *and* pall8t needs features the CLI can't provide — escape hatch is a small Swift helper binary (XPC in, JSON out), not a rewrite.

**[ADR-0002: Rename cabin → pall8t](../adr/0002-rename-to-pall8t.md)** (Accepted, 2026-07-04). "cabin" was too generic (crates.io/search collisions); "pall8t" keeps the pallet-under-containers metaphor, is collision-free, and stays pronounceable.

**[ADR-0003: Pivot to an embedded agent multiplexer](../adr/0003-multiplexer-pivot.md)** (Accepted, 2026-07-04). Prototype testing showed the filer was unused and external terminal tabs (Ghostty et al.) made agent monitoring impossible — and broke the IDE-integrated-terminal usage mode. v2 drops the filer and embeds a minimal herdr-style multiplexer: tabs with real PTYs (`portable-pty` + `vt100` + `tui-term`) running `container exec`, plus heuristic `Waiting`/`Working` detection and notifications. Supersedes v1's "no embedded terminal emulation" non-goal. Depending on herdr/tmux underneath was rejected (heavy dependency, and the container-exec integration + agent monitoring is the product).

**[ADR-0005: Per-tab session holders instead of a central daemon](../adr/0005-session-holders.md)** (Accepted, 2026-07-05, prompted by [#2](https://github.com/TakiTake/pall8t/issues/2)). Config clobbering, no detach, and cross-instance container races all stem from state living in one foreground process. Rather than a tmux-style central daemon (single point of failure for every agent session, upgrade kills sessions, RPC + lifecycle machinery), each tab gets a tiny frozen `pall8t-tab` holder process owning its PTY (ring buffer + per-tab socket, dtach-style), and mutations go through a flock-protected registry. `q` becomes detach. A central daemon remains the documented escalation path if detached-state notifications or a socket API become requirements.

**[ADR-0004: Multi-repo project workspaces with identity-path mounts](../adr/0004-workspace-model.md)** (Accepted, 2026-07-04). A project references multiple repos; agents work in a host-persistent workspace mounted at the identical absolute path inside the container, seeded with hardlink clones of each repo. Canonical checkouts are not mounted (apple/container lacks RO mounts, [#990](https://github.com/apple/container/issues/990); `git worktree add` writes to the parent repo anyway) — worktrees are cut from the workspace clones and survive container restarts. Switch to RO mounts + `--reference` alternates when #990 ships.
