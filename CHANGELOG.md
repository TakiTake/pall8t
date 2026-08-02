# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.3.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.3.0
[0.2.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.2.0
[0.1.0]: https://github.com/TakiTake/pall8t/releases/tag/v0.1.0
