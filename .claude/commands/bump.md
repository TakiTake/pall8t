---
description: Bump pall8t's version — Cargo.toml, Cargo.lock, and the CHANGELOG section — and open the PR. The mechanical half of a release; /release does the whole thing.
argument-hint: <version | major | minor | patch>
---

Bump pall8t's version to `$1` and open a PR with the result.

This is the *mechanical* half of cutting a release: version, lockfile,
changelog, gates, PR. It stops there — tagging, the GitHub Release, and
the Homebrew tap belong to `/release`, which calls this as its first step.
Run this when you want the bump prepared without committing to publishing
yet, or when `/release` sends you here.

## 1. Resolve the version

`$1` is either an explicit version (`0.4.0`) or a bump level (`major`,
`minor`, `patch`) to apply to the current `Cargo.toml` version. If it is
empty, ask — do not guess.

pall8t is pre-1.0, so **semver's usual rules are shifted one place left**:
in `0.y.z`, the `y` carries breaking changes and `z` carries everything
else. A removed or renamed config key, a changed default, a removed
command, or a changed exit-code contract is a **minor** bump (`0.3.0` →
`0.4.0`), not a patch. If the change is breaking and the caller asked for
`patch`, say so and stop rather than shipping a number that lies.

Refuse, with the reason, if:

- the version is not `MAJOR.MINOR.PATCH` — Cargo requires semver, so
  anything else fails at manifest parse, and `release.yml` compares the
  pushed tag against `cargo pkgid`'s version,
- it is not strictly greater than the current version, or
- a `v$VERSION` tag already exists (`git tag -l "v$VERSION"`, and
  `gh release view v$VERSION` for one published without a local tag).

## 2. Preflight

- `main` is clean and up to date with `origin/main`.
- CI on `main` is green: `gh run list --branch main --limit 1`.
- `CHANGELOG.md` has an `## [Unreleased]` section with **content under it**.
  An empty one means there is nothing to release; stop and say so.

Work in a worktree, never on `main`:
`git worktree add .claude/worktrees/bump-$VERSION -b chore/bump-$VERSION origin/main`.

## 3. Make the change

**`Cargo.toml`** — the `version` field only. Then `cargo check`, so
`Cargo.lock` records the new version too; a lockfile left behind shows up
as a stray diff in the next unrelated PR.

**`CHANGELOG.md`** — rename the existing `## [Unreleased]` heading to
`## [$VERSION] - YYYY-MM-DD` using **today's real date** (`date +%F`; never
a guessed one), and open a fresh, empty `## [Unreleased]` above it.

That heading format is load-bearing, not style. `release.yml` extracts the
release notes with:

```awk
/^## \[/ { if (found) exit; if (index($0, "## [$VERSION] -") == 1) { found=1; next } }
```

It matches from the start of the line and expects the ` - ` separator, so
`## [0.4.0]` or `##  [0.4.0] -` silently yields empty notes and fails the
release job *after* the tag is public. Changing the format here means
changing the workflow too.

Then the link footer at the bottom:

- repoint `[Unreleased]` at the new version:
  `[Unreleased]: https://github.com/TakiTake/pall8t/compare/v$VERSION...HEAD`
- add, above the previous entry:
  `[$VERSION]: https://github.com/TakiTake/pall8t/releases/tag/v$VERSION`

Review the section you just renamed while you are here. It accumulated
entry by entry across many PRs, so it can contradict itself — a `Fixed`
note describing something a later `Removed` entry deleted, two entries for
one change, or wording that made sense mid-branch and not at release. Fix
what is wrong; do not invent entries for changes nobody wrote down.

## 4. Prove the release job will find the notes

Do not hand this to a tag and hope. Run the workflow's own extraction
against the file:

```sh
# substitute the real version into verline — the shell will not do it for you
awk -v verline="## [$VERSION] -" '
  /^## \[/ { if (found) exit; if (index($0, verline) == 1) { found=1; next } }
  found { print }
' CHANGELOG.md | sed -e '/^\[.*\]: http/d' -e '/./,$!d'
```

Empty output means the release job would fail on a tag that is already
public and unpushable-over. Non-empty output is the release notes users
will actually see — read them once as a stranger would.

Also confirm the two version sources agree, which is what `release.yml`
checks after the fact:

```sh
cargo pkgid | sed 's/.*[@#]//'   # must equal $VERSION
```

## 5. Gates and PR

Run the quality gates from `CLAUDE.md` — `scripts/lint.sh` (both targets)
and `cargo test` — and **check their exit status**, not just that output
scrolled past. Then run the `local-review` skill on the diff.

Commit, push, open a PR titled `chore: release $VERSION`, and report the
number. **Do not merge it** — merges are the human's, and so is everything
after: `/release` picks up from the merged commit for the tag, the
release, and the tap.
