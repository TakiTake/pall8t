# pall8t — Claude in an apple/container

> Formerly named "cabin" (see ADR-0002). This is **DESIGN v2**: the v1 filer was removed and the external-terminal-tab model was replaced by an embedded multiplexer after prototype testing (see ADR-0003).

A Rust + ratatui TUI that runs AI coding agents (and shells) inside per-project sandboxed dev containers on macOS using [apple/container](https://github.com/apple/container) — with correct host↔container file ownership — and multiplexes them as tabs, herdr-style: you always see which agent is working, which is waiting for your approval, and which is done.

## Why

The workflow pall8t serves: **several tasks in parallel on the same git repo, one AI agent session per task**, so sessions never mix. Previously this ran on Podman + DevContainer (one DevContainer per project) — a redundant stack on macOS, where apple/container provides lightweight per-container VMs natively.

The barrier to switching is that apple/container's CLI is completely different from docker's, so none of the existing devcontainer tooling can drive it. Docker-CLI-compatibility wrappers over apple/container exist, but emulating docker semantics on top of a different engine is a leaky extra layer; a small purpose-built tool for this one workflow is simpler. pall8t wraps apple/container natively (see ADR-0001) and gives up docker compatibility on purpose.

**Sales point:** pall8t is just a TUI on stdin/stdout — it runs identically in a standalone terminal (Ghostty, iTerm2, …) and inside an IDE's integrated terminal (VS Code etc.). The embedded multiplexer (ADR-0003) is what makes this portable; spawning external terminal tabs could not offer it.

## 1. Goals / Non-goals

**Goals**

- Run AI agents (`claude`, later others) and arbitrary shells inside an apple/container VM, never on the host. This is the core of the tool.
- Run anywhere a terminal runs: standalone or from an IDE's integrated terminal (VS Code etc.). No dependency on a specific terminal app.
- One agent session per task, many tasks per project: tabs are independent `container exec` sessions sharing the project's container.
- Files created in the mounted project dir are owned by the host user — never root.
- One keep-alive container per project, created lazily, shared by all of that project's tabs.
- Minimal multiplexer: a shortcut creates a new tab running a terminal or an AI agent *inside the TUI*. No panes, no splits, no mouse requirements.
- Agent awareness: each tab's agent is monitored; tabs waiting for approval/input are surfaced in the sidebar, the status bar, and (optionally) a macOS notification. One key jumps to the next tab that needs you.

**Non-goals**

- File browser / preview (v1 had one; removed — unused in practice).
- Rich multiplexing: panes, splits, workspaces, mouse-driven layout (use herdr/tmux if you need that).
- Detach/reattach server persistence. If pall8t exits, its exec sessions end (containers keep running; agents inside a tab die with the PTY). Accepted for now — revisit trigger in ADR-0003.
- devcontainer.json compatibility (may come later; see Roadmap).

## 2. The UID problem and the strategy

apple/container runs each container in a lightweight VM; bind mounts go through virtiofs. There is no Podman-style `--userns keep-id` yet ([apple/container#165](https://github.com/apple/container/issues/165)). Two failure modes:

1. Container process runs as root → new files in the bind mount show up root-owned.
2. Container process runs as an arbitrary non-root UID → can't write files owned by your host UID inside the mount.

**Strategy: bake the host UID into the image at build time.**

- pall8t builds a per-user image (`pall8t-base:<uid>-<gid>`) from its Containerfile with `--build-arg UID=$(id -u) --build-arg GID=$(id -g)`, creating user `dev` with exactly your host UID/GID, a real home dir, and passwordless sudo.
- Containers are started with `--user dev`, so every process — shell, claude, compilers — creates files as your UID.
- Belt-and-suspenders: `container run/exec --uid/--gid` are also passed.

## 3. Container lifecycle

One keep-alive container per project:

```
name  = pall8t-<slug(dirname)>-<sha256(abs_path)[..8]>    e.g. pall8t-myapp-3fa9c21b
mounts:
  <project abs path>   -> /work        (the project)
  ~/.pall8t/home       -> /home/dev    (persistent shared home: claude auth,
                                        shell history, dotfiles survive rebuilds)
run:
  container run -d --name <name> \
    -v <project>:/work -v ~/.pall8t/home:/home/dev \
    -w /work --user dev --uid <uid> --gid <gid> \
    --cpus <n> --memory <m> \
    pall8t-base:<uid>-<gid> sleep infinity
```

State machine per project: `Absent → Created/Stopped → Running`. pall8t reconciles on a 2s tick via `container list --all --format json` (absolute CLI path — spawned environments may have a minimal PATH), warns if the `container` system service is not running, and lazily creates/starts on demand: opening a tab on a project with no container triggers build image (if missing) → run → attach, with progress in the status bar. Stopped containers are restarted with `container start`, not recreated.

**Claude auth persistence.** `~/.pall8t/home` is the container-side `$HOME`, so claude login state persists across containers and rebuilds, and is *isolated from the host's* claude credentials. Log in once inside any pall8t tab; done.

## 4. Base image (Containerfile)

```dockerfile
FROM ubuntu:24.04
ARG UID=501
ARG GID=501
# node + claude CLI + common tools; dev user with host UID/GID
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates curl git sudo ripgrep less vim openssh-client && \
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash - && \
    apt-get install -y nodejs && npm i -g @anthropic-ai/claude-code && \
    (getent group ${GID} || groupadd -g ${GID} dev) && \
    useradd -m -u ${UID} -g ${GID} -s /bin/bash dev && \
    echo 'dev ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/dev
USER dev
WORKDIR /work
```

Users can point `image` in config at their own Containerfile per project; pall8t passes the same UID/GID build args.

## 5. Multiplexer architecture

Each tab is a real PTY running `container exec` — the agent's own screen, rendered inside the TUI.

```
tab = {
  pty:    portable-pty PtyPair, child = container exec -it --user dev -w /work <name> <cmd>
  parser: vt100::Parser (screen state, fed from a reader thread)
  kind:   Shell | Agent(claude)
  state:  Working | Waiting | Idle | Done   (see §6)
}
```

- **Crates:** `portable-pty` (PTY + child process), `vt100` (terminal state machine), `tui-term` (ratatui widget rendering a vt100 screen). All battle-tested together; no custom escape-sequence handling.
- **Input routing:** all keys go to the active tab's PTY, except the prefix key (default `ctrl+b`, configurable). Prefix, release, then an action key — one reserved key keeps pall8t out of the shell's way (same model as tmux/herdr).
- **Reader threads:** one thread per tab reads PTY output, feeds the parser, records `last_output_at`, and wakes the UI. Writes go through the PTY master from the UI thread.
- **Resize:** terminal area size changes propagate to every PTY via `TIOCSWINSZ` (portable-pty `resize`).
- **Tab lifecycle:** child exit → state `Done` (tab stays visible with its final screen until closed). Closing the last tab of a project does not stop the container.
- **Scrollback:** vt100's built-in scrollback, view-only (prefix `[` enters scroll mode, `q` leaves). No copy-mode in v1.

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
| `x` | Close tab (confirm if child still running) |
| `s` / `b` / `L` | Start/stop container / rebuild image / container logs |
| `z` | Toggle sidebar |
| `[` | Scrollback view (q to exit) |
| `?` | Help overlay |
| `q` | Quit pall8t (confirm if any tab is `Working`/`Waiting`; containers keep running) |

## 8. Config

`~/.config/pall8t/config.toml`:

```toml
default_image = "pall8t-base"    # tag suffix :<uid>-<gid> is appended
cpus = 4
memory = "4G"
prefix = "ctrl+b"
notify = "bell"                  # off | bell | banner (macOS notification)
agent_command = "claude"         # what `prefix a` runs

[[projects]]
name = "myapp"
path = "/Users/you/src/myapp"
# image = "my-custom:dev"        # optional per-project override
# containerfile = ".pall8t/Containerfile"

[agents.claude]
waiting_patterns = ["Do you want", "❯ 1\\. Yes"]
working_patterns = ["esc to interrupt"]
```

`pall8t .` adds the cwd as a project, selects it, and opens an agent tab.

## 9. Security notes

- Container has no access to host beyond the two mounts (project dir, pall8t home). SSH agent (`--ssh`) is opt-in per project, off by default.
- Host claude credentials never enter the container; the sandboxed claude has its own login.
- `sudo` inside the container is convenience only — root in the VM guest, not on the host; virtiofs writes still land as the mapped host-side owner.
- YOLO-mode claude (`--dangerously-skip-permissions`) becomes reasonable here: blast radius is the project dir + throwaway VM. With `Waiting` detection you can also run permission-mode claude across many tabs without babysitting each one.

## 10. Roadmap

1. **v2.0 (next prototype):** tab multiplexer (PTY + vt100 + tui-term), agent/shell tabs, claude `Waiting`/`Working` detection, sidebar + status bar + bell, `prefix n`, container lifecycle carried over from v1.
2. **v2.1:** macOS banner notifications, detection patterns for more agents (codex etc.), per-project `.pall8t/Containerfile` auto-detection, container stats in sidebar.
3. **v2.2:** copy-mode in scrollback, port publish UI, `--ssh` toggle.
4. **Later:** session restore (reopen tab set), minimal devcontainer.json subset, socket API for scripting.

## 11. Architecture decision records

Decisions live in [`docs/adr/`](../adr/); this section only summarizes them.

**[ADR-0001: Implementation language — Rust over Swift](../adr/0001-implementation-language.md)** (Accepted, 2026-07-04). pall8t stays in Rust and integrates with apple/container by wrapping the `container` CLI rather than linking the Swift `ContainerClient` library. Key reasons: the XPC API is pre-1.0 and unstable (v0 compatibility was already removed in 0.12.x), so linking it creates a client/apiserver version-skew problem that CLI wrapping avoids entirely; the CLI provides JSON output (`container ls --format json`, `inspect`) for robust parsing; and the Rust TUI ecosystem (ratatui/crossterm/clap) has no Swift equivalent. Revisit if apple/container 1.0 ships a versioned XPC API *and* pall8t needs features the CLI can't provide — escape hatch is a small Swift helper binary (XPC in, JSON out), not a rewrite.

**[ADR-0002: Rename cabin → pall8t](../adr/0002-rename-to-pall8t.md)** (Accepted, 2026-07-04). "cabin" was too generic (crates.io/search collisions); "pall8t" keeps the pallet-under-containers metaphor, is collision-free, and stays pronounceable.

**[ADR-0003: Pivot to an embedded agent multiplexer](../adr/0003-multiplexer-pivot.md)** (Accepted, 2026-07-04). Prototype testing showed the filer was unused and external terminal tabs (Ghostty et al.) made agent monitoring impossible. v2 drops the filer and embeds a minimal herdr-style multiplexer: tabs with real PTYs (`portable-pty` + `vt100` + `tui-term`) running `container exec`, plus heuristic `Waiting`/`Working` detection and notifications. Supersedes v1's "no embedded terminal emulation" non-goal. Depending on herdr/tmux underneath was rejected (heavy dependency, and the container-exec integration + agent monitoring is the product).
