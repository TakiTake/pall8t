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

Two layers, merged per field with the project winning: global `~/.pall8t/config.toml`, per-project `.pall8t/config.toml` — the project-scope mirror of `~/.pall8t`. `[[mounts]]` is one field: a project that declares any mounts replaces the global list rather than adding to it.

```toml
[container]
cpus = 4
memory = "8g"
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

Note that `~` expands on the **host**, and an identity mount lands at that same absolute path inside the container — `~/src/other-lib` is `/Users/you/src/other-lib` in the sandbox, not `/home/dev/src/other-lib`. Set `target` if you want it somewhere friendlier.

### Customizing the Containerfile

Resolution priority: explicit `containerfile` config (relative to the project dir) → `.pall8t/Containerfile` if present → the built-in default image (node + claude CLI + gh).

- **User-level default.** The shipped default is materialized once at `~/.pall8t/Containerfile` by `pall8t init` and never overwritten — edit it to customize the default shared by all projects; delete it to restore the shipped one.
- **Project-level.** Create `.pall8t/Containerfile` to opt a project into its own image instead of the shared default; it always wins over the user-level default. Copying `~/.pall8t/Containerfile` as a starting point is the easy path.

A few caveats apply either way: there is no fallback to a root `./Containerfile` — that file usually belongs to the project's own app image, so pall8t never picks it up implicitly; point `containerfile` at it explicitly if you really want that. A project Containerfile (`containerfile`, or `.pall8t/Containerfile`) builds with the **project directory** as its build context, so `COPY` paths are relative to the project root — the same tree `container.watch` resolves against ([ADR-0010](docs/adr/0010-project-root-build-context.md)). The whole tree is shipped to the builder, so a large one wants an ignore file: apple/container reads it from `<containerfile>.dockerignore`, next to and named after the Containerfile (never a `.dockerignore` at the context root). The shared default image is the exception — it builds from `~/.pall8t`, its own directory, so project files can never affect it. Custom toolchains must live outside `/home/dev` — the persistent home mount shadows it.

The image tag embeds the Containerfile's content hash, so any edit — no commit required — triggers a rebuild on the next `run`, and superseded images are pruned automatically after a successful build (images any existing container still uses, running or stopped, are kept). Only the Containerfile itself is hashed by default, not files it `COPY`s in — set `container.watch` to fold specific extra files into the same hash (e.g. a lockfile the Containerfile builds the toolchain from), so editing one of them also triggers a rebuild instead of silently reusing a stale image. Constraints: literal paths only (no globs), relative to the project dir, must already exist as regular files (a missing entry, or one that isn't a regular file, is a hard error, never silently skipped), capped at 100 files / 4 MiB combined, and only usable with a project Containerfile (`containerfile` or `.pall8t/Containerfile`) — the built-in default image builds from `~/.pall8t` and can't depend on per-project files. `pall8t build` remains the escape hatch for what no hash can see (e.g. an updated base image) — but note it still uses the builder's layer cache, so a `RUN` step whose instruction text didn't change (an `npm install -g` of a CLI's latest version, say) is served from cache, stale contents and all. `pall8t build --no-cache` bypasses the layer cache and re-runs every step.

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

Type `pall8t run` into a herdr pane and herdr recognizes the sandboxed agent as-is — no wrapper function or special launch command needed. herdr injects `HERDR_ENV`/`HERDR_PANE_ID`/`HERDR_SOCKET_PATH`/`HERDR_BIN_PATH` into `pall8t` itself (the host process), not into the sandboxed `claude`, so pall8t acts on them before it execs into the container.

All of it works with no config at all except the tab/agent naming. The whole `[herdr]` surface is three fields:

```toml
[herdr]
sandbox = "full"       # what the *sandboxed* agent may do to your herdr session:
                       #   "full" (default) — the whole herdr CLI except the host-admin
                       #                      namespaces `server.`, `session.`,
                       #                      `integration.`, `plugin.` (always denied)
                       #   "readonly"       — inspection only (list/get/read/wait)
                       #   "off"            — no herdr CLI inside the sandbox at all
auto_rename = true     # opt-in: name this run's tab *and* agent `<base>-<n>`, so what
                       #   you read off the tab is what other agents can address.
                       #   Undefined (the default) = pall8t renames nothing.
# agent_name = "api"   # base name instead of the workspace dir's basename → `api-1`.
                       #   Inert on its own — without auto_rename it warns on stderr.
```

`pall8t herdr doctor [--json]` checks the wiring: env vars present, socket reachable, `herdr` binary resolvable, plus the sandbox-bridge prerequisites (configured mode, cached Linux herdr CLI). Read-only, diagnostic only.

### What the integration does

- **Agent state (idle/working/blocked).** `pall8t run` execs the `container` client with `argv[0]` set to the sandboxed agent's name. herdr assigns pane identity from the host process tree by argv0 basename (on macOS via `sysctl(KERN_PROCARGS2)`), and that identity is what unlocks its screen-content state detection — which works on a sandboxed agent as well as a native one, because the agent's real UI streams through the pane's PTY unchanged. Without the hint herdr only ever sees a process named `container` and tracks no state. The name is the first herdr-recognized agent in the run command (`claude`, `codex`, `gemini`, …; launchers like `env`/`npx`/`uv run` and `pkg@version` specs are looked through), falling back to `HERDR_AGENT` when the command contains none — herdr's own env hint is Linux-only (`/proc/<pid>/environ`), which is why pall8t honors the variable itself on macOS. Anything unrecognized (a wrapper script, a shell one-liner) yields no guess instead of a wrong one, and `HERDR_AGENT` never overrides a name found in the command — a stale one would mislabel `pall8t run -- codex` as claude — so pall8t prints a note when it ignores one. Homebrew installs `container` as a bash exec wrapper whose inner `exec` would rewrite argv[0] and destroy the hint, so pall8t looks through such a pass-through wrapper and execs its target directly, carrying over its env assignments. Side effect: `ps` shows the host-side client process under the agent's name; its executable is still `container`.
- **Sidebar identity.** pall8t reports the pane's display name to herdr (`herdr pane report-metadata … --display-agent "<agent> (pall8t)"`), which takes priority over the plain agent name herdr derived for the pane. Sent deliberately *without* `--agent`, since herdr only surfaces `display_agent` when it matches its own host-process-derived label — a match that holds only once the argv0 hint has taken effect, and this report must not depend on it. When no agent name could be determined nothing is sent at all: the pane keeps whatever name herdr already had rather than being labeled with a guess. Best-effort — a missing `herdr` binary or unreachable socket warns and the run continues.
- **Naming the tab and the agent** — `auto_rename` (issue #71; numbering scheme [ADR-0011](docs/adr/0011-tab-numbering-state.md)). A herdr agent's *name* is what makes cross-sandbox agent-to-agent messaging usable — `herdr agent prompt api-2 "run the tests" --wait`. Without one the only working target is the pane id (`w13:p3`), which changes every run and reads like nothing: neither tab labels nor tab ids resolve as agent targets (verified on herdr 0.8.2). pall8t gives the tab and the agent the same string, `<base>-<n>`: `<base>` is the workspace directory's basename, slugged (or `agent_name`), and `<n>` is **pall8t's own counter**, kept per base name in `~/.pall8t/state/herdr-naming.json` — handed out once and never reused while one herdr server run lasts, so a tab keeps its name for its whole life and that name stays usable as an address other agents type. [ADR-0011](docs/adr/0011-tab-numbering-state.md) has the rest: how the count survives a herdr restart, and why it needs no herdr call at all (a failing `tab.list` costs the tab its rename, never the agent its number). Delete the state file to start numbering over — the next run seeds itself from the labels on screen. A name already claimed gets a further counter (`foo-1-2`), counting both a name a live agent answers to and a label another tab already wears, since herdr enforces no uniqueness on labels. A tab *you* renamed is never clobbered; one still on herdr's own label, or on a label pall8t wrote on an earlier run, is taken over — and when a label is left alone the run says which name actually reaches the agent. The two halves land at different times: the tab is renamed before the sandbox even starts, the agent only once herdr recognizes it (after the exec into `container run`), so a small detached `pall8t` child waits for that and renames it then, logging to `~/.pall8t/logs/herdr-naming.log`. Best-effort and independent — a failing agent rename never stops the tab from being named — and naming happens in every `sandbox` mode, `off` included.
- **The herdr CLI works *inside* the sandbox** — `sandbox = "full"` or `"readonly"` ([ADR-0007](docs/adr/0007-herdr-bridge.md)). The sandboxed agent gets `HERDR_ENV`/`HERDR_{WORKSPACE,TAB,PANE}_ID`, a working `HERDR_SOCKET_PATH`, and a version-matched Linux `herdr` binary on `PATH`, so herdr's own agent skill runs unmodified: a sandboxed agent can inspect neighboring panes, split, start sibling agents, and wait on them. Under the hood a host-side relay listens on its own Unix socket under `~/.pall8t/run/` and pall8t mounts *that* socket into the sandbox at `HERDR_SOCKET_PATH` (apple/container forwards a socket mount into the guest as a live socket, verified on 1.2.2 — see the [ADR-0007 amendment](docs/adr/0007-herdr-bridge.md#amendment-2026-08-23-the-socket-premise-was-wrong)). Every request is audit-logged to `~/.pall8t/logs/herdr-relay-<container>.log` and `herdr.sock` itself is never handed to the sandbox; the policy check is where the host-admin namespaces (`server.`, `session.`, `integration.`, `plugin.`) are denied in every mode, and an unrecognized method counts as a mutation, so a newer herdr's additions are transparent in `full` and refused in `readonly`. Note that panes and agents created through the bridge run **on the host**, outside the sandbox — `full` is a deliberate, audited opening for multi-agent coordination; set `readonly`/`off` to close it. Setup is best-effort (a failure warns and the run continues without the bridge); the first bridged run downloads the matching `herdr-linux-*` release into `~/.pall8t/tools/` (cached per version). Custom Containerfiles need nothing extra — the bridge is a mount, not an in-container process.
- **Delegating to a sibling agent** — [`skills/pall8t-herdr/SKILL.md`](skills/pall8t-herdr/SKILL.md). herdr's own skill covers the CLI; this one covers what's specific to asking *another* agent from *inside* a sandbox, starting with the settled-state trap that makes a healthy bridge look stalled: `agent prompt --wait --until idle` can never match a pane the human isn't looking at (it settles into `done`), so it always runs to its timeout. Use the plain `--wait`.

Native session resume/restore (`pall8t resume`, live session-id reporting via `herdr pane report-agent-session`) isn't implemented yet: it needs a change to pall8t's foreground/exec-replace process model, and, upstream, a way for herdr to let a custom source supply its own resume command instead of its current hardcoded `claude --resume <id>` table. Tracked in [issue #18](https://github.com/TakiTake/pall8t/issues/18).

## Known limitations (v1)

- **Shared home under parallel runs.** All containers share `~/.pall8t/home` rw; Claude Code has known `~/.claude.json` corruption issues under concurrent sessions — the same conditions as parallel agents on the host. The experimental per-run home fork that 0.3.0 offered as a way out (`[home] mode = "isolated"`) was removed for lack of use — see [ADR-0008](docs/adr/0008-drop-home-compositor.md). A leftover `[home]` section is now ignored with a warning; if you actually ran isolated mode, fold pending runs in with `pall8t home merge` **before** upgrading, since nothing after the upgrade can read them (see the [CHANGELOG](CHANGELOG.md)).
- **A writable mount is the real directory.** `[[mounts]]` no longer clones anything, so an agent can modify what you mount unless you set `readonly = true`. Mount reference material read-only; mount only what you're willing to have changed.
- **Workspace isolation is the caller's responsibility.** Two agents in the same directory will step on each other; use worktrees.

Full requirements in [docs/requirements.md](docs/requirements.md); architecture decisions in [docs/adr/](docs/adr/); release process in [docs/release.md](docs/release.md).
