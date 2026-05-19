# wait-for-work-finalize-wake

`wait_for_work` doesn't return `SessionFinished` to its caller when
a plan finalizes. Reviewers (and masters) blocked on a long-poll
just sleep through the Finalize commit and time out 30 min later
with no signal that the plan is done.

## Why

When a Finalize commit lands:

1. `runtime` rebuilds the repo and broadcasts a `LiveEvent`.
2. `wait_for_work`'s blocked `rx.recv()` returns, re-enters
   `compute_match`.
3. `compute_match` calls `projection::waiting_on(is_finished:
   true, ...)`, which returns
   `WaitingOn { role: WaitingRole::None, reason: SessionFinished, ... }`
   (`src/projection.rs:55-61`).
4. The role gate at `src/server/wait.rs:201-204` compares
   `w.role` (`None`) against the caller's `role` (`Master` or
   `Reviewers` — never `None`). Mismatch → returns `Ok(None)`.
5. Waiter goes back to sleep until the next broadcast (which
   may never come if nothing else changes), eventually timing
   out.

All the downstream wiring for the terminal state already exists
and is unreached today:

- `WorkAction::SessionFinished` (`src/server/wait.rs:245`)
- `"session_finished"` wire variant
  (`src/server/wait.rs:780`, also in the trinity-core API enum)
- `derive_locations(..., SessionFinished, ...)` returns `Vec::new()`
  (`src/server/wait.rs:389`)
- `prompt_hint_for(SessionFinished, ...)` returns `None`
  (`src/server/wait.rs:316`); the response just won't carry a
  hint, which is fine.

The role gate is the only thing in the way.

## What

Three-line code change in `src/server/wait.rs::compute_match`:

```rust
let w = waiting_on(candidate.is_finished, status, candidate.gate.as_ref());
// SessionFinished is terminal for every participant — wake any
// role. Key the exception off the projected reason, not the raw
// candidate flag, so `wait_for_work` has a single source of
// truth (`waiting_on`) for what work exists.
let terminal = matches!(w.reason, WaitingReason::SessionFinished);
if !terminal && w.role != role {
    return Ok(None);
}
```

Plus a regression test that exercises the **blocked-waiter path**
(the actual bug), not just the "already-finished" entry path. The
shape:

- Build an active plan with at least one approved commit (so
  Finalize is a legal next operation in the harness).
- Spawn `wait_for_work({role: Master, plan_id})` as a tokio task
  with a generous deadline. It blocks.
- Commit the Finalize move on the master thread (`git commit`
  adding `.trinity/finished/<slug>/<reviewer>.md`).
- `await` the wait_for_work task with a tight timeout (say 5 s).
  Assert it returns `WorkAction::SessionFinished`, not a Timeout.
- Repeat for `role: Reviewers`.

Both role cases pin the broadcast-wakes-the-waiter path through
the new role-gate exception. An "already finished before the
call" assertion is fine to add too as a smoke test, but the
blocked-waiter case is the one that catches regressions.

## Files touched

- `src/server/wait.rs` — the role-gate exception in
  `compute_match`, plus the new regression test.

## Acceptance criteria

- A `wait_for_work({role: master, plan_id: X})` call that
  blocks on an active plan returns `{work: "session_finished",
  ...}` promptly (sub-second) after `X` finalizes — the
  broadcast wakes the long-poll and the new role-gate exception
  lets it through.
- Same for `role: reviewers`.
- Existing behavior unchanged for active plans: master-role
  reasons still gate on Master; reviewer-role reasons still
  gate on Reviewers. The exception keys off
  `waiting_on(...).reason == SessionFinished`, not a second
  independent finish check.
- The regression test exercises the blocked-waiter path for both
  roles.
- `cargo test -p trinity --test end_to_end` and the
  `src/server/wait.rs` unit tests pass.

## Non-goals

- Changing the `WaitingOn` projection. `WaitingRole::None` for
  finished plans is correct as a description of "no one is
  blocking" — the bug is in how wait_for_work consumes it, not
  in what it says.
- Changing the wire shape. `WorkAction::SessionFinished` is
  already in the wire enum; nothing new appears on the response.
- Server-side push of "plan finished" to the SSE stream. The
  runtime already broadcasts on the Finalize commit; this plan
  is only about wait_for_work returning the right work item when
  that broadcast wakes it.

## Rules

- One role gate, one exception. Don't grow new SessionFinished
  branches scattered across compute_match — funnel through the
  early-return at the role check.
- No new variants. The existing `WorkAction::SessionFinished`
  carries everything the response needs.

## Testing

- `cargo test --workspace --exclude trinity-frontend`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt -- --check`
- Manual: in one terminal `mcp_shim wait_for_work role=master
  plan_id=trinity/<some-plan>.md`, in another finalize that
  plan, observe the first terminal returns with
  `work: "session_finished"` within ~1 s.
