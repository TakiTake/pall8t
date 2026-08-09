# ADR-0009: Explicit `[[mounts]]`, with read-only support

- Status: Accepted
- Date: 2026-08-09
- Replaces: `[[repos]]` (FR-4 as written in [docs/requirements.md](../requirements.md)) — a **breaking config change**
- Supersedes, in part: the "apple/container has no read-only mounts" premise in [ADR-0004](0004-workspace-model.md) (Constraints, Alternatives) and [ADR-0006](0006-drop-tui.md) (Decision, Follow-ups), and the corresponding entries in requirements §7/§8. The identity-path insight both ADRs are built on is unaffected and becomes the *default* here.
- Does **not** affect: the workspace mount, the container home, or the herdr bridge's binary mount (see Consequences)

## Context

ADR-0004 recorded that apple/container had no read-only mount support ([apple/container#990](https://github.com/apple/container/issues/990), then an open feature request) and chose *protection by duplication*: each reference repo is cloned with `git clone --local` and the copy is mounted at the original's absolute path, so an agent that writes damages only the clone. ADR-0006 kept that when the TUI went away, and both left the same follow-up: *"#990 resolved → mount reference repos ro directly."*

The trigger fired, earlier than the docs assumed. #990 closed 2026-01-07, before 1.0.0 was ever tagged — so pall8t had been carrying a workaround for a constraint that no shipped version it supported actually had.

Since the ADRs record the limitation as verified, the fix needed verification of equal weight rather than a reading of upstream. Measured on apple/container 1.2.2, mounting a scratch directory and writing to it from inside an `alpine` container:

| mount flag | result |
| --- | --- |
| `--mount type=virtiofs,source=…,target=…,ro` | `Read-only file system` on both write and create |
| `-v src:dst:ro` | `Read-only file system` |
| `-v src:dst:readonlyy` (typo) | **mounts read-write, silently** |
| `--mount …,readonlyy` (typo) | `Error: unknown directive readonlyy`, before the container starts |
| `-v src:dst` (baseline) | writes through, as expected |

Enforcement is real, and the two flags are not equivalent. `Parser.volume` splits `-v` on `:` and hands the third field to the filesystem options unvalidated, while `Parser.mount` validates each `--mount` directive and throws on an unknown one.

That result reframes the whole feature. `[[repos]]` was never really about repositories: it cloned a repo *because cloning was the only way to protect one*. With read-only mounts available, the cloning is the accident and the mounting is the point — and the git requirement it dragged along (`source` had to contain `.git`, in every mode) was a consequence of `git clone --local` needing something to clone, not of anything a user wanted.

## Decision

Replace `[[repos]]` with **`[[mounts]]`: a literal mount primitive over any directory.**

```toml
[[mounts]]
source = "~/notes"      # any directory, git or not
target = "/notes"       # optional; defaults to `source` (identity mount)
readonly = true         # optional; defaults to false
```

- **No cloning.** The `git clone --local` machinery is deleted. What you mount is what the agent sees.
- **Any directory.** The `.git` requirement goes with the clone that needed it.
- **`target` defaults to `source`.** The identity mount stays the default because it is what keeps absolute paths meaningful on both sides — git metadata, build output, and anything the agent reports back all resolve the same way on the host. An explicit `target` is for when you want it somewhere else and accept that they diverge.
- **`readonly` defaults to `false`**, and `pall8t run --readonly[=BOOL]` overrides every entry for one run. Precedence: flag, then entry, then the default.
- Every mount pall8t emits goes out as `--mount`, never `-v`.

Writable is the default because a mount primitive that silently refused writes would surprise anyone who did not ask for that; the name says "mount this directory", so it mounts it. Read-only is the stronger protection and is one word away.

`--mount` over `-v` is not the same kind of choice — it is a safety fix that applies to every mount regardless of mode. A typo in a protection flag must fail the run rather than silently produce the unprotected mount, and the table above shows `-v` doing exactly that.

The `--reference` alternates half of ADR-0004's deferred design is **not** adopted, and is now moot: it belonged to the workspace-seeding model that ADR-0006 removed with the TUI.

## The breaking change

`[[repos]]` is a hard parse error naming its replacement, not a silent translation and not a deprecation warning. `[home]` got parse-and-warn (ADR-0008) because ignoring it left an experimental feature switched off; ignoring `[[repos]]` would leave a directory the user believes is mounted simply absent from the sandbox.

Auto-mapping was rejected for a sharper reason: the semantics do not survive translation. `[[repos]] source = X` mounted a *clone* of X, so agent writes never reached the real checkout. `[[mounts]] source = X` mounts X itself, where the same writes land on the user's files. Mapping one to the other would silently pick a protection level on the user's behalf — either weaker than they had (writable) or more restrictive than they chose (read-only). The error therefore shows the replacement *and* states the difference, so the migration is a decision rather than a paste.

## Alternatives considered

- **Keep `[[repos]]`, add `readonly` to it** — this is what this change originally did, and it shipped nothing: it kept the git-only restriction, kept the clone machinery on the default path, and left `readonly = false` meaning "mount a copy" — an indirection nobody could infer from the key's name.
- **Widen `[[repos]]` to accept any directory** — rejected: the name would then be a lie, and validation would be mode-dependent, since the clone path genuinely needs a git repo. A config that works with `readonly = true` and fails with `readonly = false` is a trap.
- **Keep the clone as a third mode (`mode = "copy"`)** — rejected for now: it is a real use case (an agent that must commit or fetch against a repo it may not modify) but it is a *workflow*, not a mount, and it can return as its own feature without holding up the primitive. The escape hatch today is to clone by hand and mount the clone.
- **`-v src:dst:ro`** — rejected on the evidence above. A misspelled option mounting read-write in silence is precisely the failure a protection flag must not have.

## Consequences

- **Existing configs break, loudly and once.** Every `[[repos]]` entry needs rewriting as `[[mounts]]`, and the error says how. Clones under `~/.pall8t/repos` become dead weight and can be deleted; pall8t no longer reads or writes that directory.
- **An agent can now damage a real directory** if a mount is writable — something no `[[repos]]` config could do. That is the honest consequence of a literal primitive, and `readonly = true` is the answer. The run prints the mode of every mount so the choice is never invisible.
- **A target may not cover what the run is built on.** The workspace, a worktree's `.git`, and `/home/dev` are checked against every target, and targets are checked against each other. Covering the home would take out the agent's own config and session history — previously unreachable by config, and reachable now that targets are arbitrary, so the guard grew to match.
- **A read-only mount does not carry the uid/gid remapping.** Measured on 1.2.2 inside one container: the writable workspace appears as `501:20`, matching the host, while a read-only mount of a directory the host also shows as `501:20` appears as `0:0`. Contents stay readable (mode bits are unchanged), but git compares a repository's owner against its own euid and refuses every command with "detected dubious ownership". pall8t therefore passes `GIT_CONFIG_COUNT`/`KEY_n`/`VALUE_n` marking every read-only target as `safe.directory`. Scoped by path, and by environment rather than a config file, because `safe.directory = *` in the image would disable the check for every repository the sandbox ever sees.
- **A linked git worktree brings its main `.git` with it** on an identity mount, resolved with [`worktree::main_git_dir`] — the same helper FR-3 uses for the workspace — and inheriting the entry's mode. Mounting the worktree alone would hand the sandbox a directory git cannot read as a repository. Skipped for a retargeted mount, where the pointer file's absolute paths no longer line up.
- **`~` in `source` expands host-side**, so a mount lands at the *host's* absolute path inside the container, not under the container's `$HOME`. `~/x` is `/Users/you/x` inside the sandbox, not `/home/dev/x` — a real trip hazard when telling an agent where to look, and the reason `target` exists.
- **Every mount changed flag**, including the workspace and the container home. They were `-v host:dest` and are now `--mount type=virtiofs,source=…,target=…`. Same semantics, validated parsing, and no dependence on `:` not appearing in a path. `run_argv_shape` pins the exact strings.
- **The herdr bridge is deliberately untouched.** ADR-0007 copies the verified Linux `herdr` binary per run and mounts the copy writable *because* nothing could be mounted read-only; with that constraint gone, the shared verified cache could be mounted directly and the per-run copy and its pruning retired. That changes the bridge's threat model and wants a live herdr session to test against, so it is left as a follow-up rather than changed in passing.
- **A floor under the runtime.** Read-only enforcement was verified on 1.2.2; the `,ro` directive itself parses in 1.0.0 and later, so this does not raise the floor on its own. The documented baseline is already 1.2.0, set separately and for an unrelated reason — the image-config env leak fixed by apple/container#2027 (requirements §2.0).
