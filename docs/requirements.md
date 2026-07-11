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

### 2.1 In Scope

- Run AI agents on apple/container as foreground processes
- Mount `~/.pall8t/home` rw as the container home, isolating credentials and agent config from the host
- Mount the current directory rw as the workspace
- If cwd is a git worktree, automatically mount the main repository's `.git` as well
- At run time, compare the Containerfile hash and rebuild before launch if it changed
- Customization via TOML config (CPU/memory allocation, reference repositories, default command, etc.)
- Duplicate reference repositories with `git clone --local` (hardlinked objects) before mounting, protecting the originals
- TTY passthrough and signal forwarding (behave well under tmux / herdr)

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

### FR-2: Automatic build

- On `run`, compare the Containerfile hash against the last build; if it changed, build before launching
- On build failure, do not launch the agent; exit non-zero
- `pall8t build` performs an explicit build
- Build output streams live to stderr by default (not captured/hidden), kept off pall8t's own stdout so `built <tag>` and `ls --json` stay machine-readable

### FR-3: git worktree support

- If cwd's `.git` is a pointer file (worktree), detect the main repository's common `.git` directory and mount it so the path structure inside the container matches
- git operations inside the worktree (status / commit / diff) must work as they do on the host

### FR-4: Reference repositories

- Duplicate each repository listed under `[[repos]]` in the config via `git clone --local` and mount the copy
- `cp -al` is rejected: hardlinks don't apply to directories, and hardlinked working-tree files risk corrupting the original via in-place writes
- Positioned as the workaround for apple/container's lack of read-only mounts

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

[[repos]]                # reference repos (duplicated via git clone --local, then mounted)
source = "~/src/other-lib"
```

## 6. Non-Functional Requirements

- **NFR-1 Startup overhead**: when no build is needed, added latency between `run` and agent launch must be minimal (leverage apple/container's boot speed)
- **NFR-2 Zero resident processes**: pall8t itself runs no daemon
- **NFR-3 Host non-pollution**: never touch the host's existing agent configuration such as `~/.claude`
- **NFR-4 tmux / herdr affinity**: achieved not through special integration features but through correct TTY passthrough, signal handling, and exit codes

## 7. Known Limitations

- **Shared home under parallel execution**: with parallel runs, all containers share `~/.pall8t/home` rw. Claude Code itself has known `~/.claude.json` corruption issues under concurrent sessions (non-atomic read-modify-write) — the same conditions apply when running in parallel on the host. Accepted as a known limitation in v1
- **No read-only mounts**: apple/container limitation. Reference repositories are protected via `git clone --local` duplication instead
- **Workspace isolation is the caller's responsibility**: pall8t does not prevent conflicts when multiple agents run in the same directory (interleaved edits, `.git/index.lock` contention, working trees swapped by branch switches). Worktree-based workflows are recommended

## 8. Roadmap (post-v1)

1. **Per-run home clones and knowledge aggregation**: give each run a copy of `~/.pall8t/home` to isolate writes, while aggregating knowledge each agent adds at user level (skills etc.) back to the host. Merging knowledge produced in parallel is the essential challenge — a good answer here could become pall8t's core value
2. **Read-only mounts**: make reference repositories ro once apple/container supports it
3. **Network restrictions**: egress control to strengthen sandbox integrity
