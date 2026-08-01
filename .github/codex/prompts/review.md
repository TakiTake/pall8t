Review ONLY the changes introduced by this pull request. The checkout is
the PR merge ref, so its parents delimit the PR exactly regardless of the
base branch: find the diff with `git log --oneline HEAD^1..HEAD^2` and
`git diff HEAD^1...HEAD^2`.

Context: this is pall8t, a Rust CLI that runs AI coding agents inside
apple/container sandboxes on macOS. Its conventions: clippy pedantic is
enforced with `-D warnings`; code comments state constraints the code
can't express; logic lives in pure functions with dependencies passed as
arguments, covered by table tests; parsing of external CLI output is
factored into pure `parse_*` functions tested against literal captured
output. Architecture decisions live in docs/adr/ — flag changes that
contradict an accepted ADR.

Report ONLY actionable findings, most severe first. For each:

- `file:line`, one-sentence claim, and the concrete failure scenario
  (inputs/state → wrong outcome). A finding you cannot attach a failure
  scenario to is not a finding — leave it out.
- Distinguish severity: correctness/security defects first, then
  robustness gaps, then test-coverage gaps for the changed code. Skip
  style — the linter owns style.
- Check the tests too: does a new test actually pin the behavior it names?
  Would it stay green if the code under test were broken?

If nothing meets the bar, say exactly that in one line. Do not pad,
summarize the PR, or restate the diff.
