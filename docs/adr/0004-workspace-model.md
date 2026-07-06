# ADR-0004: Multi-repo project workspaces with identity-path mounts

- Status: Amended by ADR-0006 (project-workspace model dropped; hardlink-clone protection retained)
- Date: 2026-07-04

## Context

The real workflow spans repositories: one platform repo plus per-service repos, with development projects that split PRs and cross repo boundaries. Tasks run in parallel, each in its own git worktree. Requirements:

1. A pall8t *project* references **multiple repos**.
2. Canonical checkouts (each repo's main) must be **protected from agents** — ideally mounted read-only.
3. Agents freely create **git worktrees**, which must **survive container restarts** (and deletion/recreation).
4. Paths must be valid **both on the host and in the container**, so git metadata (worktrees record absolute paths) and host-side IDEs keep working.

Two hard constraints discovered:

- apple/container has **no read-only mount support yet** ([apple/container#990](https://github.com/apple/container/issues/990), open feature request).
- Even with RO mounts, `git worktree add` must write `.git/worktrees/<name>` in the *parent* repo — impossible on an RO mount and undesirable on the canonical checkout.

## Decision

Each project gets a **host-side workspace directory** at a unique path, mounted into the container **at the identical absolute path** (identity-path mount):

```
<workspace_root>/<project-slug>-<hash8>/     default root: ~/.pall8t/workspaces
  repos/<repo>/    seeded clone per source repo — the worktree parent
  wt/              worktrees created by agents
```

- **The canonical repos are not mounted at all** (for now). Protection by absence — stronger than read-only.
- **Seeding:** at project creation pall8t runs, host-side, `git clone <repo> <ws>/repos/<name>`. A same-filesystem clone hardlinks objects (fast, near-zero extra space). The origin remote URL is copied from the source repo so fetch/push work inside the container (creds live in the persistent agent home).
- **Worktrees:** agents run `git worktree add` against the workspace clones; parents and worktrees both live in the workspace, so everything persists and works from either side of the mount.
- **Identity paths** keep `.git` absolute-path metadata valid host-side and container-side, and let a host IDE open exactly the paths agents report.
- Container naming/mounting moves from "project = one dir" to "project = workspace": `pall8t-<slug(name)>-<sha256(ws path)[..8]>`, mounts = workspace (rw) + `~/.pall8t/home → /home/dev`.

## Alternatives considered

- **Mount canonical repos read-write** — rejected: one stray `git checkout`/`rm` by an agent damages the real checkout.
- **Worktrees directly from the canonical repo** — rejected: writes to the canonical `.git`, and worktree links would dangle when the container is gone if paths differed.
- **Clone with `--reference` to the canonical repo** — deferred: alternates point at the canonical path, which would have to be mounted (RO) to be readable in the container. Adopt once #990 ships; until then hardlink clones give the same space win without the mount.

## Consequences

- Workspace clones drift from canonical until fetched; agents fetch from origin (network + creds in container), or the user refreshes host-side. A `prefix r`-style "sync repos" action may come later.
- Disk cost is bounded by hardlinking; deleting a project offers to delete its workspace.
- When #990 lands: additionally mount each canonical repo RO at its identity path and switch seeding to `--reference` alternates (zero duplication, agents can read canonical main directly).

## Revisit triggers

- apple/container#990 resolved → RO identity-path mounts of canonical repos.
- Workspace sync pain → built-in fetch/refresh command.
