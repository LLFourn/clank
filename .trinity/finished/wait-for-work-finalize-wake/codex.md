APPROVE

Reviewed the full series (beba479 plan intro → 4dea725 plan
revision → 34c4790 implementation). The plan landed exactly as
spec'd:

- `src/server/wait.rs::compute_match` keys the role-gate exception
  off the projected `WaitingReason::SessionFinished` rather than a
  parallel `candidate.is_finished` check. `waiting_on(...)` stays
  the single source of truth for what work exists; the exception
  is one early-return guarded by `terminal`, not branching
  scattered across the function.
- All the downstream wiring for the terminal state already
  existed and is now reachable: `WorkAction::SessionFinished`
  (`wait.rs:250`), `"session_finished"` wire variant
  (`wait.rs:785`), `derive_locations(..., SessionFinished, ...)`
  returning `Vec::new()` (`wait.rs:394`), and `caller_already_voted`
  returning false for SessionFinished (`wait.rs:347-352`).
- No new wire variants. No projection change. The bug fix is
  three lines plus a load-bearing comment explaining why the
  exception is keyed off the projected reason and not the raw
  flag.
- Two regression tests in `integration_tests` pin the actual
  blocked-waiter path, not the easier "already finished before
  the call" path:
  - `finalize_wakes_blocked_master`: spawn master wait_for_work
    on an unreviewed plan (master blocks because waiting_on
    routes to reviewers), commit Finalize, signal HeadChanged,
    assert session_finished within 3 s.
  - `finalize_wakes_blocked_reviewer`: pre-approve as `bob` so
    waiting_on flips to {role:Master, reason:ReadyToStartImpl},
    bob's reviewer wait_for_work blocks on role-mismatch, then
    finalize → assert session_finished.
- Ruthless's spot-check (reverting the production change while
  keeping the tests) confirmed both tests fail with `Elapsed(())`
  after 3 s — the exact symptom of the original bug. They are
  genuine regression coverage, not false positives.

Verification passed:
- `cargo test -p trinity finalize_wakes_blocked`
- `cargo test --workspace --exclude trinity-frontend`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`

Approved as-is.
