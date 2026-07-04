# pall8t — Claude in an apple/container

> Formerly named "cabin"; renamed to pall8t (pronounced "pallet") for uniqueness. See ADR-0002.

A Rust \+ ratatui TUI that manages per-project sandboxed dev containers on macOS using [apple/container](https://github.com/apple/container), with correct host↔container file ownership. Its job: pick a project, browse its files, and open terminal tabs that are *transparently inside* the project's container — where you run `claude`, or any shell command.

## 1\. Goals / Non-goals

**Goals**

- Run `claude` CLI (and arbitrary shells) inside an apple/container VM, never on the host.  
- Files created in the mounted project dir are owned by the host user — never root.  
- DevContainer-like UX without IDE support: one container per project, created lazily, reused across tabs.  
- "New tab" \= a real terminal tab (Terminal.app / iTerm2 / WezTerm / kitty / Ghostty) whose shell is already `exec`'d into the container. No manual `container exec` typing.  
- Lightweight filer: browse the project tree, preview text files. Read-only by design.

**Non-goals**

- Full two-pane file manager (use yazi inside the container tab if needed).  
- Embedded terminal emulation inside the TUI (fragile; real tabs are better).  
- devcontainer.json compatibility (may come later; see Roadmap).

## 2\. The UID problem and the strategy

apple/container runs each container in a lightweight VM; bind mounts go through virtiofs. There is no Podman-style `--userns keep-id` yet ([apple/container\#165](https://github.com/apple/container/issues/165)). Two failure modes:

1. Container process runs as root → new files in the bind mount show up root-owned.  
2. Container process runs as an arbitrary non-root UID → can't write files owned by your host UID inside the mount.

**Strategy: bake the host UID into the image at build time.**

- `pall8t` builds a per-user image (`pall8t-base:<uid>-<gid>`) from its Containerfile with `--build-arg UID=$(id -u) --build-arg GID=$(id -g)`, creating user `dev` with exactly your host UID/GID, a real home dir, and passwordless sudo.  
- Containers are started with `--user dev`, so every process — shell, claude, compilers — creates files as your UID. Host side sees your ownership; container side can read/write.  
- Belt-and-suspenders: `container run/exec --uid/--gid` are also passed, so even if the image is swapped for one without the `dev` user, processes still run as your UID (they just lack a passwd entry/home, which the baked image provides).

## 3\. Container lifecycle

One keep-alive container per project:

name  \= pall8t-\<slug(dirname)\>-\<sha256(abs\_path)\[..8\]\>     e.g. pall8t-myapp-3fa9c21b

mounts:

  \<project abs path\>        \-\> /work            (the project)

  \~/.pall8t/home             \-\> /home/dev        (persistent shared home: claude auth,

                                                 shell history, dotfiles survive rebuilds)

run:

  container run \-d \--name \<name\> \\

    \-v \<project\>:/work \-v \~/.pall8t/home:/home/dev \\

    \-w /work \--user dev \--uid \<uid\> \--gid \<gid\> \\

    \--cpus \<n\> \--memory \<m\> \\

    pall8t-base:\<uid\>-\<gid\> sleep infinity

State machine per project: `Absent → Created/Stopped → Running`. The TUI reconciles on every tick via `container list --all --format json`, and lazily creates/starts on demand:

- Opening a tab on a project with no container: build image if missing → run container → spawn tab. Progress shown in the status bar.  
- `container exec -it --user dev -w /work <name> bash -l` is what every tab runs.  
- Stopped containers are restarted with `container start`, not recreated (cheap).  
- Deleting a project entry offers to `container delete` its container. Project files are never touched.

**Claude auth persistence.** `~/.pall8t/home` is the container-side `$HOME`, so `claude` login state (`~/.claude`, `~/.claude.json`) persists across containers and rebuilds, and is *isolated from the host's* claude credentials. Log in once inside any pall8t tab; done. (Alternative — mounting host `~/.claude` — was rejected: apple/container `-v` mounts directories, `~/.claude.json` is a file, and credential isolation is the point of sandboxing.)

## 4\. Base image (Containerfile)

FROM ubuntu:24.04

ARG UID=501

ARG GID=501

\# node \+ claude CLI \+ common tools; dev user with host UID/GID

RUN apt-get update && apt-get install \-y \--no-install-recommends \\

      ca-certificates curl git sudo ripgrep less vim openssh-client && \\

    curl \-fsSL https://deb.nodesource.com/setup\_22.x | bash \- && \\

    apt-get install \-y nodejs && npm i \-g @anthropic-ai/claude-code && \\

    (getent group ${GID} || groupadd \-g ${GID} dev) && \\

    useradd \-m \-u ${UID} \-g ${GID} \-s /bin/bash dev && \\

    echo 'dev ALL=(ALL) NOPASSWD:ALL' \> /etc/sudoers.d/dev

USER dev

WORKDIR /work

Users can point `image` in config at their own Containerfile per project; pall8t passes the same UID/GID build args.

## 5\. TUI layout

![pall8t TUI mockup](tui-mockup.svg)

Three areas: project list (left), file tree \+ preview (right), status/keybar (bottom). `Tab` moves focus between panes.

**Keymap**

| Key | Action |
| :---- | :---- |
| `Enter` | Open terminal tab with a shell inside the project's container (creates/starts it as needed) |
| `c` | Same, but runs `claude` directly |
| `s` | Start/stop container |
| `b` | (Re)build the image for this project |
| `L` | Show container logs in a pager overlay |
| `a` / `d` | Add project (path prompt with completion) / remove entry |
| `j/k`, arrows | Navigate; `h/l` collapse/expand dirs in file tree |
| `g/G`, `/` | Top/bottom, filter |
| `r` | Force refresh |
| `q` | Quit (containers keep running) |

The filer is read-only: navigate, expand dirs, preview text files (first \~200 lines, binary detection). Anything mutating happens in a container tab, as your UID.

## 6\. Terminal tab integration

`pall8t` detects the hosting terminal via `$TERM_PROGRAM` and spawns tabs natively:

| Terminal | Mechanism |
| :---- | :---- |
| iTerm2 | AppleScript: `create tab with default profile`, `write text "<cmd>"` |
| Terminal.app | AppleScript `do script` (new window; true tabs need Accessibility perms — documented) |
| WezTerm | `wezterm cli spawn -- <cmd>` |
| kitty | `kitty @ launch --type=tab <cmd>` (requires `allow_remote_control`) |
| Ghostty | AppleScript fallback / `open -na Ghostty` with command args |
| unknown | Print the exec command \+ copy to clipboard (`pbcopy`) |

The spawned command is always the raw `container exec -it --user dev -w /work <name> <shell-or-claude>` — so the tab keeps working even if pall8t exits. pall8t guarantees the container is running *before* the tab opens, so there's no race.

## 7\. Config

`~/.config/pall8t/config.toml`:

default\_image \= "pall8t-base"       \# tag suffix :\<uid\>-\<gid\> is appended

cpus \= 4

memory \= "4G"

\[\[projects\]\]

name \= "myapp"

path \= "/Users/you/src/myapp"

\# image \= "my-custom:dev"          \# optional per-project override

\# containerfile \= ".pall8t/Containerfile"

Projects added via `a` are appended here. pall8t also works with zero config: `pall8t .` adds the cwd as a project and selects it.

## 8\. Security notes

- Container has no access to host beyond the two mounts (project dir, pall8t home). SSH agent (`--ssh`) is opt-in per project, off by default.  
- Host claude credentials never enter the container; the sandboxed claude has its own login.  
- `sudo` inside the container is convenience only — root in the VM guest, not on the host; virtiofs writes still land as the mapped host-side owner.  
- YOLO-mode claude (`--dangerously-skip-permissions`) becomes reasonable here: blast radius is the project dir \+ throwaway VM.

## 9\. Roadmap

1. **v0 (this prototype):** project list, file browser \+ preview, lifecycle, image build, iTerm2/Terminal.app/WezTerm/kitty tab spawn, config persistence.  
2. **v0.2:** per-project Containerfile auto-detection (`.pall8t/Containerfile`), port publish UI, `--ssh` toggle, container stats in status bar.  
3. **v0.3:** minimal devcontainer.json subset (image, mounts, ports, postCreateCommand).  
4. **Later:** multiple containers per project, volume manager, session restore of tab sets.

## 10\. Architecture decision records

Decisions live in [`docs/adr/`](../adr/); this section only summarizes them.

**[ADR-0001: Implementation language — Rust over Swift](../adr/0001-implementation-language.md)** (Accepted, 2026-07-04). pall8t stays in Rust and integrates with apple/container by wrapping the `container` CLI rather than linking the Swift `ContainerClient` library. Key reasons: the XPC API is pre-1.0 and unstable (v0 compatibility was already removed in 0.12.x), so linking it creates a client/apiserver version-skew problem that CLI wrapping avoids entirely; the CLI provides JSON output (`container ls --format json`, `inspect`) for robust parsing; and the Rust TUI ecosystem (ratatui/crossterm/clap) has no Swift equivalent. Revisit if apple/container 1.0 ships a versioned XPC API *and* pall8t needs features the CLI can't provide — escape hatch is a small Swift helper binary (XPC in, JSON out), not a rewrite.

**[ADR-0002: Rename cabin → pall8t](../adr/0002-rename-to-pall8t.md)** (Accepted, 2026-07-04). "cabin" was too generic (crates.io/search collisions); "pall8t" keeps the pallet-under-containers metaphor, is collision-free, and stays pronounceable.

