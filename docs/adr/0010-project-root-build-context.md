# ADR-0010: Project-root build context for project Containerfiles

- Status: Accepted
- Date: 2026-08-09
- Extends: `container.watch` (issue #35, FR-2) — the rebuild trigger for files a Containerfile `COPY`s in
- Does **not** affect: the shared default image, which keeps building from `~/.pall8t` (its Containerfile's own directory)

## Context

`container build` was invoked with the Containerfile's parent directory as
the build context. For a project Containerfile that means `.pall8t/` — so a
`COPY` could only reach files placed inside `.pall8t/`, while the files
worth copying live at the project root: lockfiles that pin a toolchain
(`flake.nix`/`flake.lock`, `package-lock.json`, …). Issue #35 already built
the rebuild trigger for exactly those files (`container.watch`, whose
config example is the flake pair), but the context made them unreachable —
the watch could hash a file the Containerfile had no way to see.

The concrete trigger: pall8t's own dev image moved from a rustup-installed
Rust to a toolchain pinned by a repo-top nix flake, replacing `mise.toml`
as the version manager so the host shell (`nix develop`) and the sandbox
image install from the same `flake.lock`. That requires
`COPY flake.nix flake.lock` in `.pall8t/Containerfile`.

## Decision

**A project Containerfile — explicit `container.containerfile` or the
`.pall8t/Containerfile` probe — builds with the project directory as its
build context.** `COPY` paths are relative to the project root, the same
tree `container.watch` paths are resolved against, so "watch it" and "copy
it" name a file the same way.

The shared default image is unchanged: it builds from `~/.pall8t`, where
project files must not be able to affect it (decision 5 of issue #35 —
its tag is shared across projects).

## Consequences

- A Containerfile that `COPY`d paths relative to `.pall8t/` must rewrite
  them relative to the project root (pre-1.0 breaking change, noted in the
  changelog).
- The whole project tree is shipped to the builder. A large tree (a Rust
  `target/`, `node_modules/`) makes builds slow; the fix is an ignore file.
  apple/container reads it from **`<containerfile>.dockerignore`, next to
  and named after the Containerfile** (`BuildCommand.swift` resolves
  `dockerfile + ".dockerignore"`; it never looks for a `.dockerignore` at
  the context root, where docker also would) — pall8t's own is
  `.pall8t/Containerfile.dockerignore`, allowlist-style (ignore `*`,
  re-include the flake files), with patterns relative to the context root.
- `resolve` carries the context on `ResolvedImage` (`ctx_dir`), so the
  decision is made once at resolve time and pinned by tests, not recomputed
  at build time from the Containerfile path.
