# queue-wfw-watch

## Summary

wfw should watch `.clank/queue/` and emit `PromoteFromQueue`
when a queue item appears, not just at startup.

## Design

No new `FilesystemSignal` variant needed. CLI wfw's watcher
already sends a unit wake on any notify event under `.clank/`.
Since `.clank/queue/` is inside `.clank/`, queue file
creation already wakes the loop.

The fix: after each refold in the watch loop finds no plan or
ad-hoc work, scan the queue. If the role is Master and queue
is non-empty, emit `PromoteFromQueue` and exit.

Remove the fast-exit for idle master with no plans — let it
fall through to the watch loop so it stays parked. The idle
hook fires once before parking but doesn't cause an exit.

## Implementation

- `cli/wfw.rs`: remove the fast-exit block for idle master
  (the `if role == Master && plans.is_empty()` early return).
  Master always falls through to the watch loop.
- `cli/wfw.rs`: in the watch loop, after `derive_status` +
  `detect_finished` finds no items, scan queue via
  `scan_queue(&repo)`. If Master and queue non-empty, emit
  `PromoteFromQueue` and return.
- Update `wfw_master_empty_exits_even_with_hooks_configured`
  test — master no longer exits immediately when idle, it
  parks. The test should verify wfw blocks (timeout exit)
  instead of immediate exit.

## Tests

- wfw master parked with no plans, queue file added mid-watch
  → wfw wakes and emits PromoteFromQueue.
- wfw master with active plans ignores queue.
