# all-reviewers-gate
# Master not woken or finalized until every registered reviewer signs off

## Problem

Today the gate logic treats reviewer feedback as "any one APPROVE/FINISHED is enough". With one reviewer (the historical default) that was fine. With two or more reviewers (codex + ruthless as we now run), the gate fires early:

> We hit this live in `fix-diff-git-header-path-spaces`:
> - Master committed at `fa0f7d5`.
> - Ruthless posted APPROVE.
> - Codex hadn't reviewed yet.
> - Gate said `gate_approved`.
> - Master got `next=continue (gate_approved)` from `clank wfw`.
> - Codex's later substantive REQUEST_CHANGES would have caught a real bug, but master had already moved on under the premature signal.

The bug shape: `clank wfw` for the master returns `gate_approved` when ANY reviewer has approved, instead of waiting for ALL registered reviewers. Same for `clank finish` eligibility.

This plan fixes only the gate-computation half. The broader agent-management work (schema migrations, `clank agent add/remove/list` CLI, user-scope defaults, doctor checks) is intentionally out of scope and stays in the queued `manage-clank-agents` plan.

## Scope discipline

This is a **small, focused** plan. Resist the urge to tackle the schema refactor in the same change — the gate fix is independently valuable, can land in one or two commits, and unblocks the live workflow today.

## Approach

### Source of truth for reviewers (no schema change)

Clank already enumerates registered agents by walking `<repo>/.clank/agents/<label>/config.json` files. Each carries a `role` field (`master` / `reviewers`). The set of *expected reviewers* is:

```
{ label | <repo>/.clank/agents/<label>/config.json exists AND role == "reviewers" }
```

This is the existing enumeration logic — no new config field, no migration. The agent-management plan will later replace this with an explicit list in repo config, but until then the directory scan is the source of truth.

### Gate computation change

The function is `compute_gate(reviews: &[ReviewEntry]) -> CommitGateState` at `crates/core/src/wait.rs:168`. Today's logic:

```rust
let has_changes = reviews.iter().any(|r| r.verdict == RequestChanges || ... );
if has_changes { return ChangesRequested; }
let has_finished = reviews.iter().any(|r| r.verdict == Finished);
if has_finished { return Finished; }
let has_approve = reviews.iter().any(|r| r.verdict == Approve);
if has_approve { Approved } else { Unreviewed }
```

The `any()` calls are the bug. Change to all-of-expected-reviewers semantics:

For the latest reviewable SHA on an active plan:

1. **Threading the expected reviewer list into core (sans-IO).** `RepoState` is sans-IO and intentionally doesn't read `.clank/agents/`. Agent enumeration lives in the CLI filesystem layer (`agent_store::load_all_agent_configs` at `crates/cli/src/agent_store.rs:48`). So the reviewer list must be threaded *into* core via an existing sans-IO surface, not loaded from inside core.

   **Chosen path: extend `WorkPolicy`** at `crates/core/src/wait.rs:142` with a new field:
   ```rust
   pub struct WorkPolicy {
       pub plan_feedback: bool,
       pub adhoc_feedback: bool,
       pub expected_reviewers: Vec<AgentLabel>,
   }
   ```
   `WorkPolicy` is already where the CLI passes config-derived flags into core, and is already threaded through `derive_status(&self, reviews, policy)`. Adding the reviewer list here keeps the sans-IO boundary intact.

   Rejected alternative: extending `ReviewLookup` (codex's other suggestion). The trait's contract is "give me reviews for this SHA"; loading agent configs is a separate IO concern.

2. **`compute_gate` signature changes** to accept the expected reviewer list:
   ```rust
   pub fn compute_gate(
       reviews: &[ReviewEntry],
       expected_reviewers: &[AgentLabel],
   ) -> CommitGateState
   ```
   Both internal call sites in `derive_status` (lines 205, 262) pass `&policy.expected_reviewers`.
3. **CLI-side reviewer enumeration (callers of `derive_status`).** Every call site of `derive_status` in clank-cli must populate `WorkPolicy.expected_reviewers` before invoking. Sites to update:
   - `crates/cli/src/cli/status.rs:63` (`clank status`)
   - `crates/cli/src/cli/open.rs:696` (`clank open`)
   - `crates/cli/src/cli/wfw.rs:191`, `:305` (`clank wfw`)
   - `crates/cli/src/preview.rs:80` (`clank finish` preview pipeline)

   Additionally, `compute_finalize_readiness` at `crates/cli/src/preview.rs:401` must also accept the expected reviewer set so master-only repos can finalize. Current code rejects unless `gate_state == Finished`; with the zero-reviewer rule returning `Approved`, a master-only repo could never `clank finish` without this change. Extend the signature:
   ```rust
   pub fn compute_finalize_readiness(
       is_finished: bool,
       latest_reviewable_sha: Option<&CommitSha>,
       gate_state: CommitGateState,
       worktree_status: PlanWorktreeStatus,
       expected_reviewers: &[AgentLabel],   // NEW
   ) -> FinalizeReadiness
   ```
   And change the gate-check at line 414 to: `if gate_state != CommitGateState::Finished && !expected_reviewers.is_empty()` — i.e. for a zero-reviewer repo, `Approved` is sufficient for finalize. The three call sites of `compute_finalize_readiness` in `preview.rs` (lines 82, 541, 563) get the same reviewer-enumeration treatment as the `derive_status` callers above.

   Each call site does:
   ```rust
   use clank_core::vocab::Role;
   let expected_reviewers = agent_store::load_all_agent_configs(repo)?
       .into_iter()
       .filter(|(_, cfg)| cfg.role == Role::Reviewers)
       .map(|(label, _)| label)
       .collect();
   let policy = WorkPolicy { plan_feedback, adhoc_feedback, expected_reviewers };
   ```
   (`Role::Reviewers` is the actual enum variant from `clank_core::vocab`.)
   This is the existing enumeration mechanism; we're just feeding its output into the policy struct.

4. For each expected reviewer, look up the feedback file (already done by the existing `ReviewLookup` trait; the entries arrive in the `reviews` slice).

5. **Filter reviews to expected reviewers FIRST.** `ReviewLookup::reviews_for(sha)` (the CLI impl `FsReviewLookup` scans `<repo>/.clank/agents/*/feedback/<sha>.md`) can return entries from author labels whose `.clank/agents/<label>/config.json` no longer marks them as reviewers — orphan dirs from removed reviewers, agents whose role flipped from `Reviewers` to `Master`, etc. Without filtering, a stale `REQUEST_CHANGES` from such an author would gate `ChangesRequested` even though they aren't expected to gate anything.

   `compute_gate` (and the matching `WaitingOn` projection in `derive_status`) MUST start by filtering reviews to authors in `expected_reviewers`:
   ```rust
   let expected: HashSet<&AgentLabel> = expected_reviewers.iter().collect();
   let reviews: Vec<&ReviewEntry> = reviews
       .iter()
       .filter(|r| expected.contains(&r.author))
       .collect();
   ```
   All subsequent precedence rules operate on this filtered set. The same filter applies to the `WaitingOn::MasterToRevise { requesters, ambiguous }` payload built in `derive_status` at lines 222-235 — both lists are sourced from the same review set and must be filtered too.

   With this filter in place, "stale REQUEST_CHANGES from a removed reviewer doesn't gate" becomes a uniform principle rather than a zero-reviewer-special-case.

6. Gate state then derives from the *filtered* set. Verdict semantics: `Approve` = mid-flight signoff (master can continue iterating), `Finished` = "this plan is done" (master can finalize). Rules applied in order:
   - **Zero-reviewer case first**: if `expected_reviewers` is empty → `Approved`. Rationale: a master-only repo (no registered reviewers) has nothing to wait for; every commit is auto-approved for continuation. `compute_finalize_readiness` (see step 3) treats this as ready-to-finalize. Without this special case, the "every expected reviewer posted Finished" rule below is *vacuously true* with an empty set and would incorrectly fire `Finished`.
   - If any *filtered* review has verdict `RequestChanges` or `Unmarked` → `ChangesRequested`.
   - Else if any expected reviewer has NO entry in the *filtered* reviews → `Unreviewed`.
   - Else if EVERY expected reviewer posted `Finished` → `Finished` (master can `clank finish`).
   - Else if every expected reviewer posted `Approve` or `Finished` (and not all `Finished`) → `Approved` (master can continue, but cannot finalize yet because at least one reviewer hasn't said the plan is done).
   - (Unreachable given the checks above.)

Critical contract: a single reviewer posting `Finished` is NOT enough to make the gate `Finished`. ALL expected reviewers must post `Finished`. If two reviewers exist and one says `Finished` while the other says `Approve`, the gate is `Approved` (continue), not `Finished` (finalize). This matches the existing acceptance line that says master cannot `clank finish` until every reviewer has posted FINISHED — codex flagged that my earlier draft violated this by promoting "one Finished + others Approve" to gate `Finished`.

The existing `CommitGateState` enum variants (`Unreviewed`, `Approved`, `Finished`, `ChangesRequested`) are sufficient — no new variants needed. Their *meaning* changes: `Unreviewed` becomes "missing reviewer(s)", `Approved` becomes "every reviewer signed off mid-flight (continue OK)", `Finished` becomes "every reviewer marked done (finalize OK)".

The `waiting_on` projection in `derive_status` (lines 220-246) currently maps `Unreviewed` → `WaitingOn::FirstReview`. With multi-reviewer semantics, this needs to surface *which* reviewers are missing. Change `WaitingOn::FirstReview` to carry the missing reviewer set (e.g., `FirstReview { missing: Vec<AgentLabel> }`), OR introduce a sibling variant `AwaitingReviewers { missing: Vec<AgentLabel> }` and reserve `FirstReview` for the "zero reviews yet" case. Pick during implementation based on which variant's existing consumers would break least.

### `clank wfw` for master

`clank wfw` invoked by master returns `gate_approved` ONLY when the gate state from above is `approved` or `ready_to_finalize`. While reviewers are still owed (`awaiting_reviewers`), master's `wfw` keeps blocking with the reviewer-missing waiting_on payload.

This is the load-bearing user-facing change: master's stop-hook continuation prompt fires only after the full reviewer set is in.

### Stop hook for reviewers — non-redundant wake (optional polish)

When a reviewer's stop hook fires, check: have all peers including me already posted APPROVE/FINISHED for the latest reviewable SHA? If yes, don't fire continuation work — there's nothing to review (master will move on after the final reviewer's verdict lands).

This is symmetric to the master fix: don't redundantly wake a reviewer who has already done their job. It's polish, not load-bearing — if it complicates the implementation, defer.

### Out of scope (explicitly)

- Schema migration to a `agents` array in repo config.
- `clank agent add/remove/list/set-role` CLI commands.
- User-scope `default_agents` in `~/.clank/config.json`.
- `clank init` seeding from user-scope config.
- `clank doctor` checks for unbound agents.
- Any change to `clank as` / `clank auto` semantics.

All of these stay in the queued `manage-clank-agents` plan as the broader config refactor.

## Tests

In `clank-core` (gate projection unit tests):

- Single reviewer, APPROVE → `approved`.
- Single reviewer, REQUEST_CHANGES → `changes_requested`.
- Two reviewers, both APPROVE → `approved`.
- Two reviewers, one APPROVE one REQUEST_CHANGES → `changes_requested`.
- Two reviewers, one APPROVE one missing-feedback → `awaiting_reviewers` (or current equivalent), `waiting_on: { reviewers: [<missing>] }`.
- Two reviewers, both FINISHED → `Finished` (master can `clank finish`).
- Two reviewers, one FINISHED one APPROVE → `Approved` (master can continue but NOT finalize — only one reviewer has signed off as done; the other still treats it as mid-flight).
- Zero reviewers + any review entries → `Approved` (the zero-reviewer rule fires before the other checks; master is unblocked). Includes the case where `reviews` is empty AND `expected_reviewers` is empty.
- Zero reviewers + a stale REQUEST_CHANGES from a removed reviewer → `Approved` (the removed reviewer is no longer in `expected_reviewers`, so their entry shouldn't gate. Validates that the zero-reviewer rule fires first.)
- Two expected reviewers (codex, ruthless) both APPROVE + a stale REQUEST_CHANGES from a removed `alice` author → `Approved` (the filter drops alice's entry before the precedence rules; same principle as the zero-reviewer case but in non-zero mode).
- Two expected reviewers, one APPROVE one missing + a stale FINISHED from a removed author → `Unreviewed` (filter drops the stale entry; the still-pending expected reviewer is what gates).
- `MasterToRevise.requesters` payload test: two expected reviewers REQUEST_CHANGES + a removed-author REQUEST_CHANGES — the payload contains only the two expected authors, not the removed one.

For `compute_finalize_readiness` (in `crates/cli/src/preview.rs`):
- Zero reviewers + gate `Approved` → ready to finalize (the new `!expected_reviewers.is_empty()` guard skips the `gate != Finished` rejection).
- Non-zero reviewers + gate `Approved` → still blocked with `NotFinished { state: Approved }` (unchanged).
- Non-zero reviewers + gate `Finished` → ready to finalize (unchanged).

In `clank-cli` (`clank wfw` integration):

- Multi-reviewer setup, only some reviewers approved → master's `wfw` blocks with `waiting_on: { reviewers: [<missing>] }`.
- Multi-reviewer setup, all reviewers signed off → master's `wfw` returns `gate_approved`.

Regression:

- Existing single-reviewer-per-repo configurations continue to work without re-init.

## Acceptance

- A multi-reviewer plan cannot reach `gate_approved` until every registered reviewer has posted APPROVE or FINISHED for the current SHA.
- Master cannot `clank finish` until every reviewer has posted FINISHED.
- The bug from `fix-diff-git-header-path-spaces` (master woken after one reviewer approved while another hadn't reviewed) does not reproduce.
- All new tests pass; existing tests still pass (`cargo test --workspace`).
- `clank doctor` continues to pass on this repo (no schema changes that would invalidate existing config).

## Related

- `manage-clank-agents` (queued): the broader config refactor and CLI surface. This plan is a prerequisite-equivalent piece extracted for focus; the two are largely independent.
- `fix-diff-git-header-path-spaces` (finished 8036ce7): the live case that demonstrated the premature-wake bug.
