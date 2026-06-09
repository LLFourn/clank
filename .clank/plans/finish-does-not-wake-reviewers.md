# finish-does-not-wake-reviewers

## Problem

When a plan finalizes, `clank wfw` wakes REVIEWERS with a
"plan finished" notice — even though a reviewer has nothing to
do about a finished plan. Reviewers should only be woken when
there's an action for them (a commit to review).

## Where it comes from

`detect_finished` (crates/core/src/wait.rs:666) emits a
`WaitItem::Finished` for every watched plan that flipped to
finished since the wfw startup snapshot. In wfw.rs it's appended
to the work list UNCONDITIONALLY, after the role-aware
`work_for`:

- wfw.rs:218 (initial pass) and wfw.rs:375 (watch loop):
  `items.extend(detect_finished(&snapshot, &state.fold))` — no
  role guard, and appended AFTER the plan_filter retain so it
  isn't even scoped to the reviewer's watched plan.

Because the wake gate is `if !items.is_empty()` (wfw.rs:219), a
Finished notice ALONE makes `items` non-empty, so an otherwise-
idle reviewer is emitted to (woken) and the `PlanFinalized` hook
fires (wfw.rs:74).

## The architectural issue

`Finished` is a NOTIFICATION, not WORK. The wfw work stream is
"things this agent should DO"; injecting a notification into it
and gating wake on `!items.is_empty()` conflates the two, so a
notification wakes an idle agent. Reviewers — who never act on a
finish — are the visible victim.

Contrast Blocked (wfw.rs:224-230): blocked items are CO-SURFACED
only inside the `!items.is_empty()` block, i.e. only when there's
already actionable work, so a Blocked-only situation never wakes.
Finished should obey the same "co-surface, never wake on its own"
rule — and for reviewers it shouldn't surface at all.

## The fix

Keep `detect_finished` out of the reviewer work stream entirely:
a reviewer is never woken by a finish.

- Scope the `detect_finished` extend to `role == Role::Master`
  (master is the finalization-lifecycle role and the natural home
  for the `plan_finalized` hook). Reviewers' wfw ignores Finished.
- Preserve the two legitimate paths:
  - The one-shot "your `--plan` target is already finished, stop
    waiting" exit (wfw.rs:150) — keep; it's for an agent that
    explicitly asked about that plan.
  - The `plan_finalized` hook (HookEvent::PlanFinalized, wfw.rs:74)
    still fires — now from master's stream only, so
    `say $CLANK_PLAN finalized` still happens once without waking
    reviewers.
- Recommended: also stop Finished from being the SOLE wake reason
  for MASTER (co-surface like Blocked; never wake an idle master
  on a finish alone). At minimum, reviewers must never wake.
  Decide during impl.

## Scope

- crates/cli/src/cli/wfw.rs — the two
  `items.extend(detect_finished(...))` sites (218, 375) and their
  interaction with the `!items.is_empty()` wake gate.
- crates/core/src/wait.rs — `detect_finished` stays a pure
  function; the role decision lives at the call sites.

## Out of scope

- The plan_filter one-shot finished exit (wfw.rs:150) — keep.
- The `PlanFinalized` hook itself — keep firing (just not to
  reviewers).

## Origin

The finish-notice-to-all-roles behavior came from
`wfw-finish-notification` (finished). This narrows it: notices
serve master + the lifecycle hook, not a reviewer wake.

## Status

Stub — queued (lloyd 2026-06-09: finish wakes reviewers with
nothing to do; reviewers should only wake when there's an action).
