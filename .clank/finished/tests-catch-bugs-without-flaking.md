# tests-catch-bugs-without-flaking
# The test suite catches real regressions without depending on luck

## Why

`cli::status_tui::zellij::tests::only_a_reopen_request_in_the_runtime_directory_reaches_the_loop` failed PR #31's CI on 2026-11-10 (panic at `crates/cli/src/cli/status_tui/zellij.rs:4562`, 3429 other lib tests passed) and passed on rerun. It has also failed locally under load and is on the known-flaky list.

Lloyd, 2026-11-12: fix the flaky tests and do a general purge of tests that
do not carry useful regression protection. Doing this real work through Clank
also exercises Codex background processes after the recent event-driven changes.

## Scope and approach

Implement this as one complete commit. Before changing tests, inventory the
workspace suites and test scripts by the behavior they protect. Record the
known failures, existing duplicate coverage, expensive setup, and candidates
that only mirror implementation details. This inventory drives the work;
do not discover its scope one review correction at a time.

### Fix the known flakes

- Investigate the reopen-request test above, the receipt-callback diagnostic
  test in turn_custody, and the monitor source-restart test recorded in
  `.clank/drafts/known-flaky-tests.md`.
- Trace the older `remote::tests::off_joins_a_poll_in_flight` report to its
  current equivalent or document that its code and test were removed.
- Include the remote_owner group, especially
  `applied_waits_for_the_transition_and_a_second_caller_is_busy`, which recent
  reviews ran separately because the parallel suite failed under load.
- Identify the causal race, fixture lifetime, environment leak or production
  bug for each. Use barriers, channels, controlled clocks or existing seams
  where appropriate. Keep real filesystem/process coverage where the native
  boundary is the behavior being tested.
- Do not make a test green by increasing sleeps, weakening its assertion,
  ignoring it, retrying until green, or serializing the suite. Fix production
  defects when the investigation establishes one, with a regression test.

### Purge tests that do not protect behavior

- Review every workspace suite and test-script family, prioritizing duplicates,
  literal/source-text assertions, trivial forwarding or getter tests, and tests
  that reproduce the implementation instead of checking an observable contract.
- Delete or consolidate cases with no distinct regression protection. There is
  no deletion quota: a small useful test is worth keeping; an expensive test
  must earn its cost. Do not rewrite retained tests just for stylistic uniformity.
- For each removed family, record what it asserted, why it is redundant or
  irrelevant, and which retained test protects any meaningful behavior. For a
  unique contract, improve its test before removing its only protection.
- Preserve protection for delivery/acknowledgment, session identity, permission
  handling, durable formats, shutdown, native event registration and recovery.
  Architectural source-scanning gates can be useful: assess their invariant
  rather than deleting them just because they inspect source.
- Remove unused fixtures/helpers made obsolete by the purge. Consolidate the
  two existing draft notes into the plan's final audit so stale failures are not
  left as new work.

### Exercise Clank while doing the work

- Run all background builds/tests with `clank run --desc ...`; use the harness's
  completion notification and inspect output and exit status when it ends.
  End the turn while work runs, renew attendance only on a real check-in, and
  do not poll or repeatedly invoke status to manufacture progress.
- Keep logs outside tracked files. Record whether completion, check-ins,
  attendance clearance and the master/reviewer handoff actually worked.
  A missing wake or stuck attendance is a finding, never a passed observation;
  capture the evidence and fix or queue a clearly scoped follow-up.

### Share fixture tooling and remove incidental executables

Lloyd clarified on 2026-11-12: prefer calling Rust directly to compiling and
launching a binary for each test, use gix for Git fixture mutations where it
fits, and do not assume Python is installed, even for testing. This is a
judgment about useful boundaries, not a blanket ban on subprocess coverage.

- Inventory executable dependencies and nested builds alongside the suite
  inventory. Convert tests of library behavior to in-process calls. Reuse Cargo's
  already-built artifacts when the executable boundary itself needs testing;
  do not compile another copy inside each fixture.
- Consolidate Git fixture setup and mutations into shared Rust tooling using
  gix and the existing git_io/test helpers. Keep actual Git invocations only
  where they are the subject or an independent interoperability oracle (such
  as Git invoking a post-rewrite hook); document each retained family.
- Remove Python invocations from Rust tests, their provider/protocol fixtures,
  and the standard test/release scripts they exercise. Use common Rust fixtures
  rather than replacing each Python script with a different ad hoc executable.
  Keep process ownership, stdin/stdout, PTY and shutdown checks at their real
  boundaries when that is the regression they protect.
- Audit standalone book-recording tools separately: state their dependencies
  explicitly and do not require Python for the project's test or release gates.
  Do not claim that the repository contains no Python while an optional authoring
  tool remains. Any retained external prerequisite must be explicit and justified.
- Unit tests must not write into the developer's live runtime namespace. Use
  a private, short-path namespace shared by the test process's threads, with
  cleanup owned by the test runner or fixture. Preserve native socket/lease/watch
  coverage and the production namespace's identity contract.

## Validation and acceptance

- Record a baseline and after-change test count and elapsed suite time under
  comparable conditions; distinguish build time from test time. Speed and test
  count are evidence, not pass criteria.
- Stress each retained repaired flaky test for 200 iterations, with normal
  concurrent execution, no automatic retries of failures and bounded timeouts
  used only to fail a hang. Explain why the synchronization makes it sound;
  repeated green runs alone are not the explanation.
- For repaired race tests and representative consolidated families, introduce
  a targeted temporary fault to confirm the retained assertion catches the
  regression. Never commit the fault.
- Run the full workspace suite with normal parallelism, including remote_owner,
  plus formatting and strict Clippy. Run the applicable Linux CI checks on the
  candidate; label platform or installed-harness checks not executed as not run.
- Validate the normal tests and release checks without Python available; report
  remaining external tools and why each is needed. Keep generated fixtures and
  evidence aligned with the actual remaining test paths.
- The final audit lists each known flake's disposition, removed/consolidated
  families and retained coverage, remaining risks, and observed background-run
  behavior. A failure remains visible until explained and resolved; do not
  clear the known-flaky list merely because an isolated rerun passed.

## Out of scope

Publishing a release, new user features, a new test framework, and broad
production refactors unrelated to established defects. No requirement to run
live paid harness acceptance for this plan.
