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

1. Enumerate the expected reviewer set as above. The function signature changes to `compute_gate(reviews: &[ReviewEntry], expected_reviewers: &[AgentLabel])` — pass the expected set in. Callers (`derive_status` at `wait.rs:189`, and the two direct call sites at `wait.rs:205` and `wait.rs:262`) need to source this from `RepoState`.
2. For each expected reviewer, look up the feedback file `<repo>/.clank/agents/<label>/feedback/<sha>.md` (already done by the existing `ReviewLookup` trait; the entries arrive in the `reviews` slice).
3. Gate state derives from the *full set*. Verdict semantics: `Approve` = mid-flight signoff (master can continue iterating), `Finished` = "this plan is done" (master can finalize). Rules applied in order:
   - If any review has verdict `RequestChanges` or `Unmarked` → `ChangesRequested`.
   - Else if any expected reviewer has NO entry in `reviews` → `Unreviewed`.
   - Else if EVERY expected reviewer posted `Finished` → `Finished` (master can `clank finish`).
   - Else if every expected reviewer posted `Approve` or `Finished` (and not all `Finished`) → `Approved` (master can continue, but cannot finalize yet because at least one reviewer hasn't said the plan is done).
   - (Unreachable given the checks above.)

Critical contract: a single reviewer posting `Finished` is NOT enough to make the gate `Finished`. ALL expected reviewers must post `Finished`. If two reviewers exist and one says `Finished` while the other says `Approve`, the gate is `Approved` (continue), not `Finished` (finalize). This matches the existing acceptance line that says master cannot `clank finish` until every reviewer has posted FINISHED — codex flagged that my earlier draft violated this by promoting "one Finished + others Approve" to gate `Finished`.

The existing `CommitGateState` enum variants (`Unreviewed`, `Approved`, `Finished`, `ChangesRequested`) are sufficient — no new variants needed. Their *meaning* changes: `Unreviewed` becomes "missing reviewer(s)", `Approved` becomes "every reviewer signed off mid-flight (continue OK)", `Finished` becomes "every reviewer marked done (finalize OK)".

The `waiting_on` projection at `WaitingOn` (separate function — locate during impl) also needs updating to surface which specific reviewers are missing when the gate is `Unreviewed`.

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
- Zero reviewers (master-only repo) → gate decided by master alone (preserve existing single-agent behavior).

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
