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
- git

## Install & quickstart

```sh
brew install TakiTake/tap/pall8t
```

(published together with the v0.1.0 release; until then, or to build from source, use `cargo install --path .` — this needs the Rust toolchain.)

```sh
cd ~/src/my-project
pall8t init     # one-time: ~/.pall8t/home, .pall8t/config.toml skeleton, default Containerfile
pall8t run      # build if needed, then run the agent (default: claude) in the sandbox
```

The first `pall8t run` needs a one-time agent login inside the container; credentials persist in `~/.pall8t/home` across runs and rebuilds.

The agent session is a plain foreground process: run it under tmux or herdr for persistence and multiplexing, `Ctrl-C`/signals reach the agent, and the exit code is the agent's own.

## CLI

```
pall8t init              # generate ~/.pall8t/home, .pall8t/config.toml skeleton, default Containerfile
pall8t run [-- cmd...]   # hash check → build if needed → run (TTY passthrough)
pall8t build             # explicit (unconditional) build
pall8t ls [--json]       # list pall8t containers (--json for herdr etc.)
pall8t exec <id> -- cmd  # run a command inside a running container
pall8t stop <id>         # stop a container
pall8t herdr doctor [--json]  # check herdr env/socket/binary reachability
```

## Config

Two layers, merged per field with the project winning: global `~/.pall8t/config.toml`, per-project `.pall8t/config.toml` — the project-scope mirror of `~/.pall8t`.

```toml
[container]
cpus = 4
memory = "8g"
# containerfile = "path/to/other/Containerfile"   # relative to the project dir; default: .pall8t/Containerfile

[run]
command = ["claude"]     # --dangerously-skip-permissions is NOT in the default.
                         # Users who want it must set it explicitly.

[[repos]]                # reference repos: cloned with `git clone --local`,
source = "~/src/other-lib"   # the copy is mounted at this same path
```

Containerfile resolution: explicit `containerfile` config → `.pall8t/Containerfile` if present → the built-in default image (node + claude CLI + gh; materialized once at `~/.pall8t/Containerfile` and never overwritten — edit it to customize the shared default, delete it to restore the shipped one). There is no fallback to a root `./Containerfile` — that file usually belongs to the project's own app image, so pall8t never picks it up implicitly; point `containerfile` at it explicitly if you really want that. The build context is always the resolved Containerfile's own directory, so a `.pall8t/Containerfile` can only `COPY` files that live under `.pall8t/`. Custom toolchains must live outside `/home/dev` — the persistent home mount shadows it.

The image tag embeds the Containerfile's content hash, so any edit — no commit required — triggers a rebuild on the next `run`, and superseded images are pruned automatically after a successful build (images still used by a running container are kept). Only the Containerfile itself is hashed, not files it `COPY`s in; use `pall8t build` to force a rebuild the hash can't see (e.g. updated base image or packages).

A build streams `container build`'s own output live to stderr — no `-v` flag, this is always on, since a silent multi-minute build looks hung. Deliberately kept off pall8t's own stdout, which `pall8t build`'s final `built <tag>` line and `pall8t ls --json` need to stay machine-readable.

## Working with git worktrees

Cutting worktrees is the caller's business — you or herdr — but pall8t makes them work inside the sandbox:

```sh
git -C ~/src/my-project worktree add ../my-project-task -b task
cd ~/src/my-project-task
pall8t run
```

pall8t detects that cwd's `.git` is a worktree pointer and identity-mounts the main repository's `.git` alongside, so `status`/`commit`/`diff` inside the container behave exactly as on the host.

## herdr integration

Launch `pall8t run` via `herdr agent start` — e.g. a shell function like `p8() { herdr agent start "claude-$$" --cwd "$PWD" -- pall8t run "$@" }` — and pall8t bridges the sandbox boundary for you. (Setting `HERDR_AGENT=<name>` is optional — see the agent-state bullet below for when it applies.) The `agent start` name must be unique per herdr session — a fixed name like plain `claude` fails a second concurrent `p8` with `agent_name_taken` — so `$$` (the shell's own pid) disambiguates; it doesn't need to be pretty, since pall8t's sidebar-identity report below overrides the *displayed* name anyway. herdr injects `HERDR_ENV`/`HERDR_PANE_ID`/`HERDR_SOCKET_PATH`/`HERDR_BIN_PATH` into `pall8t` itself (the host process), not into the sandboxed `claude`, so pall8t acts on them before it execs into the container. **`herdr agent start` isn't optional**: it's what gives the pane an agent identity at all (`herdr`'s own `set_agent_name`, at pane-creation time) — plain `pall8t run` typed into an ordinary pane gets no herdr-visible identity whatsoever, sandboxed or not, since herdr's fallback detection (`identify_agent_in_job`) only inspects the **host** process tree and only ever sees the `container` client process, never the sandboxed `claude` inside the VM:

- **Agent state (idle/working/blocked).** `pall8t run` execs the `container` client with `argv[0]` set to the sandboxed agent's name — the first name in the run command that herdr recognizes (`claude`, `codex`, `gemini`, …; launchers like `env`/`npx`/`uv run` and `pkg@version` specs are looked through), falling back to `HERDR_AGENT` when the command contains none. Only recognized names are ever derived: anything else (a wrapper script, a shell one-liner) yields no guess instead of a wrong one. `HERDR_AGENT` never overrides a name found in the command — the command is what actually runs, while an env var baked into a shell function is ambient and would mislabel e.g. `p8 -- codex` as claude; pall8t prints a note when it ignores one. Homebrew installs `container` as a bash exec wrapper whose inner `exec` would rewrite argv[0] to the Cellar path and destroy the hint (observed live), so pall8t looks through such a pass-through wrapper and execs its target binary directly, carrying over the wrapper's env assignments (`CONTAINER_INSTALL_ROOT`); it also sets `HERDR_AGENT` on the process so a future herdr macOS env hint could survive even argv0-rewriting setups. herdr assigns pane identity from the host process tree by argv0 basename (on macOS via `sysctl(KERN_PROCARGS2)`), and that identity is what unlocks its screen-content state detection — which then works on the sandboxed agent as well as a native one, because the agent's real UI streams through the pane's PTY unchanged. Without the hint, herdr only ever sees a process named `container`, never recognizes the pane, and tracks no state. (herdr's own `HERDR_AGENT` env-hint is Linux-only — it reads `/proc/<pid>/environ` — which is why pall8t honors the variable itself on macOS.) Side effect: `ps` shows the host-side client process under the agent's name; its executable is still `container`.
- **Sidebar identity.** pall8t reports the pane's display name to herdr (`herdr pane report-metadata … --display-agent "<agent> (pall8t)"`), which takes priority over the plain `claude` name `agent start` gave the pane — verified live end-to-end (agent pane shows "claude (pall8t)"). When no agent name could be determined, no report is sent at all — the pane keeps the name `agent start` gave it rather than being labeled with a guess. Deliberately sent *without* `--agent`: herdr only surfaces `display_agent` when it matches `effective_agent_label()` (host-process-derived, per above) — a match that only holds once the argv0 hint has taken effect, and this report must not depend on it. Best-effort either way: a missing `herdr` binary or unreachable socket just prints a warning and the run continues.
- **No redundant tmux wrapper.** If `[run] command` is the [Claude Code agent-teams tmux wrapper](#claude-code-agent-teams-split-panes) below and pall8t detects it's running inside a herdr pane, it runs plain `claude` instead — herdr already supplies persistence/multiplexing, so the wrapper (and its status bar) is redundant chrome. An explicit `pall8t run -- <cmd>` override always wins over this.
- **`pall8t herdr doctor`** checks whether pall8t can see and reach the herdr pane it's running under (env vars present, socket reachable, `herdr` binary resolvable). Read-only, diagnostic only; `--json` for scripting.

Native session resume/restore (`pall8t resume`, live session-id reporting via `herdr pane report-agent-session`) isn't implemented yet: it needs a change to pall8t's foreground/exec-replace process model, and, upstream, a way for herdr to let a custom source supply its own resume command instead of its current hardcoded `claude --resume <id>` table. Tracked in [issue #18](https://github.com/TakiTake/pall8t/issues/18).

## Claude Code agent teams (split panes)

Claude Code can show teammate agents as tmux split panes (`teammateMode: "auto"` / `"tmux"`), but only if it's already running inside a tmux session — the default image ships tmux for exactly this. In config:

```toml
[run]
command = ["tmux", "new", "-A", "-s", "claude", "claude"]
```

then, inside the container (one-time, persists in the container home), add `"teammateMode": "auto"` to `~/.claude/settings.json`. Note that tmux here only multiplexes *within* one run: every `pall8t run` starts a fresh container (`--rm`), so there is no session to re-attach across runs — for persistence, run pall8t itself under tmux on the host. The image ships `/etc/tmux.conf` with `status off`; override in `~/.tmux.conf` inside the container if you want the status bar back.

## Known limitations (v1)

- **Shared home under parallel runs (default mode only).** The default `[home] mode = "shared"` has all containers share `~/.pall8t/home` rw; Claude Code has known `~/.claude.json` corruption issues under concurrent sessions — the same conditions as parallel agents on the host. Set `mode = "isolated"` to opt in to per-run home forks with automatic harvest and explicit promote/merge (`pall8t home inbox|show|promote|drop|merge`), plus revision history and lifecycle management (`pall8t home log|diff|rollback|ls|rm|gc`) — see [docs/specs/home-compositor.md](docs/specs/home-compositor.md). Still off by default in v1; only the pluggable local-Claude merge resolver and the non-APFS fork fallback remain on the roadmap.
- **No read-only mounts** (apple/container limitation) — reference repos are protected by `git clone --local` duplication instead.
- **Workspace isolation is the caller's responsibility.** Two agents in the same directory will step on each other; use worktrees.

Full requirements in [docs/requirements.md](docs/requirements.md); architecture decisions in [docs/adr/](docs/adr/); release process in [docs/release.md](docs/release.md).
