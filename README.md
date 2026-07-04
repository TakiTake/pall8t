# pall8t

*(pronounced "pallet" — the thing containers ship on)*

A Rust + ratatui TUI that manages per-project sandboxed dev containers on macOS using [apple/container](https://github.com/apple/container), with correct host↔container file ownership. Pick a project, browse its files, and open terminal tabs that are transparently *inside* the project's container — where you run `claude`, or any shell command.

![pall8t TUI mockup](docs/design/tui-mockup.svg)

## Why

- Run `claude` (YOLO mode included) in a throwaway VM, never on the host — blast radius is the project dir.
- Files created in the mounted project are owned by *your* host UID, never root (host UID/GID baked into the image at build time).
- DevContainer-like UX without an IDE: one keep-alive container per project, created lazily, reused across tabs.
- "New tab" is a real terminal tab (Ghostty / iTerm2 / Terminal.app / WezTerm / kitty), already `exec`'d into the container.

## Requirements

- macOS on Apple silicon, [apple/container](https://github.com/apple/container) installed and started (`container system start`)
- Rust toolchain (for now: build from source)

## Install & run

```sh
cargo install --path .
pall8t .        # add the current directory as a project and open the TUI
```

## Keymap

| Key | Action |
| :---- | :---- |
| `Enter` | Open a terminal tab with a shell inside the project's container (creates/starts it as needed) |
| `c` | Same, but runs `claude` directly |
| `s` | Start/stop container |
| `b` | (Re)build the image for this project |
| `L` | Show container logs |
| `a` / `d` | Add project / remove entry |
| `j/k`, arrows | Navigate; `h/l` collapse/expand dirs |
| `g/G`, `/` | Top/bottom, filter (file tree) |
| `Tab` | Move focus between panes |
| `r` | Force refresh |
| `q` | Quit (containers keep running) |

## Config

`~/.config/pall8t/config.toml` — see [docs/design/DESIGN.md](docs/design/DESIGN.md) for the full design, and [docs/adr/](docs/adr/) for architecture decisions.
