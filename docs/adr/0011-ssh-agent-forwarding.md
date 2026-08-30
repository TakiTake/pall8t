# ADR-0011: SSH agent forwarding, off by default

- Status: Accepted
- Date: 2026-08-30
- Extends: [ADR-0009](0009-explicit-mounts.md) (what the sandbox may reach on the host) and [ADR-0007](0007-herdr-bridge.md) (an opt-in, audited capability rather than an ambient one)
- Does **not** affect: the workspace mount, the container home, or the herdr bridge

## Context

A sandboxed agent that can read a repository but not push to it does
half a job. The workspace is mounted, `git` is in the image, and the
commit succeeds — then `git push` fails, because the sandbox holds no
credential the remote will accept.

Two ways to give it one:

1. **Copy a private key into the container home.** `~/.pall8t/home` is
   mounted at `/home/dev` on every run, so a key dropped in `.ssh/`
   reaches every sandbox. It also *stays* there after the run, readable
   by every later run and by anything that ever executes in one. The key
   material itself crosses the boundary.
2. **Forward the host's agent.** apple/container has carried a bare
   `--ssh` flag since well before the 1.2.0 this project requires. The
   runtime reads `SSH_AUTH_SOCK` from the CLI process's environment,
   forwards that socket into the guest at
   `/var/host-services/ssh-auth.sock`, and sets the guest's own
   `SSH_AUTH_SOCK` to it. Signing requests cross the boundary; the key
   does not.

The second is strictly the safer mechanism, and it is what the runtime
already offers. The question this ADR settles is not *which* mechanism —
it is whether the capability should be on by default, and what pall8t
owes the user when the mechanism is in play but cannot work.

Three facts, verified on apple/container 1.2.2 by reading
`RuntimeService.sshAuthSocketHostUrl` and `configureInitialProcess`:

| condition | what the runtime does |
| --- | --- |
| `--ssh`, `SSH_AUTH_SOCK` set and live | forwards the socket; guest `SSH_AUTH_SOCK` points at it |
| `--ssh`, `SSH_AUTH_SOCK` unset | logs to its **own** log, forwards nothing — **and still sets guest `SSH_AUTH_SOCK`** |
| no `--ssh` | forwards nothing, sets nothing |

The middle row is the trap. The guest gets a path with no socket behind
it, so the only symptom the user sees is `ssh` failing to connect to
something that was never there — and the explanation is in a log they
have no reason to open.

## Decision

**Forward the host's SSH agent on request, never by default.**

```toml
[container]
ssh = true          # default: false
```

`pall8t run --ssh` turns it on for one run; `--ssh=false` turns it off
for one run. Same precedence as every other override in this codebase —
flag beats config beats default (`config::ssh_enabled`, mirroring
`config::mount_readonly`).

**Off by default, even though it is the safer of the two mechanisms.**
The comparison that matters is not "forwarding vs. a copied key" — it is
"forwarding vs. nothing". While the run lasts, code in the sandbox can
authenticate as the user anywhere the user's keys are trusted: every
host they can reach, every repository they can push to, including ones
the run has nothing to do with. An agent is not required to be
adversarial for that to be the wrong ambient default; it only has to be
wrong. A capability that broad is worth one word of config.

**Only the human may switch it on.** `[container] ssh` is read from the
user's own `~/.pall8t/config.toml` or from `--ssh`; a project's
`.pall8t/config.toml` may turn forwarding **off**, never on, and is told
when its request was ignored. The asymmetry is deliberate and is the one
rule in this ADR that is about trust rather than ergonomics:

> A project config ships with the repository, so it is authored by
> whoever wrote the repository — which is exactly the code the sandbox
> exists to contain. Project config may shape what runs **inside** the
> box (the command, the image, the mounts it needs); it must not be able
> to widen what the box can reach **outside** it.

Without that rule, cloning a repository whose `.pall8t/config.toml` says
`ssh = true` and running `pall8t run` hands that repository's code the
user's agent, with nothing on screen saying so. Narrowing stays free:
a project declining a capability is always safe to honor.

**Warn when forwarding is on and the host has no agent.** Because the
runtime's own report goes to its own log, pall8t checks first and says
so on stderr (`config::ssh_warning`). Two ways to have no agent, not
one: `SSH_AUTH_SOCK` unset, and `SSH_AUTH_SOCK` *stale* — a shell
resumed after a reboot, or a long-lived tmux session, still exports the
path of a socket that died with the agent that made it. Testing only for
unset would let the more confusing case through in silence. The
existence probe is passed in rather than performed inside, per
`docs/testing.md`; a probe that cannot answer counts as absent and warns,
since a spurious warning costs a line of text and a spurious silence
costs the failure this exists to prevent.

**Say so when it is actually on**, not only when it is broken. A
capability this wide that announces itself only on failure is one a run
can carry unnoticed; the herdr bridge, which is narrower, already prints
`herdr bridge active`.

**Bake GitHub's host keys into the image.** A forwarded agent is only
half of what git-over-SSH needs. The sandbox runs non-interactively, so
an unknown host key is not a TOFU prompt anyone can answer — it is `git
push` dying on "Host key verification failed" without ever consulting
the agent it was handed. The default `Containerfile` therefore writes
GitHub's keys into `/etc/ssh/ssh_known_hosts` at build time, from the
authenticated `api.github.com/meta` rather than `ssh-keyscan`. The
project's own dev image already did this; the default image now does too.

## Consequences

- **A custom Containerfile needs the `known_hosts` line itself.** This is
  the sharp edge of putting the keys in the image, and it is documented
  in the README rather than enforced: pall8t does not parse a user's
  Containerfile and has no way to check. The failure is loud (`ssh`
  refuses and says why), but it names the host key, not the missing
  build step.
- **Keys are frozen at image build.** If GitHub rotates a host key, a
  cached image serves the old set until it is rebuilt. `pall8t build`
  re-runs the layer, so the fix is a rebuild — but nothing detects the
  need for one.
- **Host-side seeding was considered and deferred.** Writing
  `known_hosts` into `~/.pall8t/home/.ssh` at run time would cover every
  Containerfile at once, custom ones included, and would pick up rotated
  keys without a rebuild. It also means pall8t fetching from
  `api.github.com` on the run path (or caching, with a staleness policy
  of its own), and merging into a file the user owns and may have
  written themselves. That is a subsystem, not a line; it is the right
  follow-up if the two consequences above start biting, and it can
  replace the image bake without changing anything users write.
- **The whole agent is forwarded, not one key.** There is no per-key or
  per-host narrowing in the SSH agent protocol as forwarded here: every
  identity loaded in the agent is usable by the sandbox, against every
  host that trusts it, for the length of the run. Three mitigations are
  worth knowing and none of them is pall8t's to apply:
  - `ssh-add -c` — the agent asks the human to confirm **each** signature.
    This is the strongest control available and turns a silent capability
    into an observable one.
  - `ssh-add -t <seconds>` — the key expires from the agent on a timer.
  - Run a dedicated agent holding only the key that run needs, and point
    `SSH_AUTH_SOCK` at it. This is the narrowest option and composes with
    the two above.
- **For GitHub specifically, a scoped token is the tighter credential.**
  A fine-grained PAT can be limited to chosen repositories and
  permissions; an SSH *user* key cannot be scoped at all — it is
  everything that key can reach. pall8t already carries a token into the
  container home for `gh`. Agent forwarding earns its place where a token
  is not an option (a self-hosted git server, a bastion, a signing key),
  not as the default answer to "push to GitHub".
- **Forwarding exposes the host's `ssh-agent` to the sandbox.** No key
  material crosses, but a *channel* does: the sandbox can send arbitrary
  agent-protocol messages to a process running as the user on the host.
  That is the class CVE-2023-38408 lived in (RCE via the agent's PKCS#11
  provider, reachable from the forwarded side, fixed in OpenSSH 9.3p2).
  "The key does not cross the boundary" is true and is not the same
  claim as "nothing crosses the boundary".
- **Nothing audits what the sandbox signs.** The herdr bridge logs every
  request it forwards; the agent socket is an unlogged byte pipe, and
  pall8t cannot say what was signed or for whom. Parsing the agent
  protocol to change that is not planned — the honest position is that
  this capability is unaudited, and `ssh-add -c` is where the visibility
  has to come from.
- **`ssh = true` with no agent is a warning, not an error.** The run
  proceeds. Forwarding is one thing a run does, not its purpose, and a
  run that would otherwise succeed should not be blocked by it.
- **Nothing is forwarded to `pall8t exec`.** The flag belongs to the run
  that asked for it.
