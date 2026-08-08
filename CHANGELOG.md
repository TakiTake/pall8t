# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Reference repos are now mounted read-only** instead of being
  duplicated (ADR-0009). A repo listed under `[[repos]]` appears at its
  own absolute path inside the container as before, but the runtime
  refuses every write to it, so the agent reads the real checkout and
  cannot change it. apple/container's read-only mounts turned out to
  have landed before 1.0.0 ever shipped — pall8t had been carrying a
  workaround for a limitation no supported version had — and enforcement
  is verified on 1.2.2.
  - **Breaking for agents that write to a reference repo.** `git fetch`,
    `git commit`, and any other write inside one now fail with
    `Read-only file system` where they previously succeeded against a
    copy. Set `readonly = false` on the entry (or run `pall8t run
    --repos-readonly=false`) to get that copy back — it is unchanged,
    including the `origin` rewrite that makes `git fetch` reach the real
    upstream, and including the fact that it is kept under
    `~/.pall8t/repos` and reused by later runs rather than refreshed from
    the source.
  - `readonly` is per entry, defaults to `true`, and
    `--repos-readonly[=BOOL]` on `pall8t run` overrides every entry for
    one run. A misspelled key now fails the config parse rather than
    silently falling back to the default.
  - A read-only entry creates no clone at all: no disk, no first-run
    `git clone --local`, and nothing that can go stale against its
    source. `~/.pall8t/repos` is only created when some entry asks for a
    writable copy; existing clones there are left alone and are reused if
    you set `readonly = false`.
  - Each run prints which protection each repo got, since the two modes
    differ in what the agent may do.
  - A read-only entry that is a **linked git worktree** also gets its main
    repository's `.git` mounted read-only, the same way `pall8t run`
    already handles a worktree workspace (FR-3). Without it the sandbox
    would see a directory git cannot read as a repository at all, since
    such a worktree's `.git` is a pointer file naming a path outside the
    source.
  - Read-only mounts arrive inside the container owned by root rather
    than by the host user (apple/container applies its uid mapping only
    to writable mounts), which makes git refuse them with "detected
    dubious ownership". pall8t now marks exactly the paths it mounted
    read-only as git `safe.directory` via `GIT_CONFIG_*`, so `git log`
    and `git status` work in a reference repo with no setup. Nothing
    else's ownership check is relaxed.
- Mounts are passed to apple/container as `--mount
  type=virtiofs,source=…,target=…` rather than `-v host:dest`. Same
  semantics for every existing mount, but `--mount` directives are
  validated by the runtime: on 1.2.2 a typo'd `-v src:dst:readonlyy`
  mounts read-write in silence, while `--mount …,readonlyy` fails the run
  before it starts. A protection flag must not fail quietly.
- apple/container **1.2.0 or newer** is now the documented baseline, and
  `pall8t run`/`build`/`ls`/`exec`/`stop` warn once on stderr when the
  installed CLI is older. Older runtimes still work — the warning never
  blocks a run — but 1.2.0 is where apple/container#2027 stopped a bare
  `ENV NAME` (no value) in an *image config* from being expanded out of
  the host process's environment and injected into the container. That
  expansion happens host-side, before pall8t's argv exists, so on an
  older runtime a base image could pull a host token or path into the
  sandbox and pall8t's "forwards nothing from the host environment by
  default" could not stop it. An unrecognized version banner warns about
  nothing.

### Removed

- The experimental home compositor (`[home] mode = "isolated"`) and the
  whole `pall8t home` command family (harvest, inbox, show, promote,
  drop, merge, log, diff, rollback, ls, rm, gc), added in 0.3.0 and
  unused since — see [ADR-0008](docs/adr/0008-drop-home-compositor.md).
  `~/.pall8t/home` is now mounted as the container home unconditionally,
  exactly as `mode = "shared"` (the default) always did, so nothing
  changes for anyone who never opted in.
  - A `[home]` section left in a config file is parsed and ignored
    rather than failing the run; if it sets anything, a warning names
    the file to clean up. The bare `[home]` header that `pall8t init`
    used to write (all keys commented out) sets nothing and stays
    quiet.
  - **If you ran `mode = "isolated"`, harvest before upgrading**:
    `pall8t home merge` on 0.3.0 folds pending runs into the base. After
    upgrading there is no tool to do it. Nothing is deleted — unharvested
    runs stay in `~/.pall8t/instances/<run>/root/` and unpromoted
    changesets in `~/.pall8t/inbox/` — but pall8t no longer reads them,
    so copy out what you want before removing those directories.
    `~/.pall8t/revisions/*/snapshot/` holds full copies of the base home,
    credential files included, and is worth clearing once reviewed.
    `~/.pall8t/home` itself is untouched.
  - Exit code `2` no longer means "unresolved merge conflict" — the
    commands that produced it are gone. It is still what clap returns
    for a usage error, so a script that calls `pall8t home …` now gets
    `2` for "no such subcommand"; `0` and `1` are unchanged.

### Fixed

- `pall8t run` from a workspace with a long directory name now works on
  apple/container 1.2.0+. 1.2.0 started rejecting any container name over
  63 characters (`ManagedContainer.nameValid`, apple/container#1956),
  which `container run` checks before it launches anything — so a
  workspace whose name pushed `pall8t-<slug>-<hash>-<pid>` past the cap
  failed outright with "container ID ... is not a valid container ID"
  where 1.0.0 had accepted it. The slug is now capped at 32 characters;
  the path hash already carried the uniqueness, so nothing else changes.
  - Only paths whose basename slugs past 32 characters are affected, and
    for those the key is shortened, never re-derived: shorter names come
    out byte for byte identical. An affected workspace gets a new image
    tag base and a new `~/.pall8t/repos/<key>` clone directory, so its
    next run rebuilds the image once and re-clones the reference repo.
    Neither predecessor is cleaned up — image pruning is scoped to the
    current tag base, so the old image and the old clone directory stay
    until you delete them (`container image delete <old tag>`,
    `rm -rf ~/.pall8t/repos/<old key>`).

## [0.3.0] - 2026-08-01

### Added

- herdr bridge (ADR-0007): when `pall8t run` starts inside a herdr pane,
  the herdr CLI now works *inside* the sandbox — env passthrough
  (`HERDR_ENV`, workspace/tab/pane ids), a host-side relay to `herdr.sock`
  (apple/container mounts can't forward Unix sockets), an in-container
  socat bridge, and a version-matched Linux `herdr` binary auto-downloaded
  once to `~/.pall8t/tools/herdr/<version>/` (integrity-verified against a
  sha256 sidecar on every use) and copied per-run into
  `~/.pall8t/tools/herdr-run/<container>/`, which is mounted on `PATH` — so
  one sandbox can never tamper with the binary another executes. Governed
  by the new `[herdr] sandbox` config: `"full"` (default; host-admin
  methods such as `server stop`/`integration install` are always denied by
  namespace), `"readonly"`, or `"off"`. Every relayed request is
  peer-pinned to the sandbox container's IP and audit-logged under
  `~/.pall8t/logs/`.
- The default image now installs `socat` (required by the in-container
  bridge; its absence degrades with a warning, never a failed run).
- Project icon: `assets/pall8t-icon-cobalt.svg` (three shipping
  containers on a pallet, cobalt navy).

### Development

- Review-environment tooling: `.coderabbit.yaml` (CodeRabbit config, free
  for this public repo), a dormant Codex PR-review workflow
  (`codex-review.yml`, needs an `OPENAI_API_KEY` secret), weekly
  report-only mutation-testing (`mutants.yml`) and duplication/unused-deps
  (`hygiene.yml`) workflows, the `local-review`/`review-loop` skills, and
  `docs/testing.md`. None gate the build.

## [0.2.0] - 2026-07-25

### Added

- `[container] watch = [...]` config: extra project files (e.g. a lockfile
  such as `flake.nix`/`flake.lock`) whose path and contents fold into the
  same image-tag hash as the Containerfile, so editing one of them now
  triggers a rebuild too instead of silently reusing a stale image
  ([#35](https://github.com/TakiTake/pall8t/issues/35)). Requires a project
  Containerfile (`container.containerfile` or `.pall8t/Containerfile`);
  literal paths only (no globs), capped at 100 files / 4 MiB combined, and
  a missing or non-regular listed file is a hard error rather than being
  silently skipped.

### Changed

- README: documented Containerfile customization as its own subsection,
  corrected herdr integration guidance (plain `pall8t run` is recognized
  directly — no `herdr agent start` wrapper needed), and trimmed a stale
  pre-release Homebrew install caveat.

## [0.1.0] - 2026-07-12

Initial public release.

pall8t runs AI coding agents inside [apple/container](https://github.com/apple/container)
sandboxes: a headless CLI that launches an agent in a macOS-native lightweight
VM, with the current directory mounted as the workspace and an isolated
container home.

### Added

- `pall8t init` — generate `~/.pall8t/home`, `.pall8t/config.toml` skeleton, and
  the default Containerfile.
- `pall8t run` — hash-check the Containerfile, rebuild if needed, then run the
  agent (default: `claude`) in the sandbox with TTY passthrough and signal
  forwarding; `-- <cmd>` overrides the configured command.
- `pall8t build` — explicit, unconditional image build.
- `pall8t ls [--json]` — list pall8t containers (`--json` for scripting, e.g.
  herdr).
- `pall8t exec <id> -- cmd` — run a command inside a running container.
- `pall8t stop <id>` — stop a container.
- `pall8t herdr doctor [--json]` — check herdr env/socket/binary reachability.
- Automatic image rebuilds driven by the Containerfile's content hash, with no
  resident watch daemon; superseded images are pruned automatically after a
  successful build.
- Persistent, shared container home (`~/.pall8t/home`) so agent login and
  config survive across runs and rebuilds without touching the host's
  `~/.claude`.
- Git worktree awareness: when cwd is a worktree, the main repository's `.git`
  is mounted alongside so `status`/`commit`/`diff` behave exactly as on the
  host.
- Reference-repo protection via `git clone --local` duplication — a workaround
  for apple/container's missing read-only mounts
  ([apple/container#990](https://github.com/apple/container/issues/990)).
- Two-layer TOML config (global `~/.pall8t/config.toml`, per-project
  `.pall8t/config.toml`, project wins per field) covering container
  resources, run command, and reference repos.
- herdr integration: `argv[0]` agent-name hinting (with lookthrough for
  launchers and Homebrew's `container` exec wrapper) so herdr's screen-content
  state detection works on the sandboxed agent; sidebar display-name
  reporting (`<agent> (pall8t)`); automatic skip of the tmux wrapper when
  already running inside a herdr pane.
- Home compositor isolated mode (`[home] mode = "isolated"`, **experimental**)
  for per-run home forks with harvest/promote/merge
  (`pall8t home inbox|show|promote|drop|merge`) and revision history/lifecycle
  management (`pall8t home log|diff|rollback|ls|rm|gc`); off by default in
  favor of the shared-home mode.

[Unreleased]: https://github.com/TakiTake/pall8t/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.3.0
[0.2.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.2.0
[0.1.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.1.0
