# plan-queue

## Summary

Add a plan queue between stubs and active plans. When wfw has
no work (no active plans, no ad-hoc), it returns a new
`WaitItem::PromoteFromQueue` telling the agent to promote the
highest-priority queued item to an active plan.

## Layout

```
~/.clank/stubs/<name>.md                — ideas, not committed
<repo>/.clank/queue/<NNN>-<name>.md     — prioritized, gitignored
.clank/plans/<name>.md                  — active, committed
.clank/finished/<name>.md               — done, committed
```

Queue is repo-scoped at `.clank/queue/` (gitignored, not
committed). The filename starts with a 3-digit priority
(`000` = highest). Files are promoted in lexicographic order
(lowest number first). `clank init` adds `queue/` to
`.clank/.gitignore`.

## `clank queue` subcommand

### `clank queue`

List queued items in priority order.

### `clank queue add <stub-name> [--priority NNN]`

Copy `~/.clank/stubs/<stub-name>.md` to
`<repo>/.clank/queue/<NNN>-<stub-name>.md`. Default priority
500. Error if the stub doesn't exist.

### `clank queue remove <name>`

Delete an item from the queue. The file is removed, not
moved back to stubs.

### `clank queue promote <name>`

Move `<repo>/.clank/queue/<NNN>-<name>.md` to
`.clank/plans/<name>.md` and commit. Uses path-limited
`git add .clank/plans/<name>.md` and
`git commit .clank/plans/<name>.md -m "[<name>] intro"`
so unrelated staged changes are not included.

## wfw integration

In `derive_status` or `work_for`: when there are no plan
work items and no ad-hoc work items, check
`<repo>/.clank/queue/` for files. If any exist, return:

```rust
WaitItem::PromoteFromQueue {
    name: String,
    priority: u16,
}
```

The stop-hook renders this as:

```
- promote: queue item `foo` (priority 100) — run
  `clank queue promote foo`
```

Only the highest-priority item is surfaced. Only emitted
for `Role::Master` — reviewers idle normally. If the queue
is empty, wfw returns the existing idle behavior.

## Implementation

- `cli/queue.rs`: new module with list/add/remove/promote.
- `cli/mod.rs`: `QueueArgs` with subcommands.
- `main.rs`: wire `Queue` variant.
- `core/wait.rs`: add `WaitItem::PromoteFromQueue`.
- `cli/wfw.rs`: after derive_status finds no work AND
  role is Master, scan the queue directory and emit
  PromoteFromQueue if non-empty.
- `cli/stop_hook.rs`: render PromoteFromQueue.
- Queue scanning is filesystem-only (not git), reads
  `<repo>/.clank/queue/` sorted lexicographically.

## Tests

- `clank queue add` copies stub to queue with priority prefix.
- `clank queue` lists items in priority order.
- `clank queue promote` moves to plans/ and commits.
- wfw with no plans and non-empty queue returns PromoteFromQueue.
- wfw with active plans ignores the queue.
- Stop-hook renders promote instruction.
- Promote with unrelated staged changes does not include them.
