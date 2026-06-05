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
2. **Suppression in the promote path** (PINNED per codex f64a974):
   the two promote-emit sites at `wfw.rs:253-265` and
   `wfw.rs:377-389` currently use `queue.first()`. Replace
   with `queue.iter().find(|q| !suppressed_plans.contains(&q.name))`
   — scan to the first UNSUPPRESSED queue item. This handles
   the "highest-priority item is blocked but lower-priority
   items aren't" case correctly. A blocked first item should
   not hide the next unblocked item — that would defeat the
   purpose of the queue.
3. **Repo-wide blocks** (PINNED per codex f64a974: behavior
   UNCHANGED): today, a repo-wide block (`plan: None`) makes
   wfw park/timeout without emitting a Blocked item — verified
   by the existing test at `wfw_integration.rs:1684`. **This
   plan does NOT change that behavior.** The promote path's
   new scan-for-unsuppressed logic is plan-scope-only. Repo-wide
   blocks continue to park the entire wfw call (their
   suppression is upstream of the promote scan and doesn't
   need this plan's change).

## Surfaces touched

- `crates/cli/src/cli/wfw.rs`:
  - `check_blocks` builds `suppressed_plans`. Already in place.
  - Two promote-emit sites at `:253-265` and `:377-389`. Each
    changes from `queue.first()` to
    `queue.iter().find(|q| !suppressed_plans.contains(&q.name))`.
    No change to the repo-wide-block code path (which short-
    circuits earlier).

## Tests

- `wfw_block_on_queue_item_name_suppresses_promote`: queue item
  `foo` (only); block scoped to plan `foo`; assert `clank wfw
  --json` does NOT emit `promote_from_queue` for `foo`. The
  `Blocked` item itself continues to emit (plan-scoped blocks
  already emit Blocked items today).
- `wfw_block_on_different_plan_does_not_suppress_unrelated_promote`:
  queue item `foo`; block scoped to plan `bar`; assert wfw still
  emits `promote_from_queue` for `foo`. No over-suppression.
- `wfw_blocked_first_queue_item_still_surfaces_next_unblocked`
  (codex f64a974 catch — pinned): queue items `foo` (priority
  100), `bar` (priority 200); block scoped to plan `foo`;
  assert wfw emits `promote_from_queue` for `bar` (the next
  unsuppressed item in priority order). A blocked first item
  must NOT hide later unblocked items — that would defeat the
  purpose of the queue.
- Existing `wfw_integration.rs:1684` test (repo-wide block
  suppresses everything): MUST continue to pass unchanged.
  Repo-wide block behavior (park/timeout, no Blocked emit) is
  explicitly preserved by this plan.

## Out of scope

- Changes to block creation, answering, or lifecycle. This plan
  only changes the wfw output filter.
- Changes to `clank status`'s rendering of queue items (status
  doesn't show queue-promote suggestions; only wfw does).
- Renaming `suppressed_plans` to something queue-aware. The set's
  contents are strings; it can serve both purposes without
  renaming if the comment is updated.
- Changes to repo-wide block behavior (codex f64a974 pin —
  repo-wide blocks today park wfw without emitting a Blocked
  item; this plan preserves that exactly).

## Acceptance

- Open block scoped to a queued item's name → `clank wfw` does
  NOT emit `promote_from_queue` for that item.
- Block scoped to plan `bar` does NOT suppress promote of queued
  item `foo` (no over-suppression).
- A blocked HIGHEST-priority queue item does NOT hide a later
  unblocked item — wfw scans to the first unsuppressed entry.
- Plan-scoped blocks continue to emit a `Blocked` item alongside
  the (now-filtered) promote scan, as they do today.
- Repo-wide block behavior is UNCHANGED — existing
  `wfw_integration.rs:1684` test passes without modification.
- `cargo test --workspace` passes.

## Related

- `status-blocks-dominate-gate` (FINISHED 2026-06-05): made
  blocks first-class in `derive_status` for the per-plan gate.
  This plan extends the same "blocks dominate" principle to
  queue-promote items in wfw.
- This plan's gap was empirically demonstrated by my own block
  on `open-zellij-inherits-default-layout` during 2026-06-06:
  block created, queue-promote still firing on every poll.

## Cycle notes

- `9da0243` (this revision): pinned queue-scan +
  repo-wide-unchanged per codex `f64a974`'s catches. Codex
  separately flagged that 9da0243 accidentally modified the
  open-zellij plan body (an in-progress edit from before
  lloyd blocked that plan). That cross-plan-scope issue was
  reverted in `5b964fa` (zellij plan body restored to its
  17475a5 state). This wfw plan's substance is unchanged.
