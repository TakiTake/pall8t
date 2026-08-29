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

## Coverage

`cargo llvm-cov --summary-only` (install once with `cargo install
cargo-llvm-cov` and `rustup component add llvm-tools-preview`). It is a
guide, not a gate: the number is high because the seams above make the
decisions reachable, and a line covered by a test that asserts nothing
counts for nothing. Read it as "what has no test pointing at it at all".

## Would the test go red?

A test only counts if it fails when the code it names is broken. The
mutation-testing workflow (`.github/workflows/mutants.yml`) automates
this check — it flips conditions and deletes guards, then reports mutants
the suite failed to catch. It runs weekly and on demand (Actions →
"Mutation testing" → *Run workflow*, or
`gh workflow run mutants.yml`); either way it is report-only and never
blocks a build. For a quick local pass on one file:
`cargo mutants -f src/<file>.rs`. When you write a
nontrivial test, do the manual version once: break the code, watch the
test fail, restore it. If it stays green, the test is asserting the wrong
thing.

## What not to test

- Don't re-test the standard library or clippy-enforced properties.
- Don't write end-to-end tests that need a live `container`/herdr — those
  are exercised manually per change (and documented in the PR); the unit
  layer covers the logic via the pure-function seams above. When a test
  would need real IO to prove anything, prefer restructuring the code
  over building a mock universe (see `util.rs`'s note on why
  `run_streaming`'s fd redirection is deliberately left unverified
  in-process).
