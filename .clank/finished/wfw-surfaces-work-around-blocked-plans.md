# wfw-surfaces-work-around-blocked-plans
# When the active plan is blocked at agent level, surface the next queue item (or other active plans) instead of stale "act on this plan" hints

## Problem

Reported by lloyd 2026-06-04:
> Plan-scoped block doesn't surface the next queue item: when the active plan is blocked at the agent level, the stop hook still says "act on this approved plan" — even though the agent can't. There's no signal to promote the next queue item or work on another active plan.

Verified at `crates/cli/src/cli/wfw.rs:221-224`:

```rust
if !initial_suppress_all
    && role == Role::Master
    && plan_filter.is_none()
    && initial_state.fold.plans.is_empty()  // <-- bug
{
    // …suggest first queue item via WaitItem::PromoteFromQueue
}
```

The queue-promotion suggestion only fires when **zero active plans exist**. If one or more plans are active but all of them are suppressed by per-plan blocks for this agent, `fold.plans` is non-empty → the queue suggestion never fires. The stop hook silently has nothing to surface, OR (worse) surfaces a stale "approved plan" item the agent can't actually act on.

## Verified before promotion

- `check_blocks` produces `suppressed_plans: BTreeSet<PlanKey>` correctly. The data is there; only the dispatch logic doesn't consume it.
- Filter at lines 202-209 (initial path) and 316-323 (watch path) already drops `WaitItem::Master`/`Reviewer` entries whose plan is in `suppressed_plans`. So the per-plan suppression IS applied to the items list. But the queue fallback check at :221 doesn't account for the suppression.
- **Two gate sites need the same fix** (verified 2026-06-05): the initial-pass gate at `wfw.rs:221-225` AND the watch-loop gate at `wfw.rs:332-348`. Both check `state.fold.plans.is_empty()`. Both need to switch to the actionable-count check. Easy to miss the second one; the plan calls it out explicitly.
- **Reviewer-symmetry question resolved (negative)**: both gate sites are also gated on `role == Role::Master`. Reviewers ALREADY don't get `PromoteFromQueue` or the idle hook today — that's pre-existing behavior independent of this plan's bug. This plan's fix is master-path-only. Whether reviewers SHOULD get an idle signal when all their reviewable plans are suppressed is a separate question for a separate plan. (Plan body's earlier "verify and fix consistently" note resolved to "preserve existing reviewer behavior" — the symmetric reviewer fix is not in scope.)
- The "idle hook fires" branch at line 241 is also gated on `fold.plans.is_empty()`. Same actionable-count fix applies.

## Approach

1. **Compute "actionable plan count" for the agent.** Currently the code branches on `initial_state.fold.plans.is_empty()`. Replace with a derived count: `actionable = fold.plans.len() - intersection_with_suppressed_plans`. When that's zero, the agent has nothing to do on active plans regardless of why.

2. **Re-gate the queue-promotion + idle-hook paths** on `actionable == 0` instead of `fold.plans.is_empty()`. Master sees the next queue item even when all plans are blocked at the agent level.

3. **Watch loop**: the second gate at `wfw.rs:332-348` repeats the same `fold.plans.is_empty()` check after a snapshot rebuild. Apply the same actionable-count fix (otherwise the watch wakes up and re-suppresses without surfacing the queue).

4. **Reviewer path stays as-is.** Reviewers already don't get `PromoteFromQueue` or the idle hook (both code paths gate on `Role::Master`). Whether they SHOULD is a separate concern outside this plan's scope.

## Known risk (do NOT over-engineer pre-emptively)

Flagged by lloyd during plan revision: if an agent creates a plan-scoped block mid-implementation (working tree has uncommitted edits for plan A), this plan's fix makes wfw immediately surface "promote plan B from queue" as the next signal. An agent that follows the signal literally could:
- Switch context to plan B's setup work.
- Lose track of plan A's uncommitted edits.
- Mix plan A and plan B working-tree state on resume.

The user has decided to **ship as-spec'd and observe in practice** rather than gate the queue-promote surface on "working tree is clean." Rationale: most blocks are filed when work is in a clean state (the agent has nothing concrete to commit, hence the block); the dirty-tree case is real but not necessarily common.

If it surfaces as a real problem:
- Cheap fix: have wfw refuse to emit `PromoteFromQueue` when `git status --porcelain` is non-empty for files outside `.clank/`. The agent sees the block as the only actionable item and must resolve the WIP first.
- More structured fix: a `clank park` subcommand that stashes the current plan's WIP into a named per-plan workspace before the queue-promote signal fires.

Neither is in scope for this plan. Implementers should NOT add the working-tree-check pre-emptively — let real usage decide whether it's necessary.

## Out of scope

- Telling the agent WHICH block is suppressing each plan in the wfw output. The existing `WaitItem::Blocked` items already carry that info; this plan only fixes the actionable-signal gap, not the diagnostic.
- Auto-promoting the next queue item without user action. The signal is a hint; promotion still requires `clank queue promote`.
- Changing block scope semantics (see `block-create-explicit-scope` for that).

## Acceptance

- Single active plan + block on that plan + non-empty queue → master's wfw returns `PromoteFromQueue` for the next queue item AND the pending `Blocked` items (so the user sees both).
- Single active plan + block on that plan + empty queue → idle hook fires.
- Two active plans + block on plan A + plan B is gate_approved for master → master sees only plan B's actionable item (and the Blocked entry).
- No active plans + non-empty queue → existing behavior unchanged.
- Stop-hook continuation correctly mentions the queue item when triggered via this path.
- `cargo test --workspace` passes.

## Tests

In `crates/cli/tests/wfw_integration.rs` (existing file with the test infrastructure):

- `wfw_master_blocked_plan_surfaces_next_queue_item`: setup repo with one active plan + a plan-scope block on that plan + one queued item; assert wfw output is `PromoteFromQueue` for the queue item plus the `Blocked` entry.
- `wfw_master_blocked_plan_empty_queue_fires_idle_hook`: setup with no queue items; assert idle hook ran (via the existing hook config mechanism).
- `wfw_master_one_blocked_one_actionable_returns_only_actionable`: two plans, one blocked, one with master work pending; assert only the actionable plan surfaces.
- `wfw_reviewer_blocked_plan_does_not_surface_promote`: reviewer doesn't get `PromoteFromQueue` (preserves existing role-gate behavior). Regression-guard: this plan's master-path fix must not leak the surface to reviewers.
