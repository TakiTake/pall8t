# ADR-0011: Tab numbering is pall8t's own, counted per herdr server run

- Status: Accepted
- Date: 2026-09-05
- Supersedes: the numbering half of the `auto_rename` feature as shipped in 0.5.0 (issue #71) and the position-based scheme that briefly replaced it (issue #76). The *naming* design — one string for both tab and agent, the two halves landing at different times, the label-ownership rule — is unchanged.
- Introduces: `~/.pall8t/state/`, the first directory pall8t writes for itself and reads back on a later run

## Context

`auto_rename` names a herdr tab and its agent `<base>-<number>`, so the
name a human reads off the tab is the name they can type as a target. The
`<base>` half was never in question. The `<number>` half has now been wrong
twice, in two different ways, and both failures were found by running the
thing rather than by review.

**Attempt 1 — the tab id's counter** (`w1E:t3` → 3). Stable per tab, needs no
herdr call. But herdr persists `next_public_tab_number` per workspace in
`~/.config/herdr/session.json` and never decrements it, so it does not
restart when the server does: a fresh workspace opens at whatever count the
previous ones reached. `foo-14` as the first tab of a new project reads as
noise. That is issue #76.

It was also quietly broken. herdr encodes ids in bijective base-32 over
`"123456789ABCDEFGHJKMNPQRSTVWXYZ0"`, so the tenth tab in a workspace is
`tA`, and `strip_prefix('t')?.parse()` returned `None` for it and everything
above — the suffix silently vanished past nine tabs.

**Attempt 2 — the tab's position in its workspace.** This is the number
herdr's own default label shows, which is what made it attractive: pall8t's
suffix would read as the number a human would have seen anyway. Live testing
broke it within a day, twice:

- *Names collide.* Closing `vpnp-1` moved the tab that had been position 2
  down to position 1 **with its `vpnp-2` label intact** — nothing relabels a
  tab pall8t is not running in. The tab opened in its place landed at
  position 2, derived `vpnp-2`, and both tabs wore it. `agent.list` could
  not report the clash because the older tab's agent had exited. herdr
  enforces no uniqueness on labels, so the duplicate was written to
  `session.json` and survived.
- *Names shift under living tabs.* More fundamentally: a position belongs to
  the *list*, not to the tab. Every close renumbers everything after it, so
  a name written yesterday stops meaning the tab it was written for.

The second point is the real one. A tab's name is an address other agents
type; an address that changes when an unrelated neighbour closes is not an
address.

## Decision

**pall8t keeps the counter itself**, in `~/.pall8t/state/herdr-naming.json`:
per base name, handed out once, never reused while one herdr server run
lasts, and restarted when a new server takes over.

**A server run is identified by its API socket's `(dev, ino, birthtime)`.**
herdr exposes no session id, pid or start time anywhere — `ping` answers
version, protocol and capabilities, `herdr status --json` has no pid, and
there is no `server.info`. But herdr unlinks and re-binds the socket on
every start, and keys its *own* shutdown ownership on that socket's
`(dev, ino)`. So the identity already exists; it simply is not in the API.
pall8t knows the path without guessing, because herdr sets
`HERDR_SOCKET_PATH` on every pane process it spawns.

Three rules make that coherent against herdr's own persistence:

- **Unknown keeps counting.** An unreadable socket never triggers a reset.
- **A tab adopts the number its own label already carries**, so a tab herdr
  restored as `foo-3` stays `foo-3` rather than being renamed by a counter
  that just went back to 1.
- **A restarted counter starts past the labels still on screen**, so new
  tabs continue the visible sequence instead of walking into it.

An exclusive `flock` held across the read-modify-write, and an atomic rename
to publish, keep two concurrent `pall8t run` from taking the same number.

## Consequences

**Numbers no longer restart on their own.** A machine left up for a week,
opening and closing tabs, reaches `myproject-47`. This is the direct cost of
"never reuse within a server run", and the opposite complaint to issue #76's.
It is the trade the user asked for: a stable address is worth more than a
small one.

**pall8t now has durable state of its own.** Everything under `~/.pall8t`
was previously either configuration the user wrote or a cache that could be
deleted without consequence. This file is neither: deleting it loses the
counters, which is recoverable (the next run seeds itself from the labels on
screen) but not free. `~/.pall8t/state/` is named so that what is
regenerable and what is not stay visibly apart.

**Two stores can be wiped independently.** pall8t's counters live in
`~/.pall8t/state/`, herdr's labels in `~/.config/herdr/session.json`.
Clearing one and not the other is survivable in both directions — the
adoption and seeding rules exist precisely so that a counter meeting labels
it never wrote does something sensible — but it is a seam worth knowing
about, and the README names the recovery move.

**Numbering no longer depends on any herdr call.** The number comes from a
local file, so a failed or reshaped `tab.list` costs the *tab* its rename
(ownership of the label is then unknown, and an unknown label is kept) but
never costs the agent its number. The fallback that previously patched this
is gone, along with the base-32 bug it carried.

**The label-ownership check had to become unbounded.** "Is this label one
pall8t wrote?" can no longer be answered by comparing against the number
this run holds, because after a restart every surviving tab's label carries
a number from the previous run's sequence. It is now answered by rebuilding
the name through the same code that writes it, at any number — which is both
cheaper than the bounded sweep it replaces and more accurate.

## Alternatives considered

**Smallest free number** (the lowest number no live label uses). No state
file, no server identity, and it returns to 1 exactly when the tabs holding
those names are gone. Rejected because it reuses a number as soon as a tab
closes: `foo-1` can become a different agent than the `foo-1` someone
messaged an hour ago, which is the same address-instability that sank the
position scheme, only rarer and therefore harder to notice.

**Ask herdr for a stable per-tab number.** There isn't one that resets, and
adding one is upstream's call, not pall8t's. If herdr ever exposes a server
run id, the socket stat becomes an implementation detail behind
`keeps_counting` and nothing else moves.

**Reset the counter to 1 unconditionally on a new server run.** Simpler, and
the literal reading of the requirement. Rejected once it was clear that
herdr restores `custom_name` verbatim: on *every* restart, for *every*
workspace with surviving pall8t-labeled tabs, the fresh counter would walk
straight into names still on screen and the collision walk would produce
`foo-1-2`. Adoption plus seeding keeps the reset where it is meaningful — a
genuinely empty session still starts at 1 — without manufacturing that.
