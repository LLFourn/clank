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

- Gate the `detect_finished` extend (both wfw call sites: initial
  pass + watch loop) on `role == Role::Master`. Master is the
  finalization-lifecycle role and the natural home for the
  `plan_finalized` hook; a reviewer has no action on a finish.
- The `plan_finalized` hook (HookEvent::PlanFinalized, wfw.rs:74)
  still fires from master's stream, so `say $CLANK_PLAN finalized`
  still happens once without waking reviewers.

### Simplification: remove `wfw --plan` entirely (lloyd 2026-06-09)

Sizing this surfaced that `wfw --plan`'s only remaining
distinctive behaviors were finish-notice paths (the one-shot
"already finished at startup" exit and the watch-loop finished
notice for an explicit watcher). Under the teams redesign, all
team members are on board with all tasks — there are no separate
plans for separate agents, so a per-agent plan filter on wfw is
dead complexity. Rather than thread Finished-surfacing exceptions
through it (`role == Master || plan_filter.is_some()`), DELETE
the flag:

- `WfwArgs.plan` removed; `wfw` always watches every active plan.
- The `--plan` resolution block (incl. the one-shot
  already-finished exit) removed; `active_summary` and the
  `parse_arg`/`repo_basename` plumbing in wfw with it.
- `StartupSnapshot::capture(state)` loses its `plan_filter`
  param — the watched set is always every active plan at startup.
- The per-item plan_filter retain in both passes removed, and the
  master no-actionable-plans queue hint no longer checks
  `plan_filter.is_none()`.
- Other commands' `--plan`/plan args (status, log, block create,
  finish …) are untouched — this removes the WATCH filter, not
  plan addressing.

This collapses the gate to a clean `role == Role::Master` and
deletes the stale-resume one-shot path outright (an agent resumed
against an already-finished plan now just gets its normal
role-appropriate work view).

### Master: keep waking on Finished — Option A (resolved, ruthless 70b479f)

The stub floated also co-surfacing Finished for MASTER (never
wake alone, like Blocked). That CONTRADICTS keeping the
`plan_finalized` hook: the hook fires exactly when wfw emits the
Finished item (wfw.rs:74), so if master co-surfaced it (never
emitted alone), an idle-master finish would never fire
`say $CLANK_PLAN finalized`. The two can't both hold without
decoupling the hook from the wfw emit.

**Decision: Option A.** Master KEEPS waking on Finished — that
wake IS master's action on a finish (it fires the announcement
hook). Reviewers never wake on a finish (they have no action).
This is NOT symmetry-breaking for its own sake: master owns the
finalization lifecycle and its announcement; a reviewer owns
neither. So the ONLY change is the `role == Role::Master` guard
on the work-stream extend (218/375); master's behavior is
unchanged. (Option B — decouple the hook to `clank finish` time,
then co-surface master too — is more consistent with the
notification-not-work model but a larger change; deferred.)

Note (codex 70b479f): the stub's "appended after retain so it
isn't scoped to the watched plan" is imprecise —
`StartupSnapshot::capture` already applied the `--plan`
singleton. The bug is purely the role/wake semantics, not
explicit-plan scoping. Concern 2 (preserve the one-shot `--plan`
finished exit) is superseded by the `--plan` removal above — the
exit is deleted along with the flag it served. Concern 3:
concept-grep confirms the two work-stream extends are the only
`Finished` injections.

## Scope

- crates/cli/src/cli/wfw.rs — the two
  `items.extend(detect_finished(...))` sites (218, 375) and their
  interaction with the `!items.is_empty()` wake gate.
- crates/core/src/wait.rs — `detect_finished` stays a pure
  function; the role decision lives at the call sites.

## Out of scope

- The `PlanFinalized` hook itself — keep firing (just not to
  reviewers).
- `--plan`/plan args on OTHER commands (status, log, block
  create, finish …) — only wfw's watch filter is removed.

## Origin

The finish-notice-to-all-roles behavior came from
`wfw-finish-notification` (finished). This narrows it: notices
serve master + the lifecycle hook, not a reviewer wake.

## Status

Stub — queued (lloyd 2026-06-09: finish wakes reviewers with
nothing to do; reviewers should only wake when there's an action).
