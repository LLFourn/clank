# Test cleanup audit

Plan: `tests-catch-bugs-without-flaking`, November 2026.
This records decisions, completed checks and explicit validation limits.

## Review correction: release-tool provisioning

Review of `a570eb6b` found that the release entry point forced the maintenance
example offline, although a production-only Cargo build does not fetch its dev
dependencies. An isolated cache missing `oxc_allocator` reproduced the failure
before any release validation. The earlier full-workspace runs had populated
that dependency and therefore missed this orchestration defect.

The entry point now permits Cargo to provision its tool with locked resolution;
the validator's metadata query remains offline. A dedicated CI smoke check uses
an empty private Cargo home, builds the production binaries once, and invokes
the real release script. It requires the invalid-tag diagnostic, not merely a
nonzero exit, so missing dependencies cannot masquerade as successful rejection.
This intentionally retains one build-level integration boundary outside the
ordinary Rust test suite. The smoke check passed on macOS from an empty private
Cargo home, reaching the intended tag-validation diagnostic after the production
build. All four CI workflow tests, twelve release workflow tests and ShellCheck
for both scripts passed. The new dedicated Ubuntu CI job has not run locally;
actionlint was unavailable locally. No Rust code changed in this correction.

## Inventory and disposition

| Family | Protected behavior | Disposition |
| --- | --- | --- |
| core unit tests and round trips | Fold, turn/attendance policy, IDs, event and durable representations | Retain distinct behavioral cases; no subprocess dependency found in the pure core. |
| user-config | Config merging, concurrent mutation and lease ownership | Retain; filesystem and locking are the contract. |
| web and bridge tests | Authentication, messages, reconnects and protocol compatibility | Retain security and protocol coverage. |
| web-cloudflare | HTTP/WebSocket host and tunnel lifecycle | Retain actual network/ownership boundaries. |
| CLI parsing/rendering/docs | Public command grammar and user-visible output | Replace obsolete-command source scan with parser calls; remove vacuous version-string check and exact README wording assertions. |
| CLI lifecycle and integration | Plan/review state, rewriting, admission, delivery | Shared gix fixture initialization; direct commit/index creation in feedback-read tests. Retained native Git families are listed below. |
| Git interoperability | Native hook invocation, worktrees, index/rewrite compatibility | Keep an independent Git oracle when Git's behavior is the subject. |
| Monitor, recovery, wait, sources | Delivery, custody, registration, cancellation and shutdown | Isolate unit-test runtime files; preserve native positive-event/process tests and repair the identified causal races. |
| Architecture gates | Process, Git, config-write, unsafe and permission boundaries | Retain meaningful boundaries. Do not replace behavioral tests with source scans. |
| Release checks | Tag/package/binary/changelog agreement; Cargo artifact path | Move validation into Rust maintenance tests. Remove repeated lockfile generation and two nested Cargo binary builds. |
| License/notices | Workspace identification, dependency sources, attribution, license bodies | Move renderer into Rust. Run full freshness generation once in CI through a symlinked checkout, rather than also inside ordinary integration tests. |
| Provider/protocol fixtures | Native hooks, ACP, PTY arbitration and launch identity | Shared Cargo-built Rust process fixture; direct Grok state tests. Keep processes for native transport, hooks and PTY arbitration. |
| Browser/OpenCode scripts | Actual JavaScript execution, DOM and plugin lifecycle | Node is an explicit CI dependency; Rust assertions cannot replace execution of the shipped JavaScript. |
| Book recording | Optional terminal recordings and authoring scenarios | Separate authoring toolchain; Python remains here, outside the Rust tests and release scripts. |

## Removed or consolidated protection

- `tui_is_the_command::the_old_name_lives_only_in_the_matcher`: scanned source
  and prose for retired words. Replaced by actual parser acceptance/refusal;
  existing pane matcher tests continue to protect legacy pane recognition.
- `docs::the_version_stamp_is_not_rendered`: never supplied the invented
  `9.9.9-stamp` version it asserted absent. Reference freshness and deterministic
  generation tests remain.
- `no_ngrok::the_readme_retires_the_provider_and_documents_the_migration`:
  pinned exact prose and JSON spelling. Keep SDK/dependency prohibition,
  retired-config handling, and the executable advertised tunnel recipe test.
- `release_check` integration family: most cases validated strings after
  spawning Cargo, a shell and a fake executable; the artifact-path case built
  two dummy Rust projects. The maintenance Rust validators cover the same
  mismatches and artifact selection directly.
- `workspace_licence` Python renderer probe: now direct Rust workspace-ID
  rejection tests, including an outside path dependency with a workspace name.
- Full notice generation through a symlink: retained in the dedicated CI
  freshness gate, removed from ordinary Rust tests to avoid another full
  generator/build pass.

## Observed failures and intermediate validation

- Baseline, 18 logical CPUs, normal test parallelism: build-only phase 25.50s;
  workspace test command 175.18s (includes repeated compilation). CLI library:
  3,550 passed, nine failed, four ignored, 78.48s test time. Integration:
  435 passed, two ignored, 57.35s. Other workspace tests passed.
- All nine baseline failures were in receipt delivery or remote lifecycle
  fixtures. Unit tests shared the developer's runtime directory, which had
  107,848 entries. The native watcher scanned unrelated accumulated files.
  No live files were deleted. A private test-process namespace eliminated those
  nine failures; its initial long path exposed one Unix socket-path failure.
  The namespace now uses a short `/tmp` path. `scripts/test.sh` owns a private
  root, shares it with test-built CLI children and removes it at exit; CI uses
  this runner. Production builds do not honor the test override. Bare Cargo
  unit runs have a private fallback; use the runner for automatic cleanup and
  isolation of integration-test subprocesses too.
- After the short-path correction and first gix migrations: focused feedback
  and parser tests passed; CLI library 3,559 passed, zero failed, four ignored,
  46.10s test time. This is intermediate evidence, not final acceptance.
- Source restart fixture used to publish readiness before writing its first
  launch record. The driver could kill it between those writes and then wait
  forever for the second record. Readiness now follows the launch record.
- Reopen filtering now has direct scan coverage plus a native positive wake
  test, instead of a timeout used as evidence that unrelated events never fire.
- Remote-owner fixture now acknowledges readiness and a starting transition
  over channels. Exit tests wait for the native request notification before
  killing the owner. Remote shutdown tests hold an entered listing with a
  channel instead of guessing when it started or asserting elapsed wall time.
  A direct cancelled-caller test verifies that the blocking-work join remains
  pending until the operation is released.
- Rust maintenance validators passed all five tests. The first complete notice
  comparison found a single accidental punctuation change in the fixed header;
  corrected, and the subsequent complete generation matched byte for byte.
  No license or package differences were reported.
- The subsequent parallel workspace run passed: CLI library 3,560 passed,
  four ignored in 47.96s; integration 425 passed, two ignored in 46.45s;
  maintenance five passed. All other workspace suites passed. This predates
  the native process-fixture migration below and is not final acceptance.
- Launch ordering, receipt producer/callback, and Grok launch acceptance now
  share one Cargo-built Rust fixture instead of Python programs. The real
  exec, environment, hook and wait boundaries remain exercised. Receipt
  producer readiness uses datagrams, avoiding a platform-specific FIFO API
  and the external `mkfifo` utility. Focused validation caught an empty-profile
  fallback discrepancy in the replacement fixture; corrected before rerun.
- All 15 focused launch/receipt/Grok acceptance tests passed with the Rust
  process fixture. Grok fork-state and setup decisions now use direct Rust
  collaborators, while dedicated integration tests retain the real ACP
  stderr/identity/protocol/output-cap/deadline/reaping boundary. The unit
  Python provider scripts have been removed. The split passed 34 focused unit
  tests (two installed-Grok tests remain opt-in) and 11 matching integration
  tests, including the native process cases.
- The Rust Claude question fixture compiled and ten broker unit tests passed.
  Installed Claude 2.1.308 exposed an assumption in the old fixture: it now
  records an `api-request-blob` containing a copy of an already recorded tool
  result. The fixture now counts native `user` result records, not copied API
  requests; distinct native answers are still rejected. Native arbitration
  validation passed after this correction: all three remote/local/cancel
  scenarios, both standalone and through the real Clank question hook/broker.
- Strict workspace Clippy and formatting passed. With OS-level Python exec
  denial, 3,555 CLI unit tests and all 428 integration tests passed; two CLI
  unit tests failed because macOS also refuses the setuid `/bin/ps` inside a
  sandbox. A direct `sandbox-exec ... /bin/ps` reproduced that refusal. These
  are native process-identity tests and will be validated outside that sandbox;
  they are not counted as passed in the Python-denied run. The legacy-process
  fixture now owns its child with `proc_run::Managed` so even a failed probe
  kills and reaps it. No Python invocation was found in either test.

## Stress, Linux and fault validation

- Linux aarch64 validation passed in an isolated checkout on the existing
  Arch VM, Rust 1.99.1 with private Node 22.14.0: formatting, strict workspace
  Clippy, full parallel workspace tests and maintenance tests. CLI library:
  3,555 passed, four ignored, 55.04s; integration: 428 passed, two ignored,
  57.65s. All other workspace suites passed. This is Linux coverage, not an
  execution of Ubuntu GitHub Actions or its actionlint/licence-tool steps.
- All 16 selected repaired/runtime-isolation tests passed 200 iterations each
  with normal parallelism and no retries (3,200 test executions). Both native
  process-identity tests excluded by the Python-denial sandbox also passed
  outside that sandbox.
- Fault injection in an isolated source snapshot confirmed three assertions:
  dropping a blocking worker instead of joining it fails cancellation coverage;
  waking before queuing a reopen request fails the native event-order check;
  accepting a version prefix fails the consolidated release validator on a
  build suffix. Each compiled successfully and failed its intended test. The
  snapshot was removed; no injected fault touched the working tree.

Background checks are launched through `clank run`. The harness exposed their
exit status, and log inspection found both a compile failure and a later passing
run. A separately declared formatter/validation attendance also produced the
expected 10-minute check-in; it was cleared after the finished run was inspected.
The implementation has not yet reached the reviewer handoff.

## Final acceptance and limits

- Final macOS parallel workspace: CLI library 3,557 passed, zero failed,
  four ignored in 47.67s; integration 428 passed, zero failed, two ignored in
  44.99s. Maintenance five passed. Other suites: core 349 plus seven round
  trips, user-config 12, web 30 plus seven bridge cases and one example,
  web-cloudflare 53 (three ignored), and one core doctest all passed.
  Formatting and strict workspace/all-target Clippy passed.
- The final workspace command took 184.81s including 76s of compilation.
  Compare suite time, not that build-inclusive number, with the baseline:
  CLI 78.48s to 47.67s; integration 57.35s to 44.99s. Baseline failures and
  shared-runtime contamination make this evidence of reduced fixture cost,
  not a controlled speed benchmark. Total non-ignored cases changed from
  4,454 (including nine failures) to 4,450 passing; the goal was useful
  protection and fewer incidental processes, not a deletion quota.
- Three further isolated faults were caught by their intended tests:
  removing the receipt diagnostic cap, reporting `Applied` while still
  `Starting`, and accepting a stale restart configuration. Together with the
  three earlier faults, all six compiled and produced assertion failures;
  all temporary source snapshots were removed. These checks establish
  retained behavioral protection, not exhaustive mutation coverage.
- Python-denied maintenance tests and complete notice regeneration passed.
  The earlier Python-denied workspace passed every case except the two native
  `ps` probes refused by macOS's sandbox; both passed outside it, including
  in the final workspace. Python is absent from test/release execution paths.
  The Python-looking argv in `monitor_upgrade` is inert matching data.
- Optional installed Claude 2.1.308 acceptance passed remote answer, local
  answer and cancellation both standalone and through Clank's broker.
  Installed Grok and Codex acceptance, the browser/Playwright suite and live
  Cloudflare/network tests were not run. Ignore annotations remain explicit;
  none was added to suppress a flake. Linux validation above passed, but
  Ubuntu Actions, actionlint and cargo-deny were not executed in that VM.
- No production behavior change was needed for the observed flakes. The
  production Grok code gained narrow dependency seams with the same native
  implementation. Native fixture capabilities and the runtime override are
  gated by `test-runtime`; default installed builds do not enable them.
  Explicitly building/installing with that test feature or `--all-features`
  does opt into the test binary and override, and is not a production build.

## Executable inventory and retained boundaries

- Shared `tests/support/git.rs` owns ordinary repository initialization and
  selected commit/index fixtures. `TestEnv::init` alone replaces four Git
  processes per fixture; feedback-read, preview/rebuild, queue/finish,
  receipt/provider setup and diff/HTML fixtures reuse it. The helper commits
  named files and preserves parent trees; it deliberately does not emulate
  Git's filters, ignores, executable bits, submodules or merge machinery.
- Native Git remains in Git backend comparisons, rewrite/rebase/merge,
  worktree/index/path edge cases and hook-invocation integration families.
  Some construct their native repository alongside the independent Git
  oracle. Replacing those operations with the same gix implementation under
  test could hide interoperability failures. The shared initialization and
  simple commit helpers are the default for new ordinary fixtures.
- Cargo-built `clank` is retained where CLI argument/environment/exit handling
  or a real hook is the contract. The shared `clank-test-fixture` is built
  once by Cargo for native producer/receipt, Grok ACP and opt-in Claude PTY
  tests. No test starts another compiler to build a fixture binary. The
  remaining Cargo calls in workspace-license tests read `cargo metadata`;
  actual release builds belong to release scripts, not ordinary test fixtures.
- Shell, `sleep`, `cat`, and native process inspection remain for actual
  process-group, signal, pipe, shell hook and installer contracts. Release
  archive/installer tests retain tar and platform shell utilities to exercise
  the shipped scripts. Removing every subprocess would erase those contracts.
- Node executes the shipped OpenCode plugin and JavaScript contracts. Optional
  browser checks additionally need their documented browser toolchain.
  Rust/Cargo, a linker/C toolchain and Git remain normal prerequisites.
  Full license/notices gates additionally need pinned cargo-about/cargo-deny;
  curl/tar are release/installer tools. Python is confined to the separately
  documented optional book-recording authoring tools.

## Flake disposition and workflow observation

The old `off_joins_a_poll_in_flight` report is covered by today's
`off_joins_a_listing_in_flight`: the listing must have entered before shutdown,
and the driver explicitly releases it. The zero-grace case and direct
cancelled-worker join test retain cancellation protection. Reopen filtering
has direct scan coverage and a native enqueue-before-wake check. Restart
readiness now follows its launch-history write. Receipt callback fixtures
avoid Python startup within the callback budget; the diagnostic cap remains
asserted against oversized stderr. Remote-owner readiness, transition and
owner-exit tests synchronize on observed events rather than guessed delays.
The 16-case, 200-iteration run includes all of these and the receipt/remote
failures from the baseline; the obsolete flaky draft is consolidated here.
Only one surviving flaky-test draft was present at completion; no unrelated
draft was removed to satisfy the plan's earlier reference to two notes.

Background work was launched through `clank run`; completion output and exit
statuses were consumed before follow-up work. Long checks produced attendance
check-ins; standalone declarations were cleared after completion. No rejected
or failing check is counted as passed above. Verbose wake text and the stale
static reviewer hook label are captured in the queued wake-message plan.
Master-following event ownership is a separate queued plan after discovering
Claude's watch stayed pinned when the roster changed. The reviewer handoff
is the next workflow observation; it has not yet happened at this audit point.
