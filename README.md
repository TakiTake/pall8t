# pall8t

*(pronounced "pallet" — the thing containers ship on)*

Run AI coding agents inside [apple/container](https://github.com/apple/container) sandboxes. pall8t is a headless CLI that does one job well: launch an agent in a macOS-native lightweight VM, with your current directory mounted as the workspace, an isolated container home, and automatic image rebuilds when the Containerfile changes. Multiplexing, session persistence, and workspace isolation belong to the tools that already own them (tmux, herdr, git worktrees) — pall8t is the well-behaved foreground process they spawn (see [ADR-0006](docs/adr/0006-drop-tui.md)).

## Why

- **Sandboxed by construction.** Agents run in a per-container VM, never on the host. Files land as *your* UID, never root.
- **No Docker Desktop.** apple/container is macOS-native and boots fast, but its CLI is docker-incompatible; pall8t abstracts it into exactly what running an agent needs.
- **Host non-pollution.** `~/.pall8t/home` is mounted as the container home — the agent's login and config live there, and your host `~/.claude` is never touched.
- **Automatic rebuilds, no daemon.** At `run` time the Containerfile's content hash picks the image tag; a change means a rebuild before launch. No watch process, no state file.
- **Worktree-aware.** If your cwd is a git worktree, the main repository's `.git` is mounted too, so git inside the container works exactly as on the host.
- **Reference repos, protected by duplication.** Repos listed in config are duplicated via `git clone --local` (hardlinked objects) and the *copy* is mounted at the original's path — the workaround for apple/container's missing read-only mounts ([apple/container#990](https://github.com/apple/container/issues/990)).

## Requirements

- macOS on Apple silicon, [apple/container](https://github.com/apple/container) installed
- git, Rust toolchain (build from source for now)

## Install & quickstart

```sh
cargo install --path .

cd ~/src/my-project
pall8t init     # one-time: ~/.pall8t/home, config skeletons, default Containerfile
pall8t run      # build if needed, then run the agent (default: claude) in the sandbox
```

The first `pall8t run` needs a one-time agent login inside the container; credentials persist in `~/.pall8t/home` across runs and rebuilds.

The agent session is a plain foreground process: run it under tmux or herdr for persistence and multiplexing, `Ctrl-C`/signals reach the agent, and the exit code is the agent's own.

## CLI

```
pall8t init              # generate ~/.pall8t/home, config skeletons, default Containerfile
pall8t run [-- cmd...]   # hash check → build if needed → run (TTY passthrough)
pall8t build             # explicit (unconditional) build
pall8t ls [--json]       # list pall8t containers (--json for herdr etc.)
pall8t exec <id> -- cmd  # run a command inside a running container
pall8t stop <id>         # stop a container
```

## Config

Two layers, merged per field with the project winning: global `~/.pall8t/config.toml`, per-project `./pall8t.toml`.

```toml
[container]
cpus = 4
memory = "8g"
containerfile = "Containerfile"   # relative to the project dir

[run]
command = ["claude"]     # --dangerously-skip-permissions is NOT in the default.
                         # Users who want it must set it explicitly.

[[repos]]                # reference repos: cloned with `git clone --local`,
source = "~/src/other-lib"   # the copy is mounted at this same path
```

Containerfile resolution: explicit `containerfile` config → `./Containerfile` if present → the built-in default image (node + claude CLI + gh; materialized once at `~/.pall8t/Containerfile` and never overwritten — edit it to customize the shared default, delete it to restore the shipped one). Custom toolchains must live outside `/home/dev` — the persistent home mount shadows it.

The image tag embeds the Containerfile's content hash, so any edit — no commit required — triggers a rebuild on the next `run`, and superseded images are pruned automatically after a successful build (images still used by a running container are kept). Only the Containerfile itself is hashed, not files it `COPY`s in; use `pall8t build` to force a rebuild the hash can't see (e.g. updated base image or packages).

## Working with git worktrees

Cutting worktrees is the caller's business — you or herdr — but pall8t makes them work inside the sandbox:

```sh
git -C ~/src/my-project worktree add ../my-project-task -b task
cd ~/src/my-project-task
pall8t run
```

pall8t detects that cwd's `.git` is a worktree pointer and identity-mounts the main repository's `.git` alongside, so `status`/`commit`/`diff` inside the container behave exactly as on the host.

## Claude Code agent teams (split panes)

Claude Code can show teammate agents as tmux split panes (`teammateMode: "auto"` / `"tmux"`), but only if it's already running inside a tmux session — the default image ships tmux for exactly this. In config:

```toml
[run]
command = ["tmux", "new", "-A", "-s", "claude", "claude"]
```

then, inside the container (one-time, persists in the container home), add `"teammateMode": "auto"` to `~/.claude/settings.json`. Note that tmux here only multiplexes *within* one run: every `pall8t run` starts a fresh container (`--rm`), so there is no session to re-attach across runs — for persistence, run pall8t itself under tmux on the host. The image ships `/etc/tmux.conf` with `status off`; override in `~/.tmux.conf` inside the container if you want the status bar back.

## Known limitations (v1)

- **Shared home under parallel runs.** All containers share `~/.pall8t/home` rw; Claude Code has known `~/.claude.json` corruption issues under concurrent sessions — the same conditions as parallel agents on the host. Accepted for v1 (per-run home clones are roadmap item 1).
- **No read-only mounts** (apple/container limitation) — reference repos are protected by `git clone --local` duplication instead.
- **Workspace isolation is the caller's responsibility.** Two agents in the same directory will step on each other; use worktrees.

Full requirements in [docs/requirements.md](docs/requirements.md); architecture decisions in [docs/adr/](docs/adr/).
