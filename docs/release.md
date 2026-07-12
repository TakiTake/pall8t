# Release process

For an agent-operational, step-by-step version of this (who does what, agent
vs. user), run `/release` — see [`.claude/skills/release/SKILL.md`](../.claude/skills/release/SKILL.md).

1. Bump the version in `Cargo.toml`, run `cargo check` so `Cargo.lock` picks it
   up, and add a new section to `CHANGELOG.md` (`## [x.y.z] - YYYY-MM-DD`,
   plus its link-reference footer entry).
2. Open a PR with those changes, get it merged.
3. Tag and push (exact commands in the `/release` skill, step 2 — tag pushes
   are gated to the user, not something an agent can run).
4. Pushing the tag triggers [`.github/workflows/release.yml`](../.github/workflows/release.yml),
   which builds the `aarch64-apple-darwin` binary, verifies the tag matches
   `Cargo.toml`, packages `pall8t-vX.Y.Z-aarch64-apple-darwin.tar.gz` (binary +
   `LICENSE` + `README.md`) with a `.sha256` checksum, and publishes a GitHub
   Release with notes pulled from the matching `CHANGELOG.md` section.
5. Update the formula in [TakiTake/homebrew-tap](https://github.com/TakiTake/homebrew-tap)
   with the new `url` and the `sha256` from the published `.sha256` file, then
   commit and push the tap. (First release only: the tap repo and its initial
   `Formula/pall8t.rb` don't exist yet and need to be created, not just
   updated — until that's done, `brew install TakiTake/tap/pall8t` in the
   README won't resolve.)
