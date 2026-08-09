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
- If `HERDR_ENV=1` is set in this environment, the **herdr bridge** is configured — `HERDR_SOCKET_PATH` is meant to reach the host herdr session through a pall8t relay, with `herdr` (a version-matched Linux build) on `PATH`. But `HERDR_ENV=1` alone is not proof the bridge is *usable*: the in-container socket is created by a `socat` bridge at startup, so if the image lacks `socat`, or the `herdr` binary didn't provision, the socket/CLI won't actually be there. Confirm before relying on it — run a cheap read like `herdr pane current --current` (or check `HERDR_SOCKET_PATH` exists) and, if it fails, report that the bridge isn't up rather than improvising a client. No `HERDR_ENV` at all means no bridge (herdr absent, or `[herdr] sandbox = "off"`, or setup failed). Follow herdr's own skill for usage. Two pall8t-specific facts: panes, agents, and commands you create via herdr run **on the host, outside this sandbox** — treat that as deliberately crossing the sandbox wall, not a loophole to exploit; and requests may come back `sandbox_denied` — that's pall8t policy (`[herdr] sandbox` config, audit-logged on the host), so report it rather than trying to work around it.

## Developing pall8t itself

The pall8t repo ships `.pall8t/Containerfile` — the default probe pall8t resolves to for its own sandbox — so a sandbox launched in that repo already has Rust (`cargo`, `clippy`, `rustfmt` at `/usr/local/rust/bin`, provisioned from the repo-top nix flake at image build time). Build checks:

```sh
cargo check
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings
```

(The last line is the cross-target lint gate from CLAUDE.md — the image's flake toolchain ships that target's std, and `scripts/lint.sh` runs fmt plus both clippy legs in one go. Run cargo directly; the toolchain version comes from `flake.lock`, and editing `flake.nix`/`flake.lock` triggers an image rebuild on the next `pall8t run`.) Requirements: `docs/requirements.md`; decisions in `docs/adr/`. Keep both updated when you change architecture-relevant behavior.

## Being a good sandbox citizen

- Session lifetime equals process lifetime: if your process is killed, the container is removed but the workspace (and your commits) persist.
- Persistence and multiplexing live **outside** the sandbox (tmux/herdr on the host) — don't build workarounds for them inside. When the herdr bridge is present (above), that outside surface is reachable the sanctioned way: through the `herdr` CLI.
