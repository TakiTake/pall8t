# ADR-0001: Implementation language — Rust over Swift

- Status: Accepted
- Date: 2026-07-04

## Context

pall8t (formerly named "cabin") is a TUI that targets apple/container exclusively: it manages per-project keep-alive containers, builds UID/GID-matched base images, and spawns terminal tabs exec'd into containers. Since apple/container itself is written in Swift and exposes a Swift client library (`ContainerClient`, XPC to `container-apiserver`), we evaluated whether pall8t should be written in Swift to integrate at the API level, or stay in Rust wrapping the `container` CLI.

## Decision

Implement pall8t in Rust, integrating with apple/container by shelling out to the `container` CLI.

## Rationale

1. **CLI wrapping is the most stable integration surface.** As of 2026-07 apple/container is at 0.12.x (pre-1.0). The XPC API is unstable: recent releases removed compatibility with the v0 XPC API, and a versioned API is still planned. Linking `ContainerClient` directly would create a version-skew problem — pall8t's build-time client version must match the user's installed `container-apiserver`. The `container` CLI always matches its own daemon, so wrapping it avoids skew entirely.
2. **Machine-readable output exists.** `container ls --format json`, `container image list --format json`, and `container inspect` provide structured output, so CLI wrapping does not mean fragile text parsing.
3. **TUI ecosystem.** ratatui/crossterm/clap have no comparable Swift equivalents (SwiftTUI is immature). pall8t is primarily a TUI, so this dominates.
4. **Existing prototype.** The working prototype is already Rust; a rewrite has no offsetting benefit.
5. **Performance is a non-issue.** The ~100ms process-spawn overhead per CLI call is imperceptible for pall8t's operation frequency (status polling, exec spawn).

## Consequences

- pall8t depends on the `container` CLI being on `PATH`; no compile-time type safety against its output schemas.
- No access to features not exposed by the CLI (e.g. fine-grained event streams).

## Revisit triggers

Reconsider when apple/container reaches 1.0 with a versioned XPC API **and** pall8t needs capabilities the CLI cannot provide (e.g. event subscription). Escape hatch: add a small Swift helper binary (XPC in, JSON on stdout) invoked from Rust — a hybrid, not a rewrite.

## References

- [apple/container](https://github.com/apple/container) / [releases](https://github.com/apple/container/releases)
- [Technical overview](https://github.com/apple/container/blob/main/docs/technical-overview.md)
- [Command reference](https://github.com/apple/container/blob/main/docs/command-reference.md)
- [apple/containerization](https://github.com/apple/containerization)
