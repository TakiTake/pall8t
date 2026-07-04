---
name: pall8t
description: How to work inside a pall8t workspace — the sandboxed apple/container environment this agent session runs in. Use when working in a directory containing repos/ and wt/, when asked to create git worktrees for a task, when developing pall8t itself, or when unsure about the sandbox layout, persistence, or git workflow.
---

# Working inside a pall8t workspace

You are running inside a Linux VM (apple/container) launched by **pall8t**, an agent multiplexer on macOS. Your session is one tab; the human may be running several agent tabs in parallel, one per task.

## Environment facts

- Your cwd is the **project workspace** — a host directory mounted at the **identical absolute path** inside this container. Everything under it **persists** across container restarts and is directly readable by the human's IDE on the host at the same path.
- Files you create are owned by the host user (your UID matches theirs). `sudo` works, but grants root only inside this VM.
- You have no access to the host beyond this workspace and your `$HOME` (`/home/dev`, also persistent — your login state survives rebuilds).
- The `container` CLI does **not** exist here; you are inside the container. Do not try to run pall8t or docker/container commands.

## Workspace layout

```
<workspace>/
  repos/<repo>/   seeded clone of each source repo — treat as the worktree parent
  wt/             one git worktree per task — DO YOUR WORK HERE
```

## Git workflow (one worktree per task)

1. Update the clone: `git -C repos/<repo> fetch origin`
2. Cut a worktree for your task:
   `git -C repos/<repo> worktree add ../../wt/<task>-<repo> -b <task-branch> origin/main`
3. Work, commit, and push from `wt/<task>-<repo>` (`origin` points at the real upstream; credentials live in your persistent home).
4. Keep the checkout in `repos/<repo>` clean — never commit or switch branches there directly.

Tasks may span multiple repos: cut one worktree per repo, same branch name.

## Developing pall8t itself

This repo ships `.pall8t/Containerfile`, so this container already has Rust (`cargo`, `clippy`, `rustfmt` at `/usr/local/cargo/bin`). Build checks:

```sh
cargo check
cargo clippy -- -D warnings && cargo fmt --check
```

(`mise` is not installed in the container; run cargo directly.) Design docs: `docs/design/DESIGN.md`, decisions in `docs/adr/`. Keep both updated when you change architecture-relevant behavior.

## Being a good tab citizen

- pall8t watches your screen: when you show an approval/input prompt, the human is notified and can jump to you (`^b n`). Just ask normally — no special protocol.
- Long-running work is fine; your tab shows "working" while you produce output.
- If the human closes your tab, this process ends but the workspace (and your commits) persist.
