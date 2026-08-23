<p align="center">
  <img src="assets/pall8t-icon-cobalt.svg" alt="pall8t icon — three shipping containers on a pallet" width="128" height="128">
</p>

# pall8t

*(pronounced "pallet" — the thing containers ship on)*

Run AI coding agents inside [apple/container](https://github.com/apple/container) sandboxes. pall8t is a headless CLI that does one job well: launch an agent in a macOS-native lightweight VM, with your current directory mounted as the workspace, an isolated container home, and automatic image rebuilds when the Containerfile changes. Multiplexing, session persistence, and workspace isolation belong to the tools that already own them (tmux, herdr, git worktrees) — pall8t is the well-behaved foreground process they spawn (see [ADR-0006](docs/adr/0006-drop-tui.md)).

## Why

- **Sandboxed by construction.** Agents run in a per-container VM, never on the host. Files land as *your* UID, never root.
- **No Docker Desktop.** apple/container is macOS-native and boots fast, but its CLI is docker-incompatible; pall8t abstracts it into exactly what running an agent needs. apple/container is the only supported runtime — there's no Docker, OrbStack, or podman backend.
- **Host non-pollution.** `~/.pall8t/home` is mounted as the container home — the agent's login and config live there, and your host `~/.claude` is never touched.
- **Automatic rebuilds, no daemon.** At `run` time the Containerfile's content hash picks the image tag; a change means a rebuild before launch. No watch process, no state file.
- **Worktree-aware.** If your cwd is a git worktree, the main repository's `.git` is mounted too, so git inside the container works exactly as on the host.
- **Mount anything the agent should see.** `[[mounts]]` puts any host directory inside the container — a reference checkout, a notes folder, a dataset. It lands at its own absolute path by default, so paths mean the same thing on both sides, or at a `target` you choose. `readonly = true` hands it over read-only and the runtime refuses every write, which is how you let an agent read a checkout it must not change ([ADR-0009](docs/adr/0009-explicit-mounts.md)).

## Requirements

- macOS on Apple silicon, [apple/container](https://github.com/apple/container) **1.2.0 or newer** installed
- git

pall8t runs on older apple/container builds and warns once per invocation instead of refusing, but 1.2.0 is where the sandbox boundary it documents actually holds. Before it, a bare `ENV NAME` (no value) in an image config was filled in from *your* host environment and injected into the container ([apple/container#2027](https://github.com/apple/container/pull/2027)) — a base image could pull a host token or path into the sandbox, and nothing pall8t does on its side prevents that.

## Install & quickstart

```sh
brew install TakiTake/tap/pall8t
```

(or build from source with `cargo install --path .` — this needs the Rust toolchain.)

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
pall8t build [--no-cache]  # explicit (unconditional) build; --no-cache re-runs every RUN step
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
# ssh = true                     # forward the host's SSH agent into the sandbox (default: false)
# containerfile = "path/to/other/Containerfile"   # relative to the project dir; default: .pall8t/Containerfile
# watch = ["flake.nix", "flake.lock"]   # extra files whose content also decides whether to rebuild

[run]
command = ["claude"]     # --dangerously-skip-permissions is NOT in the default.
                         # Users who want it must set it explicitly.

[[mounts]]                    # any host directory, git repo or not
source = "~/src/other-lib"
# target = "/src/other-lib"   # optional; defaults to the source's own path
# readonly = true             # optional; defaults to false (writable)
```

Override for a single run without editing the file: `pall8t run --readonly` (or `--readonly=false` to force them all writable). The flag wins over every entry's own setting.

### Hardening the sandbox

Everything pall8t runs is already in its own VM. `[container] hardening` decides how much the runtime confines the container *inside* that VM:

```toml
[container]
hardening = "strict"   # default: "default"
```

- **`"default"`** — what every pall8t release has run: a writable root filesystem and the runtime's normal capability set.
- **`"strict"`** — `--cap-drop ALL` (the capability *bounding* set is emptied, so nothing inside can regain one), a read-only root filesystem, `/tmp` on a tmpfs, and an 8192 file-descriptor ceiling. The only writable paths left are the ones pall8t mounted on purpose: your workspace and the container home.

Opt-in per project, because whether it holds depends on the project's toolchain — a build that writes outside the workspace, or a tool that needs a capability, works under `default` and fails under `strict`. Verified on apple/container 1.2.2: under `strict`, a write to `/var/tmp` fails with `EROFS` and `CapBnd` reads `0000000000000000`; under `default`, the same write succeeds and `CapBnd` carries the runtime's usual set.

Independently of the profile, every run gets `--init`: an init process inside the container that forwards signals to the agent and reaps the orphans it leaves behind (a `tmux` session, a background shell, teammate agents). The agent's own exit code still comes back unchanged — `exit 42` gives 42, a signal-killed command gives 143.

### SSH agent forwarding

`[container] ssh = true` forwards the host's SSH agent into the sandbox (`container run --ssh`): apple/container mounts the agent socket at `/var/host-services/ssh-auth.sock` inside the guest and points the container's `SSH_AUTH_SOCK` at it. **No key material crosses the boundary** — the sandbox sends signing requests to the agent on the host, which is the point: without it, git-over-SSH inside the sandbox needs a private key sitting in `~/.pall8t/home/.ssh`, where the agent can read it and where it stays after the run.

Off by default, because while the run lasts, code in the sandbox can authenticate as you anywhere your keys are trusted. `pall8t run --ssh` turns it on for one run; `--ssh=false` turns it off for one run.

If forwarding is on and the host has no `SSH_AUTH_SOCK`, pall8t warns: the runtime forwards nothing in that case but *still* sets `SSH_AUTH_SOCK` inside the container, so without the warning the only symptom is `ssh` failing to connect to a socket that was never there.

Note that `~` expands on the **host**, and an identity mount lands at that same absolute path inside the container — `~/src/other-lib` is `/Users/you/src/other-lib` in the sandbox, not `/home/dev/src/other-lib`. Set `target` if you want it somewhere friendlier.

### Customizing the Containerfile

Resolution priority: explicit `containerfile` config (relative to the project dir) → `.pall8t/Containerfile` if present → the built-in default image (node + claude CLI + gh).

- **User-level default.** The shipped default is materialized once at `~/.pall8t/Containerfile` by `pall8t init` and never overwritten — edit it to customize the default shared by all projects; delete it to restore the shipped one.
- **Project-level.** Create `.pall8t/Containerfile` to opt a project into its own image instead of the shared default; it always wins over the user-level default. Copying `~/.pall8t/Containerfile` as a starting point is the easy path.

A few caveats apply either way: there is no fallback to a root `./Containerfile` — that file usually belongs to the project's own app image, so pall8t never picks it up implicitly; point `containerfile` at it explicitly if you really want that. The build context is always the resolved Containerfile's own directory, so a `.pall8t/Containerfile` can only `COPY` files that live under `.pall8t/`. Custom toolchains must live outside `/home/dev` — the persistent home mount shadows it.

The image tag embeds the Containerfile's content hash, so any edit — no commit required — triggers a rebuild on the next `run`, and superseded images are pruned automatically after a successful build (images still used by a running container are kept). Only the Containerfile itself is hashed by default, not files it `COPY`s in — set `container.watch` to fold specific extra files into the same hash (e.g. a lockfile the Containerfile builds the toolchain from), so editing one of them also triggers a rebuild instead of silently reusing a stale image. Constraints: literal paths only (no globs), relative to the project dir, must already exist as regular files (a missing entry, or one that isn't a regular file, is a hard error, never silently skipped), capped at 100 files / 4 MiB combined, and only usable with a project Containerfile (`containerfile` or `.pall8t/Containerfile`) — the built-in default image builds from `~/.pall8t` and can't depend on per-project files. `pall8t build` remains the escape hatch for what no hash can see (e.g. an updated base image) — but note it still uses the builder's layer cache, so a `RUN` step whose instruction text didn't change (an `npm install -g` of a CLI's latest version, say) is served from cache, stale contents and all. `pall8t build --no-cache` bypasses the layer cache and re-runs every step.

A build streams `container build`'s own output live to stderr — no `-v` flag, this is always on, since a silent multi-minute build looks hung. Deliberately kept off pall8t's own stdout, which `pall8t build`'s final `built <tag>` line and `pall8t ls --json` need to stay machine-readable.

### What a running sandbox says about itself

Every container `pall8t run` starts is labelled, and `pall8t ls --json` hands the labels back:

```console
$ pall8t ls --json | jq '.[0]'
{
  "name": "pall8t-my-project-9f2c1a04-4711",
  "status": "running",
  "image": "pall8t-my-project:501-20-3b8f01c2d4e6",
  "labels": {
    "pall8t.version": "0.4.0",
    "pall8t.project": "/Users/me/src/my-project",
    "pall8t.image": "pall8t-my-project:501-20-3b8f01c2d4e6",
    "pall8t.herdr.pane": "%3",
    "pall8t.herdr.sandbox": "full"
  }
}
```

That is enough to map a herdr pane to the sandbox serving it, or a sandbox back to the project and image it booted, without parsing the container name. `name` and `status` are unchanged, so anything already reading them keeps working; the `pall8t.herdr.*` labels appear only for a run started from a herdr pane, and a container started by an older pall8t has no labels at all.

## Working with git worktrees

Cutting worktrees is the caller's business — you or herdr — but pall8t makes them work inside the sandbox:

```sh
git -C ~/src/my-project worktree add ../my-project-task -b task
cd ~/src/my-project-task
pall8t run
```

pall8t detects that cwd's `.git` is a worktree pointer and identity-mounts the main repository's `.git` alongside, so `status`/`commit`/`diff` inside the container behave exactly as on the host.

herdr can cut the worktree for you, which is the natural pairing — one pane per task, each with its own checkout and its own sandbox:

```sh
herdr worktree create --branch task    # checkout under ~/.herdr/worktrees/<repo>/<branch-slug>
pall8t run                             # in the pane herdr opens there
```

That layout puts the checkout far from the repository it belongs to (under herdr's own root rather than beside the main checkout), which pall8t handles the same way — the worktree's pointer file names the main `.git` by absolute path, and that path is mounted. Pinned by a test that builds the layout with real git.

Either way, every path pall8t mounts is marked `safe.directory` for git inside the container. It has to be: a mount's own directory arrives owned by root there (the files inside it map to you correctly), and git refuses a repository whose top-level directory it doesn't think you own.

## herdr integration

Type `pall8t run` into a herdr pane and herdr recognizes the sandboxed agent as-is — no wrapper function or special launch command needed (the agent-state bullet below explains how, and when a name can't be derived). herdr injects `HERDR_ENV`/`HERDR_PANE_ID`/`HERDR_SOCKET_PATH`/`HERDR_BIN_PATH` into `pall8t` itself (the host process), not into the sandboxed `claude`, so pall8t acts on them before it execs into the container:

- **Agent state (idle/working/blocked).** `pall8t run` execs the `container` client with `argv[0]` set to the sandboxed agent's name — the first name in the run command that herdr recognizes (`claude`, `codex`, `gemini`, …; launchers like `env`/`npx`/`uv run` and `pkg@version` specs are looked through), falling back to `HERDR_AGENT` when the command contains none. Only recognized names are ever derived: anything else (a wrapper script, a shell one-liner) yields no guess instead of a wrong one. `HERDR_AGENT` never overrides a name found in the command — the command is what actually runs, while an env var lingering in the environment is ambient and would mislabel e.g. `pall8t run -- codex` as claude under a stale `HERDR_AGENT=claude`; pall8t prints a note when it ignores one. Homebrew installs `container` as a bash exec wrapper whose inner `exec` would rewrite argv[0] to the Cellar path and destroy the hint (observed live), so pall8t looks through such a pass-through wrapper and execs its target binary directly, carrying over the wrapper's env assignments (`CONTAINER_INSTALL_ROOT`); it also sets `HERDR_AGENT` on the process so a future herdr macOS env hint could survive even argv0-rewriting setups. herdr assigns pane identity from the host process tree by argv0 basename (on macOS via `sysctl(KERN_PROCARGS2)`), and that identity is what unlocks its screen-content state detection — which then works on the sandboxed agent as well as a native one, because the agent's real UI streams through the pane's PTY unchanged. Without the hint, herdr only ever sees a process named `container`, never recognizes the pane, and tracks no state. (herdr's own `HERDR_AGENT` env-hint is Linux-only — it reads `/proc/<pid>/environ` — which is why pall8t honors the variable itself on macOS.) Side effect: `ps` shows the host-side client process under the agent's name; its executable is still `container`.
- **Sidebar identity.** pall8t reports the pane's display name to herdr (`herdr pane report-metadata … --display-agent "<agent> (pall8t)"`), which takes priority over the plain agent name herdr derived for the pane — verified live end-to-end (agent pane shows "claude (pall8t)"). When no agent name could be determined, no report is sent at all — the pane keeps whatever name herdr already had for it rather than being labeled with a guess. Deliberately sent *without* `--agent`: herdr only surfaces `display_agent` when it matches `effective_agent_label()` (host-process-derived, per above) — a match that only holds once the argv0 hint has taken effect, and this report must not depend on it. Best-effort either way: a missing `herdr` binary or unreachable socket just prints a warning and the run continues.
- **No redundant tmux wrapper.** If `[run] command` is the [Claude Code agent-teams tmux wrapper](#claude-code-agent-teams-split-panes) below and pall8t detects it's running inside a herdr pane, it runs plain `claude` instead — herdr already supplies persistence/multiplexing, so the wrapper (and its status bar) is redundant chrome. An explicit `pall8t run -- <cmd>` override always wins over this.
- **The herdr CLI works *inside* the sandbox** (ADR-0007). When `pall8t run` starts from a herdr pane — and `[herdr] sandbox` is `"full"` or `"readonly"`, and bridge setup succeeds (it's best-effort: a failure warns and the run continues without it) — the sandboxed agent gets `HERDR_ENV`/`HERDR_{WORKSPACE,TAB,PANE}_ID`, a working `HERDR_SOCKET_PATH`, and a version-matched Linux `herdr` binary on `PATH` — herdr's own agent skill runs unmodified, so a sandboxed agent can inspect neighboring panes, split, start sibling agents, and wait on them. Under the hood a host-side relay listens on its own Unix socket under `~/.pall8t/run/`, and pall8t mounts *that* socket into the sandbox at `HERDR_SOCKET_PATH` — apple/container forwards a mount whose source is a socket into the guest as a live socket (verified on 1.2.2; see the [ADR-0007 amendment](docs/adr/0007-herdr-bridge.md#amendment-2026-08-23-the-socket-premise-was-wrong), which corrects the earlier "mounts can't forward Unix sockets" premise). Every request still passes the relay's policy check and is audit-logged to `~/.pall8t/logs/` — `herdr.sock` itself is never handed to the sandbox. Policy via `[herdr] sandbox`: `"full"` (default — everything except host-admin methods like `server stop`/`integration install`, which are always denied), `"readonly"` (inspection only), or `"off"`. Note that panes and agents created through the bridge run **on the host**, outside the sandbox — `full` is a deliberate, audited opening for multi-agent coordination; set `readonly`/`off` to close it. First bridged run downloads the matching `herdr-linux-*` release into `~/.pall8t/tools/` (cached per version). Custom Containerfiles need nothing extra — the bridge is a mount, not an in-container process (earlier versions needed `socat` in the image for it).
- **`pall8t herdr doctor`** checks whether pall8t can see and reach the herdr pane it's running under (env vars present, socket reachable, `herdr` binary resolvable), plus the sandbox-bridge prerequisites (configured mode, cached Linux herdr CLI). Read-only, diagnostic only; `--json` for scripting.

Native session resume/restore (`pall8t resume`, live session-id reporting via `herdr pane report-agent-session`) isn't implemented yet: it needs a change to pall8t's foreground/exec-replace process model, and, upstream, a way for herdr to let a custom source supply its own resume command instead of its current hardcoded `claude --resume <id>` table. Tracked in [issue #18](https://github.com/TakiTake/pall8t/issues/18).

## Claude Code agent teams (split panes)

Claude Code can show teammate agents as tmux split panes (`teammateMode: "auto"` / `"tmux"`), but only if it's already running inside a tmux session — the default image ships tmux for exactly this. In config:

```toml
[run]
command = ["tmux", "new", "-A", "-s", "claude", "claude"]
```

then, inside the container (one-time, persists in the container home), add `"teammateMode": "auto"` to `~/.claude/settings.json`. Note that tmux here only multiplexes *within* one run: every `pall8t run` starts a fresh container (`--rm`), so there is no session to re-attach across runs — for persistence, run pall8t itself under tmux on the host. The image ships `/etc/tmux.conf` with `status off`; override in `~/.tmux.conf` inside the container if you want the status bar back.

## Known limitations (v1)

- **Shared home under parallel runs.** All containers share `~/.pall8t/home` rw; Claude Code has known `~/.claude.json` corruption issues under concurrent sessions — the same conditions as parallel agents on the host. The experimental per-run home fork that 0.3.0 offered as a way out (`[home] mode = "isolated"`) was removed for lack of use — see [ADR-0008](docs/adr/0008-drop-home-compositor.md). A leftover `[home]` section is now ignored with a warning; if you actually ran isolated mode, fold pending runs in with `pall8t home merge` **before** upgrading, since nothing after the upgrade can read them (see the [CHANGELOG](CHANGELOG.md)).
- **A writable mount is the real directory.** `[[mounts]]` no longer clones anything, so an agent can modify what you mount unless you set `readonly = true`. Mount reference material read-only; mount only what you're willing to have changed.
- **Workspace isolation is the caller's responsibility.** Two agents in the same directory will step on each other; use worktrees.

Full requirements in [docs/requirements.md](docs/requirements.md); architecture decisions in [docs/adr/](docs/adr/); release process in [docs/release.md](docs/release.md).
