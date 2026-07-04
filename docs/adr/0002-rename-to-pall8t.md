# ADR-0002: Rename cabin → pall8t

- Status: Accepted
- Date: 2026-07-04

## Context

The original name "cabin" was too generic: existing collisions on crates.io and poor searchability. Requirement: keep a name close in pronunciation but unique in spelling (cf. "symfony").

## Decision

Rename to **pall8t**, pronounced "pallet" — the thing containers ship on. The leetspeak `8` makes it collision-free and grep-proof.

## Consequences

Renamed identifiers:

| Old | New |
| :---- | :---- |
| crate/binary `cabin` | `pall8t` |
| container name `cabin-<slug>-<hash>` | `pall8t-<slug>-<hash>` |
| image `cabin-base:<uid>-<gid>` | `pall8t-base:<uid>-<gid>` |
| persistent home `~/.cabin/home` | `~/.pall8t/home` |
| config `~/.config/cabin/config.toml` | `~/.config/pall8t/config.toml` |
| per-project override `.cabin/Containerfile` | `.pall8t/Containerfile` |
