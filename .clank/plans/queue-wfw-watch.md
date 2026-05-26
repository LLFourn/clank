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

- `cli/wfw.rs`: change the idle-master fast-exit block to:
  1. Scan queue immediately — if non-empty, emit
     `PromoteFromQueue` and return (preserves startup behavior).
  2. If queue empty, fire idle hook (no exit), then fall
     through to the watch loop.
- `cli/wfw.rs`: in the watch loop, after `derive_status` +
  `detect_finished` finds no items AND role is Master,
  scan queue. If non-empty, emit `PromoteFromQueue` and
  return.
- Update tests:
  - `wfw_master_empty_exits_even_with_hooks_configured` →
    master now parks instead of exiting; use timeout.
  - `wfw_master_no_plans_exits_immediately_json` → master
    parks; use timeout for the empty case.
  - `wfw_idle_hook_returns_prompt` → idle hook fires but
    wfw doesn't exit with Idle item; it parks.

## Tests

- Startup with non-empty queue → immediate PromoteFromQueue.
- wfw master parked with no plans, queue file added mid-watch
  → wfw wakes and emits PromoteFromQueue.
- wfw master with active plans ignores queue.
