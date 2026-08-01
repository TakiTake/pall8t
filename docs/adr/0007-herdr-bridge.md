# ADR-0007: herdr bridge — the herdr CLI works inside the sandbox

- Status: Accepted
- Date: 2026-08-01
- Extends: ADR-0006 (headless runner; herdr owns multiplexing)

## Context

ADR-0006 made pall8t a foreground CLI that herdr spawns, and the host-side
integration (argv0 agent hint, `report-metadata`) makes the *pane* look
right. But the agent *inside* the sandbox is blind to herdr: pall8t
forwards no environment, so herdr's own agent skill
([skills/herdr/SKILL.md](https://github.com/ogulcancelik/herdr/blob/main/skills/herdr/SKILL.md))
fails its very first gate (`test "$HERDR_ENV" = 1`) and stops. A developer
using both tools gets herdr's coordination surface (inspect neighboring
panes, split, start a reviewer agent, wait on it) only for *unsandboxed*
agents — the moment they sandbox one with pall8t, that surface vanishes.

What the herdr CLI actually needs is small: newline-delimited JSON over a
Unix socket (`herdr.sock`), no authentication beyond the socket file's
`0600` mode, endpoint fully redirectable via `HERDR_SOCKET_PATH`, plus the
`HERDR_ENV`/`HERDR_{WORKSPACE,TAB,PANE}_ID` caller context. herdr also
ships musl-static Linux builds of the CLI, and its CLI hard-fails on any
protocol-version difference with the server.

Two facts about apple/container, verified live on 1.1.0, shape the design:

- **Unix sockets do not cross the mount boundary.** `connect(2)` on a
  bind-mounted socket file fails with `ENOTSUP` (virtiofs). Mounting
  `herdr.sock` into the container can never work.
- **TCP to the host does work.** The container reaches the host at its
  vmnet gateway address (`/proc/net/route`), and the host sees the
  container's own address as the peer.

## Decision

`pall8t run`, when it detects a herdr pane and `[herdr] sandbox` is not
`"off"`, assembles a **bridge**:

1. **Host relay** (`pall8t herdr relay`, hidden subcommand, spawned just
   before the exec): listens on an ephemeral TCP port, forwards NDJSON
   requests to the real `herdr.sock`. It applies a per-request policy
   (below), writes an audit log (`~/.pall8t/logs/herdr-relay-<run>.log`),
   and only accepts connections whose peer address is the sandbox
   container's own IP (`container inspect`). It watches its parent — the
   pall8t pid that becomes the `container` client via exec — and exits on
   reparent, so its lifetime equals the session's.
2. **In-container bridge**: the run command is wrapped in a `sh` bootstrap
   that reads the gateway from `/proc/net/route` and runs `socat
   UNIX-LISTEN:/tmp/pall8t/herdr.sock,fork TCP:<gateway>:<port>`, so the
   stock herdr CLI finds a real Unix socket at `HERDR_SOCKET_PATH`. The
   default image ships `socat`; a missing `socat` degrades with a warning.
3. **Version-matched CLI**: pall8t reads the host's `herdr --version`,
   downloads that release's musl-static Linux build once into
   `~/.pall8t/tools/herdr/<version>/`, and mounts it at `/opt/pall8t/bin`
   (prepended to `PATH`). Version lockstep is load-bearing: the CLI
   refuses to talk across any protocol difference, and this makes the pin
   automatic — a brew-upgraded host herdr fetches its matching Linux build
   on the next run.
4. **Env passthrough**: `HERDR_ENV=1`, `HERDR_{WORKSPACE,TAB,PANE}_ID`,
   `HERDR_SOCKET_PATH` (container path), `HERDR_BIN_PATH` — so herdr's
   published SKILL.md works inside the sandbox *unmodified*, including
   `--current` caller context.

### Policy: guardrail, not blocker

`[herdr] sandbox` (project overrides global): **`full`** (default),
`readonly`, `off`. Methods are classified:

- **Read** (list/get/read/current/layout/wait/subscribe…): always allowed.
- **Mutate** (split, prompt, agent start, send input, tabs, workspaces…):
  allowed in `full`, denied in `readonly`. Methods unknown to pall8t (a
  newer herdr) classify as Mutate — transparent under `full`, safe under
  `readonly`.
- **Host admin** (`server.stop`, `server.live_handoff`,
  `server.reload_config`, `server.reload_agent_manifests`,
  `integration.install/uninstall`, `plugin.link/unlink/enable/disable`):
  always denied. These administer the host herdr installation itself —
  herdr's own skill already tells agents never to do them, so denying
  them blocks nothing legitimate.

Denied requests get a herdr-shaped error
(`{"id":…,"error":{"code":"sandbox_denied",…}}`) naming the config knob,
so the in-container CLI fails legibly and the agent knows it's a policy,
not a bug.

### Security posture, stated plainly

`full` mode deliberately lets a sandboxed agent mutate the host herdr
session — split panes, start agents, type into panes. Those panes run **on
the host**: this is a controlled, audited opening in the sandbox wall,
chosen because coordinating sibling agents is the entire point of running
pall8t under herdr. Users who want the wall solid set `readonly` or `off`.
Compared to herdr's own `0600` socket, the TCP listener is a broader
surface; the compensations are peer-pinning to the one container's
address, the always-denied host-admin class, and the audit log. Same-host
same-user processes could already reach `herdr.sock` directly, so the
relay adds no privilege they didn't have.

## Consequences

- A developer using herdr + pall8t gets herdr's documented agent workflow
  inside the sandbox with zero setup; herdr's upstream SKILL.md needs no
  pall8t-specific edits.
- The default image grows `socat`; custom Containerfiles need it (or the
  bridge degrades to env-plus-relay for raw-socket clients only).
- First bridged run needs network access to download the Linux herdr CLI;
  it's cached per version afterwards.
- herdr's screen-scrape agent detection still comes from the host-side
  argv0 hint (unchanged); in-container integrations that report state over
  the socket (herdr's `integration install` hooks in the container home)
  now also work, since the socket surface is present.
- `pane.move` responses, closed-pane ID rules, etc. pass through untouched
  — pall8t interprets nothing but the method name.
