---
name: pall8t-herdr
description: "Delegate work to a sibling agent from inside a pall8t sandbox, over the herdr bridge. Use when this session runs in a pall8t sandbox with HERDR_ENV=1 and the task involves asking, waiting on, or reading another herdr agent. Covers the settled-state trap that makes a cross-pane wait appear to hang."
---

# Delegating across pall8t sandboxes with herdr

`pall8t run` *can* bridge the sandbox to the host herdr session (ADR-0007), so
the stock `herdr` CLI works from inside the container — but only when the run
started from a herdr pane, `[herdr] sandbox` is `"full"` or `"readonly"`, and
bridge setup succeeded. Setup is best-effort: a failure warns and the run
continues without it, so verify before relying on it (see *Before you delegate*).
herdr's own skill (`herdr --skill`) is the authority on the CLI. This skill
covers only what is specific to driving *another* agent from *inside* a
sandbox — above all, the one mistake that makes a working setup look broken.

## The rule: never pass `--until idle`

```sh
herdr agent prompt <target> '<request>' --wait --timeout 180000   # correct
herdr agent prompt <target> '<request>' --wait --until idle       # always times out
```

`--until idle` **can never match a pane you are not looking at.** In herdr,
`idle` means "ready for input *and* its tab has been seen in the focused UI".
A tab the human is not watching settles into `done` — the same underlying idle
state, relabelled. Reading the pane through the CLI does not mark it seen.

Delegation is exactly that case: you are in your pane, the target is in theirs.
So the target finishes in seconds, sits in `done`, and your wait blocks until
`--timeout` and returns `{"code":"timeout"}` — after which it is easy to
conclude, wrongly, that the bridge stalled or the request never arrived.

The one thing that clears `done` is the tab being focused — by the human, or by
`pane focus` / `agent focus`. So `--until idle` *appears* to work whenever the
human happens to be watching the target, which is what makes it survive casual
testing and fail exactly when the delegation is unattended.

The default `--wait` already matches `idle`, `done`, and `blocked`. Do not
narrow it. herdr's own skill says the same: "For normal agent work, `--wait` is
enough... Do not repeat those defaults with `--until`." Reserve `--until` for a
state-specific workflow, such as `--until blocked` to catch an approval prompt.

## Recognising it

`explain` reports the detection verdict; `list` reports the seen-adjusted
status. When they disagree, this is what you are looking at:

```sh
herdr agent explain <target>   # state: idle   <- detection: ready for input
herdr agent list               # <target> done <- seen-adjusted: --until idle can't match
```

Measured on a live pair: the default `--wait` returned in **2.2 s** with
`agent_status: done`, while the identical call with `--until idle` ran its full
timeout and errored even though the target had answered within seconds. In the
original report the target settled **3.3 s** after submission and the caller
stayed blocked a further **121 s**.

## If a wait really does hang, diagnose in this order

1. **`--until`** — the above. Cheapest to rule out, and by far the most likely.
2. **The target's own turn** — sample `herdr agent list` a few times. If it is
   `working`, it is simply still working; if it went `blocked`, it is waiting on
   an approval dialog, and `--wait` returns that state rather than hanging.
3. **The bridge, last.** Every forwarded request is audit-logged on the host to
   `~/.pall8t/logs/herdr-relay-<container>.log` (the container name already ends in the run's pid): a `start` line, then one
   timestamped JSON line per forwarded request — which tells the human exactly
   when each call crossed, and so whether a gap is before or after the bridge.
   The bridge is a byte pump with no buffering: the same delegation measured
   4.4 s host-direct, 4.0 s through the relay socket, and 2.0 s from inside the
   container. It does not add latency, so suspect it only after 1 and 2.

## What the bridge refuses

A denied request comes back as a real herdr error, not a hang:

```json
{"id":"…","error":{"code":"sandbox_denied","message":"pall8t blocked `agent.prompt` from the sandbox ([herdr] sandbox = \"readonly\"; …)"}}
```

That is host policy (`[herdr] sandbox` in `.pall8t/config.toml`), not a
malfunction. Report it to the human rather than working around it.

- `"full"` (default) — inspection and mutation; host-admin namespaces
  (`server.`, `integration.`, `plugin.`, `session.`) are denied in every mode.
- `"readonly"` — inspection only; `agent.prompt` is denied, so delegation is off.
- `"off"` — no bridge at all; `HERDR_ENV` is unset.

## Before you delegate

- Confirm the bridge is live with a cheap read — `herdr pane current --current`.
  `HERDR_ENV=1` alone is not proof: the socket and the Linux `herdr` binary are
  provisioned best-effort at run time.
- `herdr agent list` is the authority on target names. Address an agent by its
  name; do not guess one from a tab label you have not verified.
- Panes, agents, and commands you create through the bridge run **on the host,
  outside this sandbox**. That is a deliberate, audited opening for multi-agent
  coordination — not a loophole to route around sandbox limits.
- `agent prompt` types into whatever the target's input box already holds. If
  the human left an unsent draft there, your text joins it. Worth a glance with
  `herdr agent read <target>` when a reply looks garbled.
