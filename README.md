# pall8t

*(pronounced "pallet" — the thing containers ship on)*

Run AI coding agents inside [apple/container](https://github.com/apple/container) sandboxes, several at once, and always know which one needs you. pall8t is a minimal agent multiplexer TUI (Rust + ratatui): each tab is a real PTY exec'd into a per-project container; the sidebar shows every agent's state — working, **waiting for your approval**, idle, done.

![pall8t TUI mockup](docs/design/tui-mockup.svg)

## Why

- **One agent session per task, in parallel.** Tabs are independent `container exec` sessions; sessions never mix.
- **Sandboxed by construction.** Agents run in a lightweight VM, never on the host. Files land as *your* UID, never root. Host credentials never enter the container.
- **Multi-repo projects.** A project references several repos; agents cut git worktrees in a host-persistent workspace that survives container restarts (mounted at the identical path inside the container).
- **Runs anywhere a terminal runs** — standalone or inside an IDE's integrated terminal (VS Code etc.).
- **Agent awareness, herdr-style.** Waiting-for-approval detection with bell/banner notification and one-key jump (`^b n`).
- **Sessions survive the TUI.** Each tab lives in a tiny detached `pall8t-tab` holder process: quit pall8t (or close the IDE window) and agents keep running; relaunching reattaches. Multiple pall8t instances share one source of truth (ADR-0005).

Replaces a Podman + DevContainer stack on macOS without pretending to be docker (see `docs/adr/`).

## Requirements

- macOS on Apple silicon, [apple/container](https://github.com/apple/container) installed and started (`container system start`)
- git, Rust toolchain (build from source for now)

## Install & run

```sh
cargo install --path .
pall8t .        # add the current repo as a project, seed its workspace, open an agent tab
```

## Keys

Press the prefix (`ctrl+b` by default), release, then:

| Key | Action |
| :---- | :---- |
| `a` / `c` | New agent tab / shell tab in the current project |
| `n` | Jump to the next tab **waiting for you** |
| `j` / `k`, `1`–`9` | Cycle tabs / jump to tab N |
| `p` / `P` | Cycle project / add project (comma-separated repo paths) |
| `x` | Close tab, killing the agent inside (stops the container when it was the project's last tab, across all instances) |
| `s` / `b` / `L` | Start-stop container / rebuild image / logs |
| `z` | Toggle sidebar |
| `?` | Help |
| `q` | Detach: quit the TUI, agents and containers keep running; relaunch `pall8t` to reattach |

All other keys go straight to the active tab's terminal. The mouse wheel scrolls the active tab's history (any key returns to live; apps that use the mouse themselves, like vim, get the wheel forwarded instead). Set `mouse = false` in config to keep terminal-native text selection.

## Config

`~/.config/pall8t/config.toml` — see [docs/design/DESIGN.md](docs/design/DESIGN.md) for the full design and [docs/adr/](docs/adr/) for architecture decisions.

If a repo contains `.pall8t/Containerfile`, pall8t builds that project's image from it automatically (this repo ships one with a Rust toolchain, so agents can develop pall8t inside pall8t). Toolchains in custom Containerfiles must live outside `/home/dev` — the persistent home mount shadows it. The built image is tagged with the Containerfile's last commit hash, so committing a change to it makes pall8t rebuild automatically the next time the project's image is needed.

## Claude Code agent teams (split panes)

Claude Code can show teammate agents as tmux split panes (`teammateMode: "auto"` / `"tmux"`), but it only creates panes if it's already running inside a tmux session — pall8t's image ships tmux for exactly this.

1. Rebuild the image after upgrading (`prefix b` in pall8t) so tmux is available.
2. In `~/.config/pall8t/config.toml`, set:
   ```toml
   agent_command = "tmux new -A -s claude claude"
   ```
3. Inside the container, one-time (persists in the container home): add `"teammateMode": "auto"` to `~/.claude/settings.json`.

**Prefix collision:** pall8t's default prefix (`ctrl+b`) is also tmux's. Pressing `ctrl+b` twice from a pall8t tab passes the second one through to tmux, but for comfortable pane navigation it's worth changing one side — e.g. `prefix = "ctrl+q"` in `config.toml`, or remap tmux's prefix in `~/.tmux.conf` inside the container.

**Behavior notes:**
- `-A -s claude` means the tmux session persists in the container and re-attaches when the tab is reopened; open only one agent tab per project when using this, since a second tab would just mirror the same session.
- pall8t's waiting/working state detection (§6 in DESIGN.md) is per-tab, not per-teammate-pane — it only sees the pane tmux happens to show.
- The image ships `/etc/tmux.conf` with `status off`; override in `~/.tmux.conf` inside the container if you want the status bar back.
