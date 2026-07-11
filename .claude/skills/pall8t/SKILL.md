---
name: pall8t
description: How to work inside a pall8t sandbox — the apple/container environment this agent session runs in. Use when the session runs inside a container launched by pall8t, when developing pall8t itself, or when unsure about the sandbox layout, persistence, mounts, or git behavior.
---

# Working inside a pall8t sandbox

You are running inside a Linux VM (apple/container) launched by **pall8t**, a headless sandbox runner on macOS. Your session is a plain foreground process: when it exits, the container is removed. The human may run several sandboxes in parallel (via tmux/herdr), one per task.

## Environment facts

- Your cwd is the **workspace** — the host directory `pall8t run` was invoked in, mounted at the **identical absolute path** inside this container. Everything under it persists on the host and is directly readable by the human's IDE at the same path.
- Files you create are owned by the host user (your UID matches theirs). `sudo` works, but grants root only inside this VM.
- Your `$HOME` is `/home/dev`, backed by the host's `~/.pall8t/home` — **persistent across runs and rebuilds** (login state, shell history, dotfiles) and **shared by all pall8t sandboxes**.
- If the workspace is a git worktree, the main repository's `.git` is also mounted, so `git status`/`commit`/`diff` work exactly as on the host.
- **Reference repos** (from `.pall8t/config.toml [[repos]]`) appear at their usual host paths, but you are looking at a disposable `git clone --local` copy — writes never reach the original, and your changes there may be discarded.
- The `container` CLI does **not** exist here; you are inside the container. Do not try to run pall8t or docker/container commands.

## Developing pall8t itself

The pall8t repo ships `.pall8t/Containerfile` — the default probe pall8t resolves to for its own sandbox — so a sandbox launched in that repo already has Rust (`cargo`, `clippy`, `rustfmt` at `/usr/local/cargo/bin`). Build checks:

```sh
cargo check
cargo clippy -- -D warnings && cargo fmt --check
cargo test
```

(`mise` is not installed in the container; run cargo directly.) Requirements: `docs/requirements.md`; decisions in `docs/adr/`. Keep both updated when you change architecture-relevant behavior.

## Being a good sandbox citizen

- Session lifetime equals process lifetime: if your process is killed, the container is removed but the workspace (and your commits) persist.
- Persistence and multiplexing live **outside** the sandbox (tmux/herdr on the host) — don't build workarounds for them inside.
