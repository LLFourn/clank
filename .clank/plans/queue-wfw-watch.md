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
- `runtime.rs`: handle `QueueChanged` — trigger a rebuild
  (same as HeadChanged).
- `cli/wfw.rs`: in the watch loop, after derive_status
  finds no work, scan the queue and emit PromoteFromQueue
  (same logic as the startup path).

## Tests

- wfw master parked with no plans, queue file added mid-watch
  → wfw wakes and emits PromoteFromQueue.
