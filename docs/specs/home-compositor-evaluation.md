# Evaluation: home-compositor candidates vs. the spec

Status: 2026-07-07. Companion to [home-compositor.md](home-compositor.md); executes its
"next step". Three evaluations, one comparison table each: (A) off-the-shelf tools
against the spec's requirements, (B) fork mechanics for FR-1, (C) versioning/merge
engine for FR-4/FR-7 — plus (D) the ideas worth borrowing from tools we don't adopt.

## A. Off-the-shelf tools vs. spec requirements

Columns are the spec's discriminating requirements: CoW fork (FR-1), path-class policy
(FR-2), lossless harvest (FR-3), per-path promote (FR-4), lazy harvest decoupled from
process lifetime (FR-8), and the secrets NFR (zero network; secrets never in
logs/diffs/object stores).

| Tool | CoW fork | Path classes | Lossless harvest | Per-path promote | Lazy harvest | Secrets NFR | New dep | Verdict |
|---|---|---|---|---|---|---|---|---|
| [jj (jujutsu)](https://github.com/jj-vcs/jj) | ✗ (VCS checkout, full copy) | ✗ (ignore rules only) | ◐ working-copy-as-commit auto-snapshots | ◐ manual (`squash -i`) | ◐ snapshots on next jj command | ✗ secrets enter object store unless ignored | jj binary | **No** — closest in spirit; borrow ideas (D) |
| [chezmoi](https://www.chezmoi.io) | ✗ | ◐ (templates/ignore, one-way) | ✗ apply is source→home; re-add is manual | ◐ `re-add` per file | ✗ | ◐ delegates secrets to password managers | chezmoi binary | **No** — one-way dotfile model, not divergent-home merge |
| [Nix home-manager](https://github.com/nix-community/home-manager) | ✗ | ✗ | ✗ **mutations to `$HOME` are outside the model entirely** | ✗ | ✗ | ◐ | Nix toolchain | **No** — declarative build of home is the opposite of harvest; generations idea noted (D) |
| overlayfs-in-guest (base ro lower + rw upper) | ✓ fork is free | ✗ (mount-level only) | ◐ upper dir *is* the change set | ✗ | ◐ upper persists if host-visible | ✓ local | none (guest kernel) | **No as baseline** — upper-on-virtiofs needs Linux ≥5.7 *and* xattr passthrough (`trusted.overlay.*`/`userxattr`); reports show "upper does not support xattr" degradation, and apple/container documents no xattr guarantees. Upper-on-tmpfs loses the changeset at `--rm` exit. Revisit if apple/container pins this down |
| [bit-vcs/bit](https://github.com/bit-vcs/bit) | ✗ | ✗ | ◐ | ◐ | ✗ | ✗ **AI merge via OpenRouter** | MoonBit binary | **No** — [evaluated on issue #9](https://github.com/TakiTake/pall8t/issues/9#issuecomment-4904121268) |
| custom pall8t module (`src/home.rs`) | ✓ clonefile | ✓ by construction | ✓ | ✓ | ✓ | ✓ | none | **Yes** — the spec's expectation holds |

Confirmed expectation: nothing off-the-shelf combines class-aware staging (FR-2/FR-3/FR-4)
with lazy harvest (FR-8); every VCS-shaped candidate also puts secrets at risk of entering
an object store. The module builds on parts pall8t already has (git subprocess helper,
config merge, `libc`).

## B. Fork mechanics (FR-1)

Home today is ~22 MB; spec targets <100 ms at 1 GB. "Harvestable" = the instance's
changes remain host-visible and diffable after the run dies.

| Mechanism | Speed @1 GB | Disk cost | Crash-safety | Harvestable | Deps | Verdict |
|---|---|---|---|---|---|---|
| `clonefile(2)` | ~ms (CoW metadata) | shared blocks until written | clone to temp name + rename (repos.rs discipline) | ✓ plain dir on host | `libc` (already a dep; [bindings exist for Apple targets](https://github.com/rust-lang/libc)) | **Primary** |
| Recursive copy (std `fs`) | seconds, O(bytes) | full duplicate | same temp+rename | ✓ | none | **Fallback** for non-APFS homes |
| rsync | seconds | full duplicate | partial-transfer flags needed | ✓ | macOS's aging bundled rsync | No — adds nothing over std copy here |
| git clone/checkout of base | seconds + object store | duplicate + `.git` | git's own | ✓ | git | No — requires the base to be a repo (see C) |
| overlayfs upper (guest) | instant | delta only | n/a | ✗/◐ per table A | guest privileges + xattr passthrough | No as baseline |

`clonefile` notes: fails across volumes and on non-APFS — detect and fall back to copy;
directory hierarchies clone in one call; fork runs under the FR-6 base lock so the source
is quiescent.

## C. Versioning & merge engine (FR-4 merges, FR-7 log/diff/rollback)

Key structural insight: `git merge-file` gives a **stateless three-way text merge** — no
repository needed. And embedding a git repo *in the base* is actively harmful: the base is
bind-mounted as the agent's `$HOME`, so a `.git` there would make every container home
look like a repo to the agent (and to Claude Code). Structural JSON merge (`state` class)
is custom work under every option, so it doesn't discriminate.

| Engine | 3-way text merge | log/diff/rollback | Secrets exclusion | `kill -9` safety | Agent-visible artifacts in `$HOME` | Effort | Verdict |
|---|---|---|---|---|---|---|---|
| git repo in the base | ✓ native | ✓ native | ◐ ignore rules, one mistake → secret in object store forever | ✓ | ✗ **`.git` in every container home** | low | No |
| git repo *beside* the base (`--git-dir` external, base as worktree) | ✓ | ✓ | ◐ same object-store risk | ✓ | ◐ none visible, but base must stay index-consistent | medium | No — fragile coupling |
| clonefile snapshots + manifest, `git merge-file` for text | ✓ via stateless `git merge-file` | ✓ snapshot per revision, diff = tree walk, rollback = clone back | ✓ **policy-aware by construction** (secret/ephemeral paths simply excluded from snapshots/diffs) | ✓ temp+rename | ✓ none | medium | **Primary** |
| jj as engine | ✓ | ✓ | ✗ object store | ✓ | ✗ workspace metadata | low-medium | No |
| hand-rolled content store (hash/dedup) | custom | custom | ✓ | custom | ✓ | high | No — over-engineering at 22 MB–1 GB scale |

Verdict: **snapshots via `clonefile` + a manifest for FR-7; `git merge-file` (git is
already a dependency) for FR-4 text merges; custom key-path JSON merge for `state`.** No
VCS repository anywhere near `$HOME`; the policy manifest is the single source of truth
for what gets snapshotted, diffed, and merged.

## D. Ideas borrowed from non-adopted tools

| Source | Idea | Where it lands in the spec |
|---|---|---|
| jj | working copy snapshotted automatically on next invocation; conflicts are first-class objects, never block | FR-8 lazy harvest; FR-5 conflicts-only-at-promote |
| chezmoi | secrets referenced, never stored in the state engine | `secret` class excluded from snapshots/diffs |
| home-manager | numbered generations with rollback | FR-7 revision model |
| bit | AI-assisted merge — as a *local* resolver only | FR-4 pluggable resolver hook (local Claude) |

## Sources

- [Overlay Filesystem — kernel docs](https://docs.kernel.org/filesystems/overlayfs.html) (upper-layer xattr/whiteout requirements)
- [OverlayFS over virtio-fs upper layer since Linux 5.7 — Phoronix](https://www.phoronix.com/news/OverlayFS-Linux-5.7)
- [kata-dev: virtio-fs as overlayfs upper layer](https://lists.katacontainers.io/archives/list/kata-dev@lists.katacontainers.io/thread/XTN3SQQ3SDKUPCPINIZAA3YGJ7ST6AAN/) (xattr/CAP_SYS_ADMIN limitations in practice)
- [rust-lang/libc: macOS clonefile bindings](https://github.com/rust-lang/libc)
