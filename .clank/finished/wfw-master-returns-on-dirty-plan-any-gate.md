# wfw-master-returns-on-dirty-plan-any-gate

`clank wfw` blocks a master when the plan file has
uncommitted edits AND the gate is `Unreviewed` or
`ChangesRequested`. That's incoherent: reviewers cannot
approve a version they haven't seen, and the version
they're being asked to wait for is sitting dirty in the
master's worktree, not in HEAD. The master is told to
wait on reviewers who are waiting on a commit that hasn't
happened.

The fix already exists — partially. In
`crates/core/src/wait.rs`, the gate match arms
`CommitGateState::Approved` and `CommitGateState::Finished`
already short-circuit to `WaitingOn::MasterToCommit` when
`worktree == PlanWorktreeStatus::BodyDirty` (lines
347-354). The other two gate states (`Unreviewed`,
`ChangesRequested`) ignore worktree status and fall through
to "wait on reviewers" / "address changes" — the buggy
case.

## Goal

A master with a dirty plan file should never block in
`wfw`. They get a `MasterNext::Commit` item immediately,
regardless of gate state — unless the plan is blocked, in
which case block precedence wins (existing behavior, no
change needed).

## Architecture framing

The current shape is "gate decides, worktree refines."
The right shape is the opposite: **uncommitted plan
edits represent author intent that supersedes whatever the
last committed version's review state says.** A dirty
plan file is a promise of a new revision; routing the
master to anything other than "commit it" is asking them
to make decisions on a version they're already replacing.

So worktree status should be checked first, gate second:

```rust
let waiting_on = match worktree {
    PlanWorktreeStatus::BodyDirty => WaitingOn::MasterToCommit,
    _ => match gate { /* existing per-gate logic */ },
};
```

The block-precedence path (lines ~256-282) runs before
this, so blocked plans already short-circuit — the
"unless blocked" carve-out the user asked for is
automatic. No new code path for it.

## Surfaces touched

- `crates/core/src/wait.rs`:
  - Restructure the `waiting_on` derivation (lines
    ~318-389) to dispatch on `worktree` first, falling
    into the existing gate match only for `Clean` /
    `PlanFileMissing`.
  - The two existing `BodyDirty => MasterToCommit` arms
    inside `Approved` and `Finished` become dead code —
    remove them; the outer dispatch covers all gates.
  - `WaitingReason::CommitPlanRevision` is already the
    right reason; carries through unchanged.
  - The `MasterNext::Commit` doc comment (line 28-30)
    currently says "Approved gate but the plan file has
    uncommitted edits." Update to: "Plan file has
    uncommitted edits. Commit before any further work or
    review motion can land."
- `crates/core/src/wait.rs` tests (same file, `tests`
  mod):
  - Add `master_with_dirty_plan_and_unreviewed_gate_returns_commit`:
    seed plan with one commit (unreviewed), set worktree
    BodyDirty, call `work_for(master, Role::Master)`,
    assert single `WaitItem::Master { next: Commit, .. }`.
  - Add `master_with_dirty_plan_and_changes_requested_returns_commit`:
    seed plan with reviews-requested-changes, set BodyDirty,
    assert `MasterNext::Commit`, NOT `Revise`.
  - Keep existing
    `master_with_dirty_plan_and_approved_gate_returns_commit`
    test if it exists; otherwise add it (regression
    fence against re-introducing the bug).
  - Add `master_with_dirty_plan_and_blocked_plan_returns_blocked`:
    seed a pending block on a plan with BodyDirty;
    assert the blocked-plan path wins (no `Commit`
    item emitted for any role — block-precedence holds).
  - Add `reviewer_does_not_get_work_on_master_dirty_plan`:
    seed unreviewed gate, BodyDirty, call
    `work_for(reviewer, Role::Reviewer)`. Assert empty.
    This documents the deliberate reviewer-side side
    effect (see "Reviewer-side consequence" below).

## Reviewer-side consequence

`waiting_on` is plan-scoped, not role-scoped. Today,
`Unreviewed` + `BodyDirty` produces
`ReviewerApprovalsMissing { missing }` — which wakes
reviewers. After this change it produces
`MasterToCommit`, which wakes nobody on the reviewer side
(line 488's catch-all).

This is correct: if the master's plan is dirty, reviewer
effort on the committed version is at risk of being thrown
away the moment master commits the revision. Routing
reviewers to wait until the master commits avoids wasted
review cycles. We are deliberately consolidating the "wait
for commit" state across roles — master gets `Commit`,
reviewers get nothing-yet.

Edge case: if the master abandons the dirty edit (resets
the worktree), reviewers immediately go back to seeing
the unreviewed commit as work. This is fine — wfw is
event-driven and re-folds on worktree changes.

## Out of scope

- Surfacing the dirty file in `WaitItem::Master`'s
  payload (e.g. include the diff hash). Current consumers
  don't need it; `next: Commit` is enough information.
- A `PlanFileMissing` carve-out. Today missing-file
  passes through to the existing gate logic; same after
  this change. If "plan file missing" should be its own
  master action ("restore the plan or finalize"), that's
  a separate plan.
- Auto-staging or auto-committing the plan file on the
  master's behalf. wfw is a signaling layer, not an
  action layer.
- `clank status` output changes. `status` reads the same
  `WorkStatus`, so the `MasterToCommit` rendering it
  already does for Approved+Dirty automatically extends.
  No new output strings needed.

## Why this matters

Two layered wins:

1. **Removes a deadlock-shaped UX.** Today a master can
   be told "wait on reviewers" while the reason
   reviewers are quiet is that the master hasn't shipped
   anything to review. wfw was supposed to eliminate
   this exact kind of guessing.
2. **Models the actual invariant.** The dirty-plan check
   currently lives in two arms of the gate match. The
   restructure makes it one outer dispatch, mirroring the
   real invariant: "a dirty plan file pre-empts all
   gate-driven decisions." Cleaner code, fewer places
   to forget the rule next time someone adds a gate
   state.
