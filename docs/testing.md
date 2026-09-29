# Testing in pall8t

Read this before writing tests. It records the conventions the existing
suite follows — new tests should look like they belong.

## Design for testability first

The test strategy starts in the production code, not the test file:

- **Rules, parsing, and validation live in pure functions.** Anything with
  a decision in it — policy classification, reference matching, config
  merging — is a standalone function of its inputs. IO happens in a thin
  caller. If you can't test a behavior without spawning a process, first
  ask whether the decision can be extracted (`relay::classify`,
  `container::ref_matches`, `config::merge`).
- **Dependencies are arguments.** Probes with real IO (a socket connect, a
  `--version` spawn) are computed by the caller and passed in, so the
  logic under test stays pure and parallel-test-safe
  (`herdr::doctor_checks(&snap, socket_reachable, bin_resolvable)`,
  `stale_sockets(candidates, grace, is_live)`,
  `reap_stale_sockets(dir, grace, is_live)`).
  Never mutate `std::env` in a test — the suite runs in parallel.
- **A socket this process closed is not necessarily dead, and a half-close
  it performed is not necessarily seen.** macOS has no atomic
  close-on-exec for socket creation, so a subprocess another test spawns
  in that instant inherits the socket and holds it open for its own
  lifetime. Two tests here assumed otherwise and failed roughly once in
  every 300-900 full-suite runs — a bound-then-dropped socket that kept
  answering `connect`, and an upstream half-close that never reached its
  peer. Assert what the code under test decides, and inject the probe;
  the connect itself belongs in the one place that interprets it
  (`connect_says_dead`). The same goes for any resource the suite assumes
  is private while `Command::spawn` runs in parallel.
- **External CLI output is parsed by a pure `parse_*` function**, tested
  against *literal captured output* of the real tool, including a comment
  saying which version it was captured from (`parse_list_all`,
  `parse_exec_wrapper`). When the tool's schema is unstable, that's where
  the defensive fallbacks live — and each fallback gets its own test.

## What a test looks like here

- **Table tests with reasoned assertions.** The assertion message states
  *why* the expectation holds, not what failed — it's documentation that
  executes (`"501-2 must not match 501-20"`, `"the command wins over an
  ambient HERDR_AGENT"`). A test whose name and messages can't teach the
  invariant isn't finished.
- **Pin regressions and refutations.** A bug fix ships with a test pinning
  the exact failure scenario. A review finding that turned out to be a
  false positive also gets a pin when practical — correct code that drew a
  finding argued badly for itself
  (`relay_forwards_bytes_prefetched_past_the_first_line`).
- **Contracts get tests, not just behavior.** Exit-code mappings, argv
  shapes handed to `container`, JSON wire shapes — anything another
  process depends on is asserted structurally (`run_argv_shape`,
  `deny_response_is_herdr_shaped`).
- **Temp state is self-contained**: per-test directories keyed by test
  name + pid, cleaned at the end; `/tmp`-short paths when `sun_path`
  limits apply.

## Two layers: in-crate tests and `tests/cli.rs`

Most of the suite lives next to the code it covers, in `#[cfg(test)] mod
tests`. One integration target, `tests/cli.rs`, drives the built `pall8t`
binary instead. It exists for the things a unit test structurally cannot
see: `main`'s exit codes, clap's shape, which stream each message goes to,
and the command lines pall8t hands `container` and `herdr`.

It is still bound by the rule above — no live runtime, no live herdr:

- **An isolated `$HOME` per test.** `dirs::home_dir()` reads `$HOME` first
  and only falls back to `getpwuid` when it is unset or empty (verified in
  dirs-sys 0.4.1), so every child gets an explicit `HOME` under `/tmp` and
  `~/.pall8t` becomes a throwaway tree. Never `env_clear()` without setting
  it — that sends `~/.pall8t` back to the developer's real home.
- **An empty `PATH`**, so "the `container` CLI is missing" is a fact of the
  test rather than a property of the developer's machine.
- **A stand-in `container` on that `PATH` when a test needs one.** It
  replays literal captured output for the three read-only queries pall8t
  parses and records every argv it is handed. It does not emulate
  apple/container; it is there so the *command lines* — the real contract
  — can be asserted without a VM.
- **`execve` ends coverage.** `pall8t run` and `pall8t exec` replace the
  process, and no atexit handler runs, so a profile written at exit is
  lost. The launch tests therefore make the stand-in runtime remove itself
  at the last call before the exec, which turns the process replacement
  into an ordinary error return. That is also a real behaviour worth
  pinning: a runtime that disappears mid-launch must fail loudly.
- **Nothing may hang.** `herdr relay` serves until its parent exits, so the
  test for its refusal-to-run guard waits with a deadline
  (`Sandbox::run_bounded`) — if the guard ever stopped firing, the suite
  must fail, not block. The same applies to every blocking read: a
  `read_line` on a Unix socket waits forever by default, so the relay's
  own unit tests read replies through `read_reply`, which sets a socket
  deadline and prints the relay's audit log when it fires. The cost of
  getting this wrong is not one slow test: `cargo mutants` derives its
  per-mutant timeout from a baseline run that has *no* timeout itself, so
  a single stalled read there wedges the whole mutation run — no report,
  no output, until someone notices.

## Properties with a reference model

Table tests pin the scenarios someone thought of. Where a function keeps a
promise over *sequences* — a state machine, an encoder and its decoder — a
property test checks the promise over sequences nobody wrote down, with
[proptest](https://docs.rs/proptest) (a dev-dependency only). The first one
is `tab_numbers::properties`, which drives `allocate` with random runs of
allocations, server restarts, unreadable sockets and more sessions than the
state keeps.

- **Check against a model, not against the code's own bookkeeping.** The
  model is what an outside observer could know, kept in a different shape
  from the implementation — `allocate` keeps a running counter, its model
  keeps the *set* of numbers a server run has seen — so the two agree only
  if the counter is right. A model that copies a decision from the state
  instead of making it (an early draft accepted whatever sessions `evict`
  kept) passes against broken code; run the step below to find out.
- **Borrow an oracle only for what has its own tests,** and say so in the
  module doc (`number_in_label` there). The property is then about what the
  code does with the answer, and cargo-mutants will report the borrowed
  function as uncaught by the property — that is expected, not a gap.
- **Generators keep the boundaries in.** Empty strings, a lone `-`, a
  number 0, a name long enough to be capped, more sessions than the bound.
  A generator tuned to produce only "realistic" input is how a guard-clause
  bug survives a property test.
- **Pin the case count** (`ProptestConfig { cases: 256, .. }`) with a
  comment saying why: every mutant the PR gate tries runs the whole suite
  again, so cases multiply its wall time. `PROPTEST_CASES=10000 cargo test
  <module>` still overrides it for a deliberate soak.
- **Assertion messages say why**, the same as table tests.
- **Prove it goes red.** Run the property alone as the suite —
  `cargo mutants -f src/<file>.rs -- --lib <module>::properties` — and read
  what it misses; then break the code by hand in ways cargo-mutants does not
  generate (delete a statement, drop a combinator) and watch it fail.
- **Commit `proptest-regressions/` for a real failure you fixed**, since it
  is the pinned regression. Delete the file a deliberate break wrote.

## Coverage

`cargo llvm-cov --summary-only` (install once with `cargo install
cargo-llvm-cov` and `rustup component add llvm-tools-preview`). Read it as
"what has no test pointing at it at all" — a line covered by a test that
asserts nothing counts for nothing, so the number is a floor to stay above
rather than a score to raise.

CI enforces that floor at 90% lines (`quality.yml`), well under where the
tree actually sits. That is deliberate: the gap is headroom, not slack to
be filled. Closing the last few points means writing tests for the IO
boundaries the harness deliberately does not cross, and those tests can
only assert nothing. If the floor ever blocks you, the question is which
real decision lost its test, not how to get the number back up.

## Would the test go red?

A test only counts if it fails when the code it names is broken. Mutation
testing automates the check — it flips conditions and deletes guards, then
reports the mutants the suite failed to catch. It runs at two scopes, and
the difference between them is the point:

- **Per PR, and it blocks** (`.github/workflows/quality.yml`):
  `cargo mutants --in-diff` mutates only the lines the PR touched. A
  missed mutant there is a test that would not have noticed this change
  breaking, so it fails the build. It is affordable because it is
  incremental — a handful of mutants in well under a minute for a normal
  PR — and it scales with the diff: a PR adding a thousand lines of
  source gets proportionally more mutants to survive.

  Be exact about which lines those are. cargo-mutants mutates product
  code and never `#[cfg(test)]` code, so the count follows the *source*
  a PR adds, not the tests. A PR that only adds tests produces zero
  mutants and passes, the same as a docs-only PR. What the gate
  enforces is the other direction — new code must arrive with tests
  that would notice it breaking — and that is what would have caught
  the `nofile` assertion this document opens with. A pile of tests
  defending nothing new is not something this gate can see; the
  suite-cost numbers in the same workflow are where it shows up.
- **Weekly over the whole tree, report-only** (`mutants.yml`): the trend,
  including the standing misses nobody chose. Gating on that would be
  gating on a backlog, which teaches reflexive ignoring. Run it on demand
  with `gh workflow run mutants.yml`.

Most of the standing whole-tree misses are IO-boundary functions the
harness cannot reach (`host_ids`, `system_status`, `stdin_is_tty`). They
are not a to-do list; driving that count to zero would mean exactly the
assertion-free tests this document warns about.

### When the per-PR gate is red

Read the counts in the step summary before assuming a test is missing.
The job fails on any non-zero exit from `cargo mutants`, which covers
three different situations:

- `missed` non-zero — the real one. A change to your code that the suite
  did not notice. Fix the test, not the gate.
- `timeout` non-zero — a mutant made some test hang, usually a loop whose
  exit condition was what got mutated. Worth a look: a mutant that hangs
  the suite is still a mutant nothing asserts against. If it is instead a
  genuinely slow test, `--timeout-multiplier` on the step is the honest
  fix.
- The run never finished. The job is capped at 45 minutes. The weekly
  whole-tree run is the yardstick: 645 mutants in 48m35s on 2026-09-18,
  so the cap is worth about 500-600 mutants and a refactor touching a
  third of the tree can pass it. Split the PR — which is the right move
  anyway, since nobody reviews a diff that size well — or, if it truly
  cannot be split, say so in the PR and let the human run
  `cargo mutants --in-diff` locally to produce the same evidence the job
  would have.

For a quick local pass on one file: `cargo mutants -f src/<file>.rs`. To
see what the PR gate will see, without waiting for CI:
`git diff origin/main...HEAD > /tmp/pr.diff && cargo mutants --in-diff /tmp/pr.diff`
(`--list` added to that prints the mutants without running them, which is
the cheap way to find out whether a diff mutates anything at all).

When you write a nontrivial test, do the manual version once: break the
code, watch the test fail, restore it. If it stays green, the test is
asserting the wrong thing.

## What not to test

- Don't re-test the standard library or clippy-enforced properties.
- Don't write end-to-end tests that need a live `container`/herdr — those
  are exercised manually per change (and documented in the PR); the unit
  layer covers the logic via the pure-function seams above. When a test
  would need real IO to prove anything, prefer restructuring the code
  over building a mock universe (see `util.rs`'s note on why
  `run_streaming`'s fd redirection is deliberately left unverified
  in-process).
