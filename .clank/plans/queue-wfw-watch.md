# queue-wfw-watch

## Summary

wfw should watch `.clank/queue/` and emit `PromoteFromQueue`
when a queue item appears mid-watch, not just at startup.

## Design

The watcher already watches all of `.clank/`. When a file
appears in `.clank/queue/`, `fs_watcher::path_to_signal`
should emit a new `FilesystemSignal::QueueChanged`. The wfw
watch loop handles this signal by scanning the queue and
emitting `PromoteFromQueue` if the role is Master and there
are no active plan work items.

## Implementation

- `fs_watcher.rs`: add `FilesystemSignal::QueueChanged`.
  `path_to_signal` returns it for paths matching
  `.clank/queue/*.md`.
- `cli/wfw.rs`: remove the fast-exit for idle master when
  queue is empty. Instead, fall through to the watch loop
  so wfw stays parked and can wake on queue changes.
  The idle hook still fires once at startup (before parking),
  but wfw doesn't exit — it parks and watches.
  In the watch loop, after derive_status finds no work,
  scan the queue and emit PromoteFromQueue. On
  `QueueChanged` signal, re-scan the queue without a full
  refold.
- `runtime.rs`: no change needed — the CLI wfw watcher
  handles queue signals directly in its own notify loop.

## Tests

- wfw master parked with no plans, queue file added mid-watch
  → wfw wakes and emits PromoteFromQueue.
- wfw master with active plans ignores queue changes.
