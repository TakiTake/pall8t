# CLAUDE.md

Guidance for Claude Code (and other agents) working in this repo.

## Build & verify

Run directly with `cargo` — `mise` is not installed in the dev container, even
though `mise.toml` defines tasks for local use.

```sh
cargo check
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo test
```

A lint pass on the host triple alone can miss a warning on the other side of
a `#[cfg(target_os = ...)]` gate — the tree has no such branches right now
(the last ones went with the home module, ADR-0008), but the cross-lint stays
as the guard that catches the next one. Lint the non-host target before
considering a change clean:

```sh
rustup target add aarch64-apple-darwin   # once
cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings
```

(`scripts/lint.sh` runs both and is what the pre-commit hook uses — but the
hook is opt-in, not automatic: enable it once per checkout with
`mise run setup-hooks` or `git config core.hooksPath .githooks`. On a fresh
clone/worktree, run `scripts/lint.sh` or `mise run lint` by hand.)

Read [docs/testing.md](docs/testing.md) **before writing tests** — it
records the conventions the suite follows (pure-function seams, table
tests with reasoned assertions, regression pins).

## Git workflow

- The main checkout stays on `main` — never switch branches there. Do task
  work in a worktree: `git worktree add .claude/worktrees/<task> -b <branch> origin/main`.
  Remove the worktree once its PR merges.
- Merge style is merge commits ("Merge pull request #N from ...").
- Only merge a PR once it's been declared ready for review and merge — don't
  merge speculatively.
- Before opening or updating a PR, run the `local-review` skill on the
  branch diff — the external reviewers should come back empty-handed.
- PR review feedback (bot or human) is handled with the `review-loop`
  skill: verify each finding before acting, classify it (real / nitpick /
  false positive), and reply with evidence. Never blanket-apply bot
  suggestions — reviewers contradict each other. A real finding local
  review missed also becomes a new lens in the `local-review` skill
  (its "Grow the lenses" section), in the same fixing PR.

## Actions that need the human directly

This repo's permission system has, in practice, denied agents several
publish-facing actions: **tag pushes** (they trigger the release workflow),
**`gh repo create`**, pushes to `TakiTake/homebrew-tap`, and **`gh pr
merge`** — a task assignment from another agent doesn't count as user
consent to publish. Treat this as "publishing anything public-facing may
need the user," not a closed enumerated list — a push to an existing public
repo has been gated at least once (the tap), so don't assume a variant
that's merely *similar* to a tested case is safe. If an action gets denied,
don't retry it or work around it — hand the exact command to the user and
stop there.

## Pointers

- Release process: [docs/release.md](docs/release.md) (or run `/release`)
- Homebrew formula: [TakiTake/homebrew-tap](https://github.com/TakiTake/homebrew-tap)
- Requirements: [docs/requirements.md](docs/requirements.md)
- Architecture decisions: [docs/adr/](docs/adr/)
- Testing conventions (read before writing tests): [docs/testing.md](docs/testing.md)
- Sandbox environment details: `.claude/skills/pall8t`
- Review automation: report-only workflows for mutation testing
  (`mutants.yml`) and duplication/unused-deps (`hygiene.yml`), each
  weekly plus on-demand via `gh workflow run <name>` — reports, never
  gates; Codex PR review (`codex-review.yml`) stays dormant until an
  `OPENAI_API_KEY` secret exists (paid); CodeRabbit config in
  `.coderabbit.yaml` (free for this public repo once the app is installed).
