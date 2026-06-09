# gate-reviewers-only-plan-change-and-finish

## Problem

A gate reviewer (e.g. `ruthless`) currently reviews EVERY
commit, one step behind the commit reviewer. The gate state
machine treats the "gate tier" as an unconditional per-commit
second tier: as soon as all commit-reviewers go positive on the
latest reviewable commit, `compute_gate` returns
`ApprovedPendingGate` and the gate reviewer is woken — on every
commit, including intermediate pure-code WIP commits.

That is not what "gate" should mean. The gate reviewer is the
senior / expensive reviewer; it should weigh in only at two
milestones, never on routine WIP commits:

1. **Plan approval** — a commit that changes the plan document
   (`touched_plan`) and is approved by the commit reviewer. The
   spec/design is settled and the senior reviewer signs off on
   the plan itself.
2. **Finish** — a commit the commit reviewer marks FINISHED.
   The implementation is claimed done and the senior reviewer
   does the final gate before `clank finish`.

On every other commit (pure code, merely APPROVED), the gate
reviewer must stay asleep and the gate must pass straight to
`Approved` so master keeps moving.

## The model fix

Today's activation rule is unconditional: "commit tier positive
⇒ require gate tier". Make it conditional on the latest
reviewable commit being a gate MILESTONE:

> Once all commit-reviewers are positive on the latest
> reviewable commit, require the gate tier **iff** that commit
> `touched_plan` OR the commit-reviewers marked it FINISHED.
> Otherwise bypass the gate tier → `Approved`.

This is the smallest change that makes "gate reviewer wakes on
an intermediate code commit" *unrepresentable*: the gate tier is
consulted only at the two milestones, by construction — not
fixed up after the fact.

## Where it lives

- `crates/core/src/wait.rs` — `compute_gate(...)`. Currently
  `(reviews, commit_reviewers, gate_reviewers)`; after
  `all_commit_positive` it unconditionally returns
  `ApprovedPendingGate` when gate-reviewers haven't voted
  (~wait.rs:263-274). It must also receive whether the latest
  commit is a gate milestone and skip the gate tier (return
  `Approved`) when it is not.
  - Milestone signal = `touched_plan` of the latest reviewable
    commit, OR commit-reviewers all at `Finished`.
  - `touched_plan` already exists on the commit event
    (`crates/core/src/repo_state.rs:121`, used by
    `reviewable_shas` at :101-104). No new plumbing to detect
    plan-doc changes.
- `RepoState::derive_status` (~wait.rs:333) already binds
  `latest_event`; thread its `touched_plan` into the
  `compute_gate` call at wait.rs:338.
- Call sites to update: wait.rs:338 (plan path) + the test
  helpers. The ad-hoc call site (wait.rs:487) passes
  `touched_plan = false` — an ad-hoc commit is not a plan change
  (see Out of scope).

## Resulting state machine

| Condition (on latest reviewable commit) | Gate state | Who wakes |
|---|---|---|
| any reviewer Request-Changes / unmarked | ChangesRequested | master (revise) |
| commit-reviewers not all positive | Unreviewed | commit reviewers |
| commit-reviewers positive, commit is **not** a milestone (no plan change, not finished) | **Approved** | master continues — gate reviewer NOT woken |
| commit-reviewers positive, commit **is** a milestone (touched_plan OR finished), gate-reviewers haven't voted | ApprovedPendingGate | gate reviewers |
| commit + gate tiers all positive (≥1 only approve) | Approved | master continues |
| both tiers all Finished | Finished | master finalizes |

Net: gate reviewers fire ONLY at plan-doc-change commits and at
the finish commit — exactly the two milestones the user wants.

## Edge cases to pin with tests

- Pure code commit, commit-reviewer APPROVES → `Approved`, gate
  reviewer gets NO work item. (The core regression this fixes;
  assert `work_for(ruthless, Reviewer)` is empty.)
- Plan-doc commit, commit-reviewer APPROVES → `ApprovedPendingGate`,
  gate reviewer wakes.
- Final commit, commit-reviewer FINISHED → gate reviewer wakes;
  → `Finished` once gate reviewer also Finishes.
- Gate reviewer Request-Changes still trumps everything (unchanged).
- Empty `gate_reviewers`: bypass is a no-op (`Approved` either way).
- **Devolve case** (empty `commit_reviewers`, non-empty
  `gate_reviewers`): "all commit positive" and "all finished"
  are both vacuously true, which would make every commit a
  milestone again. PINNED (ruthless): when `commit_reviewers` is
  empty the FINISHED milestone cannot be signaled, so the
  milestone reduces to `touched_plan` only. Implemented via the
  `!commit_reviewers.is_empty()` guard in `commit_finished`, and
  pinned by `compute_gate_empty_commit_gate_only_wakes_on_plan_doc_not_every_commit`
  (pure-code → Approved, plan-doc → ApprovedPendingGate).

## Limitation: the gate sees only the LATEST reviewable commit

**Prominent, by design (ruthless):** the gate evaluates only the
plan's latest reviewable commit. In the normal commit-then-wait
flow this is fine — master commits the plan-doc change, the gate
goes `Unreviewed`, master waits, the commit reviewer approves,
and the plan-doc commit IS still the latest → the gate reviewer
sees it. The `compute_gate_plan_doc_commit_approved_wakes_gate`
test guarantees this common path.

The gap: if master commits a plan-doc change AND a later code
commit *before* reviews land on the plan-doc commit, the gate
sees only the code commit (not a milestone) and the plan-approval
gate for that plan-doc commit is silently skipped — defeating
milestone (1)'s purpose for that case. This requires master to
commit past its own unreviewed plan-doc change. It is inherent to
the existing single-latest-sha gate, NOT introduced here. The
deeper fix (gate tracks unreviewed plan-doc commits, not just the
latest sha) is out of scope; this limitation is documented here
so it is visible rather than a footnote.

## Out of scope

- The ad-hoc review tier inversion (`wait.rs:594-613` wakes all
  reviewers regardless of tier on planless commits, and never at
  `ApprovedPendingGate`). Separate bug — an ad-hoc commit has no
  plan to "change", so under this rule gate reviewers would
  never fire on ad-hoc unless finished. Track separately.
- A distinct "finalization-only reviewer" concept — not needed;
  the milestone rule on the existing gate tier delivers the
  requested behavior.

## Status

Stub — queued HIGH (lloyd 2026-06-09: gate reviewer `ruthless`
is reviewing every commit; it should fire only at plan approval
and at finish).
