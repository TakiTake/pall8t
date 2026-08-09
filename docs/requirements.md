# pall8t Requirements

Version: 1.0 (draft) — 2026-07-06

> Supersedes the TUI-based architecture of DESIGN v3 (`docs/design/DESIGN.md`, ADR-0003, ADR-0005). pall8t returns to its original purpose: running AI agents inside apple/container sandboxes, with no TUI.

## 1. Background and Purpose

Running AI agents (Claude Code, etc.) directly on the host gives them access to the entire host filesystem, so sandboxed execution is preferable. Existing Docker-based solutions require Docker Desktop on macOS and are heavyweight. apple/container is macOS-native and boots a lightweight VM per container, making it fast — but its CLI is incompatible with Docker's, so it doesn't fit the IDE + DevContainer ecosystem.

pall8t is a CLI tool that abstracts apple/container specifically for one purpose: running AI agents.

### 1.1 Lessons Learned (pivot from v0)

Adding a TUI, attach/detach, and other session-management features was turning pall8t into an inferior clone of herdr. Multiplexing, visibility, and session persistence are the domain of tmux / herdr; pall8t will not reimplement them.

### 1.2 Differentiators

1. Abstracts apple/container's non-Docker-compatible CLI into an interface that is necessary and sufficient for running AI agents
2. Detects Containerfile changes and rebuilds automatically at run time (no resident watch daemon)
3. Home-directory isolation (`~/.pall8t/home`) keeps the host's `~/.claude` and similar untouched

## 2. Scope

### 2.0 Runtime baseline

apple/container **1.2.0 or newer**. Below that, `Parser.allEnv` expanded a bare env name (no `=`) declared in an *image config* from the host process's environment and injected the result into the container ([apple/container#2027](https://github.com/apple/container/pull/2027)). That breaks the boundary FR-1 states — pall8t forwards nothing from the host environment by default — from underneath, since the expansion happens host-side before pall8t's argv is involved. An older runtime is not blocked: pall8t warns once per invocation and continues, because refusing to run buys the user nothing they can act on faster than the warning does.

### 2.1 In Scope

- Run AI agents on apple/container as foreground processes
- Mount `~/.pall8t/home` rw as the container home, isolating credentials and agent config from the host
- Mount the current directory rw as the workspace
- If cwd is a git worktree, automatically mount the main repository's `.git` as well
- At run time, compare the Containerfile hash and rebuild before launch if it changed
- Customization via TOML config (CPU/memory allocation, mounts, default command, etc.)
- Mount arbitrary host directories into the container via `[[mounts]]`, at their own path or a chosen one, writable or read-only (ADR-0009)
- TTY passthrough and signal forwarding (behave well under tmux / herdr)
- herdr bridge: inside a herdr pane, expose the host herdr session to the sandboxed agent (env passthrough, socket relay with policy + audit, version-matched Linux CLI) — ADR-0007

### 2.2 Out of Scope

- TUI / dashboard (→ herdr)
- attach/detach, session persistence (→ tmux / herdr)
- Resident daemons or watch processes
- Workspace isolation such as creating worktrees or clones (→ caller's responsibility; the user or herdr cuts the worktree)
- Collecting results (automated push / merge)
- Network restrictions (excluded from v1; see Roadmap)
- Docker / DevContainer compatibility

## 3. Functional Requirements

### FR-1: Agent execution (`pall8t run`)

- Mount cwd rw into the container as the workspace and run the configured command (default: `claude`)
- Mount `~/.pall8t/home` rw as the container user's home
- Pass the TTY through as-is (interactive agents must be as usable as on the host)
- Forward SIGINT / SIGTERM etc. correctly to the process inside the container
- Return the container process's exit code unchanged
- `-- <cmd>` overrides the command from the config file
- Forward nothing from the host environment by default: the container process gets only what pall8t sets explicitly (today, the herdr bridge's `HERDR_*` identity and relay port). Honoring this needs the runtime baseline in §2.0 — an image config's bare env names are expanded host-side, out of pall8t's reach, before 1.2.0

### FR-2: Automatic build

- On `run`, compare the Containerfile hash against the last build; if it changed, build before launching
- `container.watch` extends the same hash with a list of extra project files (literal paths, no globs; a project Containerfile is required) so editing a file the Containerfile `COPY`s in — e.g. a lockfile — also triggers a rebuild; a listed file that doesn't exist is a hard error, never silently hashed as empty
- A project Containerfile (`container.containerfile` or `.pall8t/Containerfile`) builds with the **project directory** as the build context, so `COPY` paths are relative to the project root and reach the files `container.watch` tracks (ADR-0010); the shared default image builds from `~/.pall8t`
- On build failure, do not launch the agent; exit non-zero
- `pall8t build` performs an explicit build
- Build output streams live to stderr by default (not captured/hidden), kept off pall8t's own stdout so `built <tag>` and `ls --json` stay machine-readable

### FR-3: git worktree support

- If cwd's `.git` is a pointer file (worktree), detect the main repository's common `.git` directory and mount it so the path structure inside the container matches
- git operations inside the worktree (status / commit / diff) must work as they do on the host

### FR-4: Mounts

- `[[mounts]]` makes a host directory visible inside the container. Any directory — a git checkout, a notes folder, a dataset; there is no requirement that it be a repository (ADR-0009)
- `source` is the host path (`~` expands host-side). `target` is where it appears inside the container and must be absolute; it defaults to `source`, an identity mount, so absolute paths mean the same thing on both sides
- `readonly` defaults to `false`. `true` mounts it read-only and the runtime refuses every write — the way to give an agent a checkout it may read and must not change. `pall8t run --readonly[=BOOL]` overrides every entry for one run
- A source that does not exist is a hard error, never a silently empty mount
- A target may not overlap the workspace, a worktree's main `.git`, or the container home, and two targets may not overlap each other — a covering mount would hide what the run is built on, or silently win over the mount before it
- A linked git worktree mounted at its own path also gets the main repository's `.git` mounted, with the same mode, so git can resolve it (FR-3's rule, applied to mounts). Not for a retargeted mount, where the worktree's absolute pointer paths no longer line up
- Read-only mounts appear root-owned inside the container, so pall8t marks exactly those targets as git `safe.directory` (via `GIT_CONFIG_*`); without it git refuses to read them at all (ADR-0009)
- `[[repos]]`, which cloned a git repository and mounted the copy, is removed. A config still declaring it fails to load with a message naming the replacement and stating the semantic difference — a mount is the real directory, so protection now comes from `readonly = true` rather than from the copy

### FR-5: Container management

- `pall8t ls`: list containers started by pall8t; `--json` for machine-readable output (intended for herdr etc.)
- `pall8t exec <id> -- <cmd>`: run a command inside a running container
- `pall8t stop <id>`: stop a container

### FR-6: Initialization (`pall8t init`)

- Generate `~/.pall8t/home` and a `~/.pall8t/config.toml` skeleton
- Generate a project `.pall8t/config.toml` skeleton (never overwrite existing files); the example Containerfile is materialized once at `~/.pall8t/Containerfile`, not inside the project — see FR-2
- Inform the user that the agent must be logged in once inside the container on first use

## 4. CLI Design

```
pall8t init              # generate ~/.pall8t/home, config skeletons, example Containerfile
pall8t run [-- cmd...]   # hash check → build if needed → run (TTY passthrough)
pall8t build             # explicit build
pall8t ls [--json]       # list running containers
pall8t exec <id> -- cmd  # run command inside container
pall8t stop <id>         # stop container
```

Design principle: from the caller's perspective (tmux / herdr / shell scripts), pall8t is a well-behaved foreground CLI. Minimal arguments, configuration lives in TOML, clean stdin/stdout, correct exit codes.

## 5. Configuration

Two layers: global `~/.pall8t/config.toml` and per-project `.pall8t/config.toml`. Project settings win.

```toml
[container]
cpus = 4
memory = "8g"
# containerfile = "path/to/other/Containerfile"   # default: .pall8t/Containerfile

[run]
command = ["claude"]     # --dangerously-skip-permissions is NOT in the default.
                         # Users who want it must set it explicitly.

[[mounts]]               # any host directory, mounted into the container
source = "~/src/other-lib"
# target = "/src/other-lib"  # optional; defaults to the source's own path
# readonly = true            # optional; defaults to false (writable)
```

## 6. Non-Functional Requirements

- **NFR-1 Startup overhead**: when no build is needed, added latency between `run` and agent launch must be minimal (leverage apple/container's boot speed)
- **NFR-2 Zero resident processes**: pall8t itself runs no daemon. (The herdr-bridge relay, ADR-0007, is session-scoped, not resident: spawned per `run`, it watches its parent and exits with the session)
- **NFR-3 Host non-pollution**: never touch the host's existing agent configuration such as `~/.claude`
- **NFR-4 tmux / herdr affinity**: achieved not through special integration features but through correct TTY passthrough, signal handling, and exit codes

## 7. Known Limitations

- **Shared home under parallel execution**: with parallel runs, all containers share `~/.pall8t/home` rw. Claude Code itself has known `~/.claude.json` corruption issues under concurrent sessions (non-atomic read-modify-write) — the same conditions apply when running in parallel on the host. Accepted as a known limitation in v1
- ~~**No read-only mounts**: apple/container limitation~~ — resolved. apple/container#990 closed before 1.0.0 shipped and enforcement is verified on 1.2.2; `[[mounts]] readonly = true` mounts a directory read-only (ADR-0009). A writable mount is the real directory, so what the clone used to protect is now protected by `readonly = true`
- **Workspace isolation is the caller's responsibility**: pall8t does not prevent conflicts when multiple agents run in the same directory (interleaved edits, `.git/index.lock` contention, working trees swapped by branch switches). Worktree-based workflows are recommended

## 8. Roadmap (post-v1)

1. **Per-run home clones and knowledge aggregation**: give each run a copy of `~/.pall8t/home` to isolate writes, while aggregating knowledge each agent adds at user level (skills etc.) back to the host. Merging knowledge produced in parallel is the essential challenge — a good answer here could become pall8t's core value. Still open: one implementation (the home compositor, `[home] mode = "isolated"`) shipped in 0.3.0 and was removed for lack of use — see [ADR-0008](adr/0008-drop-home-compositor.md); its spec and the evaluation of off-the-shelf tools are kept in `docs/specs/home-compositor*.md` for whoever tries again
2. ~~**Read-only mounts**: make reference repositories ro once apple/container supports it~~ — **done** (ADR-0009), as `[[mounts]] readonly`. Remaining: the herdr bridge's binary mount, which still uses a writable per-run copy from the era when ro mounts were thought unavailable; and a copy-style mount for agents that must commit against a repo they may not modify, if that use case resurfaces
3. **Network restrictions**: egress control to strengthen sandbox integrity
