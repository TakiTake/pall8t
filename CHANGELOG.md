# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`[container] ssh`: forward the host's SSH agent into the sandbox**
  (`container run --ssh`), so an agent can push over SSH without a
  private key ever entering the container home. Off by default —
  while the run lasts, the sandbox can authenticate as you anywhere your
  keys are trusted. `pall8t run --ssh` / `--ssh=false` overrides it for
  one run, and pall8t warns when forwarding is on but the host has no
  `SSH_AUTH_SOCK` (the runtime would otherwise forward nothing silently).
- **A drift report for the relay's herdr method classification**
  (`scripts/herdr-method-drift.py`, plus a weekly report-only workflow):
  the `READ` allowlist that decides what a sandboxed agent may call under
  `[herdr] sandbox = "readonly"` is a hand-made snapshot of herdr's
  inventory, and this says what a newer herdr serves that it doesn't
  cover. Report only — classification stays a human decision, and herdr's
  schema is not a complete inventory (methods that hijack the connection,
  like `pane.graphics.stream`, are absent from it).
- **`[container] hardening = "strict"`**: drop every Linux capability,
  mount the container's root filesystem read-only, put `/tmp` on a tmpfs,
  and cap file descriptors — leaving the workspace and the container home
  as the only writable paths. Opt-in per project: whether it holds
  depends on the project's toolchain. The default level is unchanged.
- **Every run now gets an init process** (`container run --init`), which
  forwards signals to the agent and reaps the orphans it leaves behind
  (tmux, background shells, teammate agents). Exit-code propagation is
  unchanged — verified on 1.2.2 for both a plain exit and a signal.
- **Every `pall8t run` container carries provenance labels**
  (`pall8t.version`, `pall8t.project`, `pall8t.image`, the herdr pane /
  workspace / tab and sandbox mode when running under herdr, and a
  worktree's main git dir), and `pall8t ls --json` reports them alongside
  the image. `pall8t ls` now recognizes its containers by the
  `pall8t.version` label rather than by the `pall8t-` name prefix — the
  prefix stays as a fallback for containers started by an older pall8t,
  and a container someone else happened to name `pall8t-…` no longer
  counts as one of ours.
- `pall8t build --no-cache`: bypass the builder's layer cache and re-run
  every `RUN` step. `pall8t build` alone already rebuilds unconditionally,
  but a step whose instruction text didn't change — e.g. the claude CLI's
  `npm install -g` in the dev image — was still served from the layer
  cache, so "latest" fetches never actually refreshed.

### Fixed

- **`git status`, `git log`, and `git rev-parse` failed inside the sandbox
  with "detected dubious ownership"** — in the workspace itself, not only
  in read-only reference mounts. A mount's own directory inode arrives
  root-owned inside the container while its contents map to the host user
  correctly, so the earlier measurement (taken on a path *inside* a
  writable mount) missed it. Every mounted path is now marked
  `safe.directory`, so git works in the workspace, in a `[[mounts]]`
  entry, and in a linked worktree. Verified live on apple/container
  1.2.2, including a worktree created by `herdr worktree create` under
  `~/.herdr/worktrees/`.

### Changed

- **The sandbox's Linux `herdr` CLI is now the verified cache itself,
  mounted read-only**, instead of a per-run copy mounted read-write. The
  copy existed only because ADR-0007 believed read-only mounts were
  unavailable; a read-only mount is strictly stronger (the sandbox cannot
  corrupt even its own CLI) and drops a multi-megabyte copy from every
  bridged launch. `~/.pall8t/tools/herdr-run/` is no longer used —
  leftover directories there are inert and can be deleted by hand.
- **The herdr sandbox bridge is now a mounted Unix socket, not a TCP
  relay.** The host-side relay listens on its own socket under
  `~/.pall8t/run/` and pall8t mounts that socket into the container at
  `HERDR_SOCKET_PATH`; apple/container forwards a mount whose source is a
  Unix socket into the guest as a live socket (verified on 1.2.2 — the
  ADR-0007 premise that mounts can't do this was an artifact of pall8t
  only ever emitting `--mount`, whose parser takes directories alone).
  Consequences: **custom Containerfiles no longer need `socat`** (and the
  default image no longer installs it), the relay no longer opens a TCP
  port on the vmnet gateway, and `PALL8T_HERDR_PORT` is gone. Policy
  classification and the audit log are unchanged — `herdr.sock` itself is
  still never mounted into the sandbox.
- **A project Containerfile now builds with the project directory as its
  build context** (ADR-0010), instead of the Containerfile's own directory.
  `COPY` paths resolve relative to the project root, so a
  `.pall8t/Containerfile` can bake in repo-top files — e.g. the
  `flake.nix`/`flake.lock` a `container.watch` entry tracks — and rebuild
  when they change. The shared default image still builds from `~/.pall8t`.
  A Containerfile that `COPY`d paths relative to `.pall8t/` must adjust
  them; projects with a large tree can add a `<containerfile>.dockerignore`
  next to their Containerfile (the location apple/container reads ignore
  patterns from) to keep the context small.
- pall8t's own dev toolchain is now pinned by a nix flake at the repo top
  (`flake.nix` + `flake.lock`) instead of `mise.toml`: `nix develop` on the
  host and the dev-container image (`.pall8t/Containerfile`) both provision
  Rust from the same lock file.
- The dev image's userland tools (git, ripgrep, jq, less, vim, tmux, socat,
  gh, node) also come from the flake now (`#sandbox-tools`), pinned by
  `flake.lock` instead of floating apt/NodeSource/GitHub-CLI repository
  state; the NodeSource and GitHub-CLI apt repositories are gone from the
  image. apt keeps only what nix can't cover: the bootstrap pair
  (ca-certificates/curl), setuid sudo, openssh-client, and the distro C
  link chain (cc/pkg-config/mold). The claude CLI is still npm-installed
  at image build time, now on the flake's node with a `/usr/local/npm`
  prefix.

## [0.4.0] - 2026-08-09

### Added

- **`[[mounts]]` — mount any host directory into the sandbox**
  (ADR-0009), replacing `[[repos]]`. A mount is literal: the directory
  named by `source` appears inside the container at `target`, writable
  unless `readonly = true`. No cloning, no git requirement.

  ```toml
  [[mounts]]
  source = "~/notes"       # any directory, git repo or not
  target = "/notes"        # optional; defaults to the source's own path
  readonly = true          # optional; defaults to false
  ```

  - `readonly = true` mounts the real directory read-only and the runtime
    refuses every write — how you give an agent a checkout it may read
    and must not change. apple/container's read-only mounts turned out to
    have landed before 1.0.0 ever shipped, so pall8t had been cloning to
    work around a limitation no supported version had; enforcement is
    verified on 1.2.2.
  - `pall8t run --readonly[=BOOL]` overrides every entry for one run.
    Precedence is flag, then entry, then the default. Passing it with no
    `[[mounts]]` configured warns that it has no effect, rather than
    looking like it did something.
  - `target` defaults to `source`, so absolute paths keep meaning the
    same thing on both sides. Note `~` expands **host-side**: `~/x` lands
    at `/Users/you/x` inside the container, not `/home/dev/x`.
  - Bad input fails before anything is built, rather than inside the
    runtime with a mount line already printed: a `source` that is not a
    directory, a `source` that does not exist, and a `target` that is not
    an absolute container path. `~` is deliberately *not* expanded in a
    `target` — there it would mean the container's home, not the host's,
    so it is rejected rather than quietly turned into a host path.
  - A target may not cover the workspace, a worktree's `.git`, or the
    container home, and two targets may not cover each other. Hiding the
    home would take out the agent's own config and session history.
  - A linked git worktree mounted at its own path also gets its main
    repository's `.git` mounted, same mode, so git can resolve it —
    otherwise the sandbox sees a directory git cannot read as a
    repository at all.
  - Read-only mounts arrive owned by root rather than by the host user
    (apple/container applies its uid mapping only to writable mounts),
    which makes git refuse them with "detected dubious ownership".
    pall8t marks exactly those targets as git `safe.directory` via
    `GIT_CONFIG_*`, so `git log` and `git status` work with no setup.
    Nothing else's ownership check is relaxed.

### Changed

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

- **`[[repos]]` is gone** — replaced by `[[mounts]]` (ADR-0009).
  **Breaking:** a config still declaring it fails to load, with a message
  naming the replacement.

  This is deliberately not auto-translated. `[[repos]] source = X`
  mounted a `git clone --local` *copy* of X, so an agent's writes never
  reached your checkout; `[[mounts]] source = X` mounts X itself, where
  the same writes land on your files. Mapping one to the other would pick
  a protection level on your behalf — weaker than you had, or stricter
  than you chose. Reference material that used to rely on the copy for
  protection wants `readonly = true`:

  ```toml
  [[mounts]]
  source = "~/src/lib"
  readonly = true
  ```

  Clones under `~/.pall8t/repos` are no longer read or written by pall8t
  and can be deleted. Note anything an agent committed inside one lives
  there and nowhere else — check before removing it.

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
    tag base, so its next run rebuilds the image once. The old image is
    not cleaned up — pruning is scoped to the current tag base — so it
    stays until you delete it (`container image delete <old tag>`).

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

[Unreleased]: https://github.com/TakiTake/pall8t/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.4.0
[0.3.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.3.0
[0.2.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.2.0
[0.1.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.1.0
