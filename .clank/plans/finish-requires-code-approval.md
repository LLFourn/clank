# finish-requires-code-approval

`clank finish` currently treats `gate_state == Approved` as
sufficient readiness, even when the only approved commit is the
plan intro (i.e. plan-only, no implementation). `wfw` already
distinguishes this — its `WaitingOn` routes plan-only-approval
to `MasterToImplement`, not `MasterToFinalize`, and there's a
regression test (`wfw_master_code_only_approval_routes_to_finalize`)
pinning that — but `clank finish` doesn't enforce the same
predicate.

## Fix

In `crates/cli/src/preview.rs`:

1. Track whether the `latest_reviewable_sha`'s underlying
   `PlanTimelineEvent` had `touched_code = true`. Right now
   `latest_reviewable_sha()` returns just the sha; add a
   sibling fn (or change the return to a struct) that also
   reports `touched_code`.
2. Add `FinalizeBlockReason::ImplementationNotApproved` to
   `clank_core::api`. Pushed when `gate_state == Approved` but
   `latest_reviewable_touched_code == false`.
3. `compute_finalize_readiness` takes a `latest_touched_code:
   bool` argument and pushes the new reason in the
   gate-approved-but-no-code case.
4. Map the new reason in `crates/cli/src/cli/finish.rs::reason_to_msg`
   — e.g. "approved commit is plan-only; commit and approve an
   implementation first."
5. The `AlreadyFinished` short-circuit stays unchanged. The
   amend-on-already-finished path stays unchanged. Both have
   their own gating.

## Tests

- Unit test in `preview.rs`: build a state where the only
  approved commit is plan-only, assert
  `FinalizeReadiness::Blocked` with
  `ImplementationNotApproved`.
- Integration test in `crates/cli/tests/finish_amend_integration.rs`
  (or a new `finish_gate_integration.rs`): seed a repo with a
  plan intro + approval, run `clank finish foo`, assert it
  exits non-zero with a message mentioning impl approval.
- Positive case: approve a code-touching commit, assert
  finalize is Ready.

## Out of scope

- Reviewers' UX around "I'm approving the design vs the impl."
  Reviewers already implicitly do this: an approve on a
  plan-only commit signals "design looks good," and an approve
  on a code commit signals "impl looks good." The gate change
  here just stops `finish` from confusing the two — no new
  verdict types.
- The deeper "design approval" / research workflow. Separate
  plan.
