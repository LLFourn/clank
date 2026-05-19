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

Two-line code change in `src/server/wait.rs::compute_match`:

```rust
let w = waiting_on(candidate.is_finished, status, candidate.gate.as_ref());
// SessionFinished is terminal for every participant — wake any
// role. Other states gate on a role match.
if !candidate.is_finished && w.role != role {
    return Ok(None);
}
```

Plus a regression test in `src/server/wait.rs::tests` that:

- Builds a finalized plan via the existing test harness.
- Calls `wait_for_work` for both `Master` and `Reviewers`.
- Asserts both return `WorkAction::SessionFinished` (not Timeout,
  not None).

## Files touched

- `src/server/wait.rs` — the role-gate exception in
  `compute_match`, plus the new regression test.

## Acceptance criteria

- `wait_for_work({role: master, plan_id: X})` returns
  `{work: "session_finished", ...}` immediately after `X`
  finalizes (no timeout wait).
- Same for `role: reviewers`.
- Existing behavior unchanged for active plans: master-role
  reasons still gate on Master; reviewer-role reasons still
  gate on Reviewers.
- The regression test pins both role cases.
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
