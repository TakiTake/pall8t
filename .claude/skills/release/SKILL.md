---
name: release
description: Cut and publish a new pall8t release end to end — version bump, changelog, tag, GitHub Release verification, and Homebrew tap update. Use when the user asks to cut, ship, or publish a release, or wants the next version released. Takes the new version (e.g. 0.2.0) as an argument.
---

# Cutting a pall8t release

Full narrative in [docs/release.md](../../../docs/release.md); this is the
operational, step-by-step version with who does what. Steps alternate
between the agent and the user — several publish-only actions are gated to
the user by the permission system (see the repo's root `CLAUDE.md`). Don't
retry a denial or work around it — hand the exact command to the user
instead.

## 1. (agent) Preflight and release-prep PR

- Confirm `main` is clean and CI is green (`gh run list --branch main --limit 1`).
- Decide the version (`$VERSION`, e.g. `0.2.0`) — from this skill's argument
  if given, otherwise ask.
- In a worktree (`.claude/worktrees/release-$VERSION`, branch
  `chore/release-$VERSION`):
  - Bump `Cargo.toml`'s `version`, run `cargo check` so `Cargo.lock` picks it up.
  - Add a `CHANGELOG.md` section — `## [$VERSION] - YYYY-MM-DD` summarizing
    what actually shipped since the last release (don't fabricate history),
    plus its link-reference footer entry
    (`[$VERSION]: .../releases/tag/v$VERSION`). The `## [x.y.z] - ` heading
    format is not just style — `release.yml` parses it verbatim to extract
    release notes; changing the format here means updating the workflow too.
  - Run the quality gates from `CLAUDE.md` (fmt/clippy/test, both targets).
  - Run the review loop (`/code-review`, `/skeptical-review`); fix findings;
    repeat until both come back clean.
  - Push, open a PR, report the PR number. **Do not merge it.**

## 2. (user) Merge and tag

- The user merges the PR themselves — PR merges are gated for agents.
- **(agent) Before handing over the tag command**, pull latest `main` and
  confirm `Cargo.toml`'s `version` and the `CHANGELOG.md` heading both read
  exactly `$VERSION` — `release.yml` only checks the tag against
  `Cargo.toml` *after* the tag is already public, so catching a mismatch
  here avoids pushing a tag that's guaranteed to fail downstream (see
  "If the release workflow fails" below).
- Then hand over the tag command:
  ```sh
  ! git checkout main && git pull
  ! git tag v$VERSION && git push origin v$VERSION
  ```
  Tag pushes are gated too (they trigger the public release workflow) — hand
  this exact command over rather than attempting it.

## 3. (agent) Watch the release workflow and verify the artifact

- `gh run watch <run-id>` (or poll `gh run list --workflow=release.yml --limit 1`)
  until `.github/workflows/release.yml` finishes.
- `gh release view v$VERSION` — confirm both assets exist:
  `pall8t-v$VERSION-aarch64-apple-darwin.tar.gz` and its `.sha256`.
- Download both into the scratchpad and verify:
  - `sha256sum -c` the tarball against its `.sha256` file — this is the
    authoritative check. `gh release view --json assets` may also report a
    `digest` field to cross-check against, but treat that as a bonus, not a
    required step: it's absent/null on some `gh` versions.
  - `tar -tzf` shows a single top-level dir
    `pall8t-v$VERSION-aarch64-apple-darwin/` containing `pall8t` + `LICENSE`
    + `README.md` — that single-dir layout is what lets Homebrew `cd` into it.
  - The binary's magic bytes are Mach-O ARM64:
    `od -A x -t x1z -v pall8t | head -1` should start `cf fa ed fe` with
    cputype `0c 00 00 01` (`0x0100000c` = ARM64).

**If the release workflow fails** (step 3 never finds a Release, or `gh run
watch` reports a failed run): stop — do not proceed to step 4. The tag is
already public at this point; deleting and reusing it requires
`git push --delete origin v$VERSION` (gated, hand it to the user), fixing
whatever failed on `main` in a new PR, then retagging. Report the failure
and the fix needed instead of guessing.

## 4. (agent, then possibly user) Update the Homebrew tap

Only proceed here once step 3 has confirmed a real Release with both assets
verified — don't run this step against a release that failed or doesn't
exist yet.

- Clone `https://github.com/TakiTake/homebrew-tap.git` into the scratchpad.
- Update `Formula/pall8t.rb`: `url` to the new release tarball, `sha256` to
  the verified checksum from step 3.
- Commit locally — `git add` and `git commit` in one call, with no `push` in
  the same call. A denied compound command rolls back the whole thing,
  including the local-only parts, so keep the push isolated.
- Attempt `git push origin main`. Pushes to `homebrew-tap` are gated — don't
  retry on denial, report the exact push command for the user to run from
  the scratchpad clone instead.
- Once pushed, verify the live formula: fetch
  `https://raw.githubusercontent.com/TakiTake/homebrew-tap/main/Formula/pall8t.rb`
  and confirm it's byte-identical to what was committed, with the right
  `url`/`sha256`.

## 5. (user, optional) Smoke test

```sh
! brew upgrade TakiTake/tap/pall8t || brew install TakiTake/tap/pall8t
! pall8t --version
```

This can't be run from a Linux dev container — it's the one step only the
user can actually execute end to end.
