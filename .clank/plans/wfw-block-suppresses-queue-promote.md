# wfw-block-suppresses-queue-promote

Open blocks should suppress `promote_from_queue` items in
`clank wfw` output, the same way they already suppress
plan-level Master/Reviewer items via `suppressed_plans`.

## Problem

lloyd 2026-06-06 (during the `open-zellij-inherits-default-layout`
queue-evaluation): I had an open block on a queued plan and the
stop-hook kept firing `promote_from_queue` for that same plan on
every poll, ignoring the block.

`crates/cli/src/cli/wfw.rs::check_blocks` correctly suppresses
per-plan `Master` and `Reviewer` items for any plan with an open
block (via `suppressed_plans`). But that suppression only kicks
in AFTER a plan is promoted — queue-promote items pass through
the wfw loop independently.

The result is wfw noise: a block created specifically to gate
"don't promote this yet, I have a question" gets ignored by the
promote path, and the agent gets pinged on every poll.

## Goal

A block scoped to a queue item's name should suppress
`promote_from_queue` items for that same name, mirroring how
plan-scoped blocks suppress plan items.

## Approach

1. **Block scope detection**: today's `BlockEntry.plan: Option<String>`
   carries the plan name when scoped. The queue item's name is
   the same kebab-case identifier. Match by string.
2. **Suppression in the promote path**: locate the
   `promote_from_queue` emit site in `wfw.rs` (search for
   `WaitItem::PromoteFromQueue`). Before emitting, check
   `suppressed_plans` (the set built by `check_blocks`) — if the
   queue item's name is in it, skip the emit.
3. **Repo-wide blocks**: a block with `plan: None` already
   suppresses all per-plan work via the "repo-wide suppress"
   path. Extending it to also suppress `promote_from_queue`
   matches the user's expectation that "I've blocked everything;
   stop pinging me." Pin at promote-time.

## Surfaces touched

- `crates/cli/src/cli/wfw.rs`:
  - `check_blocks` builds `suppressed_plans`. Already in place.
  - The promote-emit site (grep for `PromoteFromQueue`) gains a
    pre-check: if `suppressed_plans.contains(queue_item.name)`,
    skip the emit. If there's a separate "repo-wide block
    present" boolean (from check_blocks), check it too and skip
    on true.

## Tests

- `wfw_block_on_queue_item_name_suppresses_promote`: create a
  queue item `foo`; create a block scoped to plan `foo`; assert
  `clank wfw --json` does NOT emit `promote_from_queue` for foo.
  Verify the `Blocked` item still emits.
- `wfw_repo_wide_block_suppresses_all_promotes`: create two queue
  items; create a repo-wide block (plan: None); assert no
  `promote_from_queue` items emitted; the `Blocked` item still
  emits.
- `wfw_block_on_different_plan_does_not_suppress_unrelated_promote`:
  queue item `foo`; block scoped to plan `bar`; assert wfw still
  emits `promote_from_queue` for foo (the block on bar shouldn't
  hide unrelated work).

## Out of scope

- Changes to block creation, answering, or lifecycle. This plan
  only changes the wfw output filter.
- Changes to `clank status`'s rendering of queue items (status
  doesn't show queue-promote suggestions; only wfw does).
- Renaming `suppressed_plans` to something queue-aware. The set's
  contents are strings; it can serve both purposes without
  renaming if the comment is updated.

## Acceptance

- Open block scoped to a queued item's name → `clank wfw` does
  NOT emit `promote_from_queue` for that item.
- Repo-wide open block (plan: None) → `clank wfw` does NOT emit
  ANY `promote_from_queue` item.
- Block scoped to plan `bar` does NOT suppress promote of queued
  item `foo` (no over-suppression).
- The `Blocked` item itself continues to emit as today (so the
  agent knows there's a block).
- `cargo test --workspace` passes.

## Related

- `status-blocks-dominate-gate` (FINISHED 2026-06-05): made
  blocks first-class in `derive_status` for the per-plan gate.
  This plan extends the same "blocks dominate" principle to
  queue-promote items in wfw.
- This plan's gap was empirically demonstrated by my own block
  on `open-zellij-inherits-default-layout` during 2026-06-06:
  block created, queue-promote still firing on every poll.
