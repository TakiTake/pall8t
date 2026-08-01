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
  (`herdr::doctor_checks(&snap, socket_reachable, bin_resolvable)`).
  Never mutate `std::env` in a test — the suite runs in parallel.
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
