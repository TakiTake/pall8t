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

> **Superseded — see the [amendment](#amendment-2026-08-23-the-socket-premise-was-wrong)
> below.** The first fact was an artifact of *how* pall8t asked for the
> mount, not of apple/container. The transport is now a mounted socket;
> the TCP relay and the in-container `socat` are gone. Everything else in
> this ADR — policy classes, version lockstep, env passthrough, the
> security posture — still stands.

## Decision

`pall8t run`, when it detects a herdr pane and `[herdr] sandbox` is not
`"off"`, assembles a **bridge**:

1. **Host relay** (`pall8t herdr relay`, hidden subcommand, spawned just
   before the exec): listens on an ephemeral TCP port *(superseded: a
   private Unix socket — see the amendment)*, forwards NDJSON
   requests to the real `herdr.sock`. It applies a per-request policy
   (below), writes an audit log (`~/.pall8t/logs/herdr-relay-<run>.log`),
   and only accepts connections whose peer address is the sandbox
   container's own IP (`container inspect`). It watches its parent — the
   pall8t pid that becomes the `container` client via exec — and exits on
   reparent, so its lifetime equals the session's.
2. **In-container bridge** *(superseded by the amendment: the relay's
   socket is mounted straight in, and the bootstrap no longer bridges
   anything)*: the run command is wrapped in a `sh` bootstrap
   that reads the gateway from `/proc/net/route` and runs `socat
   UNIX-LISTEN:/tmp/pall8t/herdr.sock,fork TCP:<gateway>:<port>`, so the
   stock herdr CLI finds a real Unix socket at `HERDR_SOCKET_PATH`. The
   default image ships `socat`; a missing `socat` degrades with a warning.
3. **Version-matched CLI**: pall8t reads the host's `herdr --version`,
   downloads that release's musl-static Linux build once into
   `~/.pall8t/tools/herdr/<version>/`. Version lockstep is load-bearing:
   the CLI refuses to talk across any protocol difference, and this makes
   the pin automatic — a brew-upgraded host herdr fetches its matching
   Linux build on the next run. Integrity: herdr's releases publish no
   checksums, so the first download trusts TLS to github.com
   (trust-on-first-use); its sha256 is recorded in a sidecar and
   re-verified before each use. Downloads use per-pid temp names so
   concurrent cold-cache runs publish complete files via atomic rename.
   Because apple/container has no read-only mounts, whatever is mounted is
   writable by the sandbox (same host uid), so the shared cache is **never
   mounted**: each run copies the verified binary into a private per-run
   directory (`~/.pall8t/tools/herdr-run/<container>/`) and mounts *that*
   at `/opt/pall8t/bin`. A sandbox can therefore corrupt only its own
   throwaway copy — breaking nothing but its own herdr CLI — and cannot
   reach the binary a concurrently running sandbox executes, nor the
   verified source. Per-run copies from exited (`--rm`'d) runs are pruned
   best-effort on later runs (kept while their container is live or the
   copy is within a grace window, so a concurrently launching run's copy
   is never reaped mid-launch).
4. **Env passthrough**: `HERDR_ENV=1`, `HERDR_{WORKSPACE,TAB,PANE}_ID`,
   `HERDR_SOCKET_PATH` (container path), `HERDR_BIN_PATH` — so herdr's
   published SKILL.md works inside the sandbox *unmodified*, including
   `--current` caller context.

### Policy: guardrail, not blocker

`[herdr] sandbox` (project overrides global): **`full`** (default),
`readonly`, `off`. Methods are classified:

- **Read** (list/get/read/current/layout/wait/subscribe…): always allowed.
- **Host admin**: every method in the `server.*`, `integration.*`,
  `plugin.*`, and `session.*` namespaces (`server.stop`,
  `integration.install`, `plugin.link`, …) except the exact read-only
  methods carved out above (`server.agent_manifests`, `plugin.list`,
  `session.snapshot`, …): always denied. These administer the host herdr
  installation or its lifecycle — herdr's own skill already tells agents
  never to do them, so denying them blocks nothing legitimate. Denial is
  *by namespace*, not by an enumerated list, so an admin method a newer
  herdr adds is denied before pall8t has heard of it.
- **Mutate** (split, prompt, agent start, send input, tabs, workspaces…):
  allowed in `full`, denied in `readonly`. Methods unknown to pall8t
  *outside* the admin namespaces classify as Mutate — transparent under
  `full`, safe under `readonly`. Failing closed for those too was
  considered and rejected: the CLI version-updates automatically while
  the classifier ships with pall8t, so a global deny-unknown would break
  the bridge on every herdr release that adds a workspace-surface method
  — a blocker, which this design explicitly is not. The namespace rule
  covers where admin methods actually land; the residual risk is a future
  admin method placed outside those namespaces, accepted and revisitable.

Orthogonal to the three classes, and denied in **every** mode: a request
whose `params.source` starts with `herdr:`. Those sources are herdr's own
integrations — it recognizes them as official, stores the session
identity they report, and after a server restart resumes such a pane by
running the agent's own resume command in it, **on the host, outside the
sandbox** (herdr 0.8.2: `persist/restore.rs` -> `agent_resume::plan`). A
sandboxed agent claiming one would arrange for its own unsandboxed
resurrection, so the bridge refuses it and says why. Reports under any
other source — herdr documents `custom:<name>` for third parties — pass
untouched. This costs nothing in state detection: herdr's Claude Code
integration carries session identity only, and its state authority is the
screen manifest either way.

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
surface; the compensations are binding to the vmnet gateway address
(falling back to `0.0.0.0` only if it can't be resolved), peer-pinning to
the one container's address, the always-denied host-admin class, and the
audit log. Note what the bridge does and does not change: it runs the
container as the host user, so it grants no privilege *above* that user —
but it does give code inside the sandbox a new *capability* it otherwise
lacks, namely reachability to the host herdr session across the sandbox
boundary. That new reach is the whole point in `full` mode and is exactly
what `readonly`/`off` and the policy classes govern; same-uid parity
bounds the blast radius (a sandbox can do nothing to herdr the host user
couldn't), it does not make the opening a non-event.

## Consequences

- A developer using herdr + pall8t gets herdr's documented agent workflow
  inside the sandbox with zero setup; herdr's upstream SKILL.md needs no
  pall8t-specific edits.
- ~~The default image grows `socat`; custom Containerfiles need it.
  Without `socat` the bootstrap only warns — `HERDR_SOCKET_PATH` then
  points at a socket that never exists, so the stock herdr CLI and
  Unix-socket clients cannot connect; only a client that reads
  `PALL8T_HERDR_PORT` and speaks TCP to the gateway directly can still
  reach the relay.~~ **Superseded by the amendment**: no image needs
  `socat`, and the socket exists whenever the mount did.
- First bridged run needs network access to download the Linux herdr CLI;
  it's cached per version afterwards.
- herdr's screen-scrape agent detection still comes from the host-side
  argv0 hint (unchanged); in-container integrations that report state over
  the socket (herdr's `integration install` hooks in the container home)
  also work. `HERDR_ENV=1` is set whenever the bridge is *configured*; it
  is still not proof the socket is *up* — bridge setup is best-effort, and
  a run whose relay failed to start warns on the host and proceeds with
  the env set but no socket mounted. A client should check rather than
  assume. (Before the amendment there was a second way to get
  `HERDR_ENV=1` with no socket: an image without `socat`. That one is
  gone.)
- `pane.move` responses, closed-pane ID rules, etc. pass through untouched
  — pall8t interprets nothing but the method name.

## Amendment (2026-08-23): the socket premise was wrong

`ENOTSUP` was real, but it was the answer to the wrong question. pall8t
emits every mount as `--mount type=virtiofs,…` (ADR-0009 explains why:
`-v`'s third field is unvalidated). On container 1.2.2:

- The **CLI parser** rejects any `--mount` whose source is not a directory
  (`path '…' is not a directory`), so a socket never gets that far.
- The **runtime** has always treated a mount whose source is a Unix socket
  as a socket to forward into the guest, not a filesystem to mount
  (`UnixSocketConfiguration(direction: .into)` — the same mechanism
  `--ssh` uses for the agent socket).
- `-v host.sock:/guest.sock` skips the parser's directory guard and
  reaches that runtime path.

Verified live on 1.2.2 against a host `AF_UNIX` echo server: the guest
sees a real socket, connects, and the host process receives the payload
and replies. Also verified: the guest node carries the **host's** file
mode (so a 0600 host socket is unreachable from a non-root guest process,
and 0666 is reachable by `dev`), and the runtime creates the mount
target's parent directory.

### What changes

1. The relay listens on a **Unix socket of its own** at
   `~/.pall8t/run/<container>.sock` (name truncated with a hash suffix
   when `sun_path`'s 104-byte budget demands it), mode `0666` inside a
   `0700` directory. It prints that path as its readiness line, because
   `container run` needs the socket to exist before it can take it as a
   mount source.
2. That socket is mounted into the sandbox at `HERDR_SOCKET_PATH`
   (`Mount::socket`, emitted as the two-field `-v` — the one form the
   runtime accepts for a socket; tracked upstream as pall8t#52).
3. The in-container bootstrap loses the `socat` bridge and the
   `/proc/net/route` gateway lookup; it now only prepends
   `/opt/pall8t/bin` to `PATH` and execs. **Custom Containerfiles no
   longer need `socat`**, and the shipped default image no longer
   installs it.
4. `PALL8T_HERDR_PORT` is gone, as are the peer-IP pinning and
   `container::ip_address`/`default_gateway` that served it.
5. Sockets left by an exited run are reaped by the next run: a socket
   nothing is listening on is stale by definition, and a live one belongs
   to a concurrent sandbox and is left alone.

### Security posture, restated

This is a narrowing. The TCP listener was reachable by every process on
the machine and every container on the vmnet, which is what the peer-IP
gate existed to compensate for. The listening socket is reachable only
through a `0700` directory owned by this user — a user who can already
connect to `herdr.sock` itself — and each run's socket is mounted into
exactly one container. The `0666` mode on the socket file governs access
*inside* the VM, where the only principal is the sandboxed agent, which
is precisely who should reach it.

The policy classes and the audit log are untouched, and deliberately so:
the relay is still the only thing the sandbox can talk to, and every
request still passes `classify` before it reaches `herdr.sock`. Mounting
`herdr.sock` itself would have been simpler and is exactly what this
design refuses — it would hand the sandbox the unfiltered API.
