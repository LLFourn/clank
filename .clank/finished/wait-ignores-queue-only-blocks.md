# wait-ignores-queue-only-blocks

## Bug

`clank wait` currently treats a human-awaiting block on a queued plan as work and
returns immediately:

```text
blocked  claude/simctl-up-design-decisions  scope=simctl-up  (awaiting human)
```

That is status context, not agent work. If the block is awaiting a human answer,
an agent woken by `wait` has nothing to do. Returning immediately makes stop
hooks / wait loops churn on a non-actionable condition.

Related display bug: `clank status --tui` currently appears to use the normal
queue/promote indicator for this state. A human block must visually dominate the
promote/queue state: if the queued item that would otherwise be promoted is
blocked, the TUI should show the block state, not the usual promote emoji.

Live repro left intact by the user:

```text
/Users/llfourn/src/frostsnap/.clank/worktrees/full-app-sim-driver
```

Observed state:

- `clank status` shows `queue: 1 item`.
- The queued item is `.clank/queue/500-simctl-up.md`.
- The pending block is
  `.clank/agents/claude/blocks/simctl-up/simctl-up-design-decisions.md`.
- The block is scoped to `simctl-up`, which is queued, not active.
- In the user's bound session, `clank wait --timeout 5` returns immediately with
  the `blocked ... (awaiting human)` row instead of waiting until timeout or
  until real work appears.
- `clank status --tui` shows the normal promote emoji for the same
  blocked-queued-plan state, instead of the block indicator.

## Likely Cause

`crates/cli/src/cli/wait.rs` has explicit queue-only block surfacing logic in
both the initial pass and the watch-loop pass:

- It scans the queue to the first item not suppressed by a plan-scoped block.
- If every queued item is suppressed, it emits the pending `Blocked` items and
  returns.

That behavior was intended to show what holds the queue, but it violates the
wait contract: `wait` should return for actionable work, answered blocks
(`Unblocked`), or timeout. A pending human block is not actionable.

## Desired Behavior

- `clank status` should continue to show pending blocks, including blocks scoped
  to queued plans. The user still needs visibility.
- `clank status --tui` should make blocked state dominate the queue/promote
  indicator for blocked queued plans. The user should be able to see that the
  queue is stopped on a human ask, not that it is normally ready to promote.
- `clank wait --timeout 5` should not return immediately when the only thing in
  the repo is a pending block on every queued plan. It should wait until timeout.
- If a lower-priority queued plan is not blocked, `wait` should still return
  `promote_from_queue` for that unblocked queued plan. A block on the top queue
  item must not hide lower-priority work.
- If an unblock answer appears for the calling agent, `wait` should return
  immediately with `Unblocked`.
- If an actionable active-plan item exists, `wait` may still co-surface blocked
  context alongside that actionable item. The important invariant is that
  `Blocked` alone does not make `wait` return.
- Repo-wide pending blocks should be reviewed under the same invariant: they may
  suppress work, but if they are only awaiting a human, they should not be
  emitted as the sole wait result unless there is a deliberate UI reason and a
  test that justifies it.

## Implementation Notes

- Do not remove `WaitItem::Blocked`; it is useful for status/JSON context and
  for co-surfacing beside real work.
- Change the queue-only branches in `crates/cli/src/cli/wait.rs` so "queue
  non-empty but all entries suppressed by pending blocks" parks rather than
  emitting only blocked items.
- Prefer a helper that encodes the rule once: pending `Blocked` items are
  attachable context, not wake-worthy work. That avoids reintroducing the bug in
  the initial and watch-loop branches independently.
- Keep the `Unblocked` path first-class and wake-worthy.
- Audit the TUI queue/status rendering path and make its precedence match the
  wait/status model: block > promote. If a queued plan is suppressed by a
  pending plan-scoped block, render the block emoji/state for that queue row or
  header instead of the normal promote emoji.

## Tests

Add in-process CLI/core tests that do not spawn long-running binaries:

- Queue contains one item, that item has a pending plan-scoped block, and there
  are no active actionable plans: `wait --timeout <short>` times out rather than
  emitting only `Blocked`.
- Queue contains two items, the first is blocked and the second is unblocked:
  `wait` emits `promote_from_queue` for the second item.
- Active actionable work plus an unrelated/other-plan pending block still
  returns the actionable item and may include blocked context.
- A pending block that later gets an answer still wakes the owning agent via
  `Unblocked`.
- TUI rendering for a queued plan with a pending plan-scoped block uses the
  block indicator rather than the normal promote indicator.

Use the Frostsnap worktree above as a manual regression fixture while it remains
available, but pin the behavior with repo-local tests so the fix does not depend
on that external path.

## Acceptance

- In the Frostsnap repro, with only `simctl-up` queued and blocked awaiting a
  human, `clank wait --timeout 5` waits for the timeout instead of returning the
  pending block row.
- `clank status` still displays the queued item and the pending block.
- `clank status --tui` shows the block indicator/state for the blocked queued
  plan rather than the normal promote emoji.
- Lower-priority unblocked queue items still surface for promotion.
- Answered blocks still wake the target agent.
- Focused tests pass; `cargo check -p clank` passes.
