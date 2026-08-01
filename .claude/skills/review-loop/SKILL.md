---
name: review-loop
description: Address review feedback on a GitHub PR — fetch bot/human comments, verify every finding against the code before acting, classify it (real / valid nitpick / false positive), fix real ones with a regression pin, refute false ones with evidence, reply per finding, and request re-review. Use when a PR has review comments to address, when asked to "handle the review", or after an automated reviewer (ultrareview, Codex, CodeRabbit) posts findings. Loops until reviewers are satisfied, max 5 iterations.
---

# Review loop

Reviewer findings — especially from bots — are *claims*, not verdicts.
Multiple reviewers contradict each other, and a plausible-sounding finding
can be wrong. The core rule of this loop: **verify first, then classify,
and never apply a finding you haven't reproduced or reasoned through.**

## 1. Gather every finding

```sh
gh pr view <N> --comments                                     # conversation
gh api --paginate repos/{owner}/{repo}/pulls/<N>/comments     # inline review threads
gh api --paginate repos/{owner}/{repo}/pulls/<N>/reviews      # review verdicts
```

`--paginate` matters: without it a busy PR silently truncates to the
first page and findings past it are never addressed. Merge all pages into
one collection before deduplicating.

Collect findings from all sources, including resolved-looking threads with
no reply. Deduplicate across reviewers (bots often overlap).

## 2. Verify each finding — before touching the code

For each finding, decide which it is by *evidence*, not plausibility:

- **Reproduce it**: write the failing test, or trace the concrete
  input/state that triggers the defect. If you can make it fail, it's real.
- **Refute it**: trace the actual code path or the documented semantics of
  the API in question (std docs, upstream source) far enough to show the
  failure cannot happen. "Looks fine to me" is not a refutation.

## 3. Classify and act

| Class | Action |
|---|---|
| **Real defect** | Fix it, and add a regression test that pins the exact failure scenario the reviewer described. The fix without the pin is half the job. |
| **Valid nitpick** | Apply if cheap; otherwise explain in the reply why it's deferred. |
| **False positive** | Do NOT change behavior to appease it. But treat it as a readability smell: if correct code drew the finding, the code argued badly for itself — clarify the comment/structure at that spot and, when practical, pin the questioned behavior with a test so the next reviewer (human or bot) doesn't trip on it. |

Never blanket-apply a bot's suggestions: reviewers disagree, and applying
both sides of a contradiction produces churn. The classification decision
is the review — record it.

## 4. Gate, commit, push

Run the full local gate before pushing (`scripts/lint.sh` and
`cargo test` in this repo). Commit messages state the classification:
what was real and fixed, what was refuted and why the code changed anyway
(clarity, pinning) or didn't.

Pushing fix commits to the PR's own feature branch is ordinary workflow —
it's what keeps the loop moving. But the repo's publish gates still
apply: anything CLAUDE.md reserves for the human (tag pushes, merging,
other repos) or that the permission system denies is handed to the human
as an exact command, never retried or worked around.

## 5. Reply per finding, then request re-review

Reply on the PR (thread reply for inline comments, PR comment otherwise)
with the verdict and the evidence: for a fix, name the commit and the
pinning test; for a refutation, show the trace ("this `into_inner` unwraps
the `Take`, not the `BufReader`") — enough that the thread can be resolved
without re-deriving your work. Then request re-review from whoever raised
it — each reviewer has its own trigger: CodeRabbit reviews pushed commits
automatically (or on `@coderabbitai review`), the Codex workflow runs on
`synchronize` (every push) when its API key is configured, and humans get
a normal re-review request. Confirm the re-review actually started; a
reply nobody re-reads closes nothing.

## 6. Loop, bounded

Repeat from step 1 when new feedback lands. Stop and hand back to the
human when:

- 5 iterations pass without convergence,
- two reviewers directly contradict each other (present both sides and a
  recommendation instead of picking silently), or
- a finding requires a scope/design decision that isn't yours to make.

Never merge the PR yourself — in this repo, merging is always the human's
action, regardless of how green everything is.
