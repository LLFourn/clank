# wfw-finish-notification

## Summary

Two finish-boundary fixes in one plan, since they share the
projection.

1. **Wake parked `wfw` on finalize.** Today the watcher fires
   when a watched plan moves into `finished_plans`, `derive_work`
   returns no actionable items, and the process keeps blocking
   forever. The agent never learns the plan ended.
2. **Distinguish "plan approved" from "impl approved."** Today
   the projection collapses both into `WaitingOn::MasterToFinalize`
   — `clank status` tells the master to run `clank finish` as
   soon as the intro is approved, before any implementation has
   landed. The latest reviewable commit's `touched_code` flag
   already carries the right signal; the projection just
   ignores it.

Fixes share the same `WaitItem` redesign and projection update:

- Introduce a flat tagged `WaitItem` enum carrying actionable
  work AND terminal events ("a watched plan finished") in one
  list. `wfw` emits whatever non-empty `Vec<WaitItem>` the
  refold produces.
- Add `WaitingOn::MasterToImplement` (and a matching
  `MasterNext::Implement` work variant) so the
  approved-plan-only state is distinct from
  approved-with-code → finalize.

## Hard Direction

- **Terminal event is not work.** Don't conflate
  "address this commit" with "this plan is over." Keep them as
  distinct variants of the same flat type so the wfw envelope
  can carry both kinds in one observation.
- **One flat output type.** Replace `WorkItem` with `WaitItem` —
  same `MasterAction` / `ReviewerAction` variants, plus a new
  `Finished` variant. The JSON envelope's `items[]` already wants
  a flat tagged list; the Rust type matches that shape. Nested
  `WaitOutcome { Work(...), Finished(...) }` from earlier drafts
  is rejected: it serializes awkwardly and would force callers
  to special-case priority ordering between work and finished.
- **Work and finished can coexist on one wake.** Two watched
  plans, one transitions to finished, the other has reviewer
  work for our caller — both go out in the same `items[]`.
  Returning only one of them and assuming we'll catch the other
  next time is wrong: the next `wfw` invocation builds a fresh
  startup snapshot AFTER the finalize already settled, so the
  terminal notice would be lost forever. Single observation,
  full set of items.
- **Startup snapshot.** The set of "plans this wfw is watching"
  is captured once, at startup. New plans introduced mid-wait
  still get covered by `derive_work` (it recomputes against
  current state on every refold), but they're not in the
  finished-detection set. This avoids ambiguity about whether a
  watched plan that disappeared "ended" or was simply never
  watched.
- **Implement vs finalize routing.** When the gate is `Approved`
  and the worktree is clean, the projection routes on the
  latest reviewable commit's `touched_code` flag:
  - `touched_code: false` (the approved commit didn't attribute
    code to this plan — it was a plan-only revision) →
    `WaitingOn::MasterToImplement`. The plan was just settled;
    next move is to write code under the `[<stem>]` prefix.
  - `touched_code: true` (the approved commit attributed code
    to this plan, whether or not it also revised the plan file)
    → `WaitingOn::MasterToFinalize`. Implementation has landed
    and been approved; `clank finish` is the next move.
  The user can always ignore the finalize suggestion and commit
  more code; it's a hint, not a gate. The existing
  `Posture::from_commit_kind` / `ReviewTargetPhase` vocabulary
  that classifies "both plan and code touched" as planning is
  scheduled for removal in a separate plan, so this routing
  doesn't try to thread the needle on that classification — it
  reads the raw timeline flags directly. This is the missing
  nuance behind `clank status` currently saying "finalize" the
  moment a plan-only intro is approved.

## Types

In `crates/core/src/plan_view.rs`, extend `WaitingOn` with a new
variant for the approved-plan-only case:

```rust
pub enum WaitingOn {
    FirstReview,
    ReviewerApprovalsMissing { missing: NonEmptyVec<AgentLabel> },
    MasterToRevise { requesters: Vec<AgentLabel>, ambiguous: Vec<AgentLabel> },
    MasterToCommit,
    /// Gate approved, worktree clean, latest reviewable was
    /// plan-only (`touched_code: false`). Next move is the
    /// implementation, not finalize.
    MasterToImplement,
    /// Gate approved, worktree clean, latest reviewable had
    /// `touched_code: true`. Run `clank finish`.
    MasterToFinalize,
}
```

`plan_view::evaluate` updates to route the `Approved + Clean`
branch on the latest reviewable's `touched_code` flag, per the
Hard Direction. `evaluate`'s signature changes to take the
latest `PlanTimelineEvent` (or a `(sha, touched_code)` pair) so
the routing has the data it needs without re-walking the
timeline.

In `crates/core/src/wait.rs` (new module). The existing
`crates/core/src/work.rs` is removed; its `derive_work` moves
here and adopts `WaitItem` as its return type. There's no
`WorkItem` anymore.

```rust
use std::collections::{BTreeMap, BTreeSet};
use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, PlanKey};
use crate::plan_view::{PlanView, WaitingOn};
use crate::repo_state::RepoState;
use crate::vocab::WaitingReason;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role { Master, Reviewers }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterNext {
    /// REQUEST_CHANGES on the latest reviewable. Address + recommit.
    Revise,
    /// Approved gate but the plan file has uncommitted edits. Commit
    /// the next revision (or stash).
    Commit,
    /// Approved plan-only commit. Write the implementation under
    /// the `[<stem>]` commit prefix.
    Implement,
    /// Approved commit with code attribution. Run `clank finish`.
    Finalize,
}

/// Flat tagged list `wfw` returns from one refold round.
/// Actionable items (`Master`, `Reviewer`) and terminal events
/// (`Finished`) sit at the same level. Empty vec at the call
/// site means "no outcome, keep blocking."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitItem {
    Master {
        plan: PlanKey,
        sha: CommitSha,
        next: MasterNext,
        reason: WaitingReason,
    },
    Reviewer {
        plan: PlanKey,
        sha: CommitSha,
        feedback_path: String,
    },
    Finished {
        plan: PlanKey,
        finalized_at: CommitSha,
    },
}

/// Snapshot taken once at `wfw` startup. `detect_finished`
/// compares the current state against this to decide which
/// watched plans newly transitioned to finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupSnapshot {
    /// Plan keys this wfw is responsible for. With `--plan foo`,
    /// `{foo}`. Without `--plan`, the active plan keys at startup.
    pub watched: BTreeSet<PlanKey>,
    /// Finalize SHAs already present at startup, keyed by plan.
    /// A watched plan is "newly finished" iff its current
    /// `FinishedPlan` entry has a `finalized_at` NOT in this set.
    /// The set-per-plan stays correct across re-intro /
    /// re-finalize cycles.
    pub finished_at_startup: BTreeMap<PlanKey, BTreeSet<CommitSha>>,
}

impl StartupSnapshot {
    /// Build from initial fold + the resolved `--plan` filter.
    pub fn from(state: &RepoState, plan_filter: Option<&PlanKey>) -> Self;
}

/// Agent-perspective work derivation. Only emits `Master` and
/// `Reviewer` variants — `Finished` is detected separately by
/// `detect_finished`.
///
/// Master-role mapping (extends the existing table with
/// `MasterToImplement → MasterNext::Implement` /
/// `WaitingReason::ReadyToStartImplementation`):
///
/// | `WaitingOn`              | `MasterNext` | `WaitingReason`               |
/// |-------------------------|--------------|-------------------------------|
/// | `MasterToRevise`        | `Revise`     | `AddressCommitChanges`        |
/// | `MasterToCommit`        | `Commit`     | `CommitPlanRevision`          |
/// | `MasterToImplement`     | `Implement`  | `ReadyToStartImplementation`  |
/// | `MasterToFinalize`      | `Finalize`   | `ReadyToFinalize`             |
pub fn derive_work(
    views: &[PlanView],
    author: &AgentLabel,
    role: Role,
) -> Vec<WaitItem>;

/// Plan-key-ordered `Finished` notices for every watched plan
/// that transitioned into `finished_plans` since startup.
pub fn detect_finished(
    snapshot: &StartupSnapshot,
    current: &RepoState,
) -> Vec<WaitItem>;
```

Pure — no IO, no fold mutation. `WaitingOn`, `PlanView`,
`StartupSnapshot`, `WaitItem`, `MasterNext`, `Role` all live in
`clank-core`. `WaitItem` is the only type the CLI serializes.

## CLI Wiring

`crates/cli/src/cli/wfw.rs`:

1. **Plan-filter resolution.** `--plan <stem>` now accepts a plan
   that's already in `finished_plans` (not just active). The
   startup check becomes: if the resolved key is active OR
   finished, proceed; otherwise error with the standard
   "unknown plan" candidate list (now including finished entries).
   - If the resolved key is active at startup → normal flow.
   - If the resolved key is ONLY in `finished_plans` at startup
     → emit a single `Finished` `WaitItem` carrying that plan's
     latest `finalized_at` and exit 0. No watch loop, no fold
     work. This is the "agent resumed with stale state" path —
     tell them the plan ended rather than block them or fail.
   - Unfiltered `wfw` (no `--plan`) is unchanged: it never
     replays historical finished plans. Finished-detection only
     covers plans that were active when wfw started.
2. After plan-filter resolution, build the `StartupSnapshot` from
   `initial_state.fold` + the resolved filter. Stash it for the
   lifetime of the run.
3. Each refold:

   ```rust
   let state = refold();
   let mut items = derive_work(&views, &author, role);
   items.extend(detect_finished(&snapshot, &state.fold));
   if items.is_empty() { return Ok(None); }   // keep blocking
   Ok(Some(items))
   ```
4. Remove the current `Some(_) => return Ok(None)` early-exit
   for "filtered plan disappeared mid-watch" — the
   snapshot-based detection covers it correctly.

`emit(items, json)` becomes:

- Human: existing per-item lines for `Master` and `Reviewer`;
  one `finished  <plan>  <short-sha>` line per `Finished` item.
- JSON: `{"items": [...]}` with each item serialized through
  `WaitItem`'s `#[serde(tag = "kind")]`. Discriminator widens
  from `master`/`reviewer` to `master`/`reviewer`/`finished`.

## Sequencing

One commit. Adds:

- `crates/core/src/plan_view.rs`: extend `WaitingOn` with
  `MasterToImplement`; rewire `evaluate` to route the
  `Approved + Clean` branch on the latest reviewable's
  `touched_code` flag.
- `crates/core/src/wait.rs` with `WaitItem`, `Role`, `MasterNext`
  (including the new `Implement` variant), `StartupSnapshot`,
  `derive_work`, `detect_finished` + tests.
- Deletes `crates/core/src/work.rs` (its types/function migrate
  into `wait.rs` under the new names).
- Updates `crates/cli/src/cli/wfw.rs` to build the snapshot at
  startup, switch to the combined `derive_work ++ detect_finished`
  flow, and widen `emit` for the new variant + the new
  `MasterNext::Implement` output.
- Updates `crates/cli/src/cli/status.rs` `waiting_actor` /
  `waiting_reason` for the `MasterToImplement` variant
  (`master` actor, "gate approved — implement under [<stem>]"
  reason).
- Integration tests for the finished-wake paths AND the
  plan-only-vs-impl routing.

The pure-core / IO-CLI boundary stays exactly as it is today.

## Tests

### Core: `detect_finished`

Table-driven against synthetic `StartupSnapshot` + `RepoState`:

- Watched plan moves into `finished_plans` for the first time →
  one `Finished` item with the new `finalized_at`.
- Watched plan already in `finished_plans` at startup, same SHA
  in the new state → no item.
- Watched plan re-finalized at a different SHA (re-intro / re-
  finalize cycle) → one `Finished` with the new SHA.
- Unwatched plan finished → no item. (Finished-detection is
  scoped to the watched set; new plans don't get this signal.)
- Multiple watched plans finished on the same fold → items in
  ascending `PlanKey` order.

### Core: `derive_work` (regression + Implement)

The existing `derive_work` tests survive the WorkItem→WaitItem
rename. Update the `matches!` patterns + variant names but keep
the same coverage matrix (master-flavored / reviewer-eligible /
ineligible). Add one new row:

- `WaitingOn::MasterToImplement` → `WaitItem::Master` with
  `next: Implement` and `reason: ReadyToStartImplementation`.

### Core: `plan_view::evaluate` Approved routing

Add three table rows to the existing `evaluate` test suite,
covering each combination of the boolean flags under
Approved+Clean:

- `touched_plan: true, touched_code: false` (plan-only revision),
  Approved, Clean → `MasterToImplement`.
- `touched_plan: true, touched_code: true` (revision + code in
  one commit), Approved, Clean → `MasterToFinalize`. Code
  attribution wins — once code lands and is approved, finalize
  is on the table.
- `touched_plan: false, touched_code: true` (pure impl
  attribution), Approved, Clean → `MasterToFinalize`.

Keep the existing `MasterToCommit` (BodyDirty) and
`MasterToRevise` (ChangesRequested) rows unchanged.

### Integration: `crates/cli/tests/wfw_integration.rs`

1. **Reviewer finish wake (human output).** Temp repo, alice
   approved the intro (no reviewer work for her). Park
   `clank wfw --author alice --role reviewers --timeout 30s`.
   Run `clank finish` from another process. Assert exit 0 +
   stdout contains `finished` and the plan stem.
2. **Reviewer finish wake (JSON).** Same setup with `-j`. Parse
   stdout; assert `items[0].kind == "finished"`,
   `items[0].plan == "foo"`, `items[0].finalized_at` matches
   HEAD post-finalize.
3. **`--plan` filter finish wake.** Same scenario but `wfw`
   invoked with `--plan foo`. Assert the same finished outcome.
   Pins the snapshot-based detection in place of the deleted
   "filtered plan vanished → silent no-op" branch.
4. **Mixed: work + finished on one wake.** Two active plans `a`
   and `b` at startup. Alice has approved the intros on both
   (no reviewer work for her). Park
   `clank wfw --author alice --role reviewers --timeout 30s`.
   Land a new reviewable commit on `a` AND `clank finish` on
   `b`, both before debouncing settles. Assert `items[]` carries
   BOTH a `Reviewer` entry for the new sha on `a` and a
   `Finished` entry for `b` — the finished notice must not be
   dropped under the work item. This is the regression that
   would have hidden under an earlier "work outranks finished"
   priority rule.
5. **`--plan` against an already-finished plan.** Create and
   finalize plan `foo`. Then run
   `clank wfw --plan foo --author alice --role reviewers --timeout 1s`.
   Assert exit 0 within ~1s (no watch loop), stdout contains
   `finished` and `foo`. Repeat with `-j`: assert
   `items[0].kind == "finished"`, `items[0].plan == "foo"`,
   `items[0].finalized_at` matches the finalize commit's SHA.
   This pins the "agent resumed with stale state" path: an
   explicit `--plan` targets a known plan, and `wfw` should
   tell the caller it's over rather than fail or block.
6. **Plan-only approval routes to Implement.** Temp repo with
   a `[foo] intro` commit (`touched_plan: true, touched_code:
   false`). alice approves the intro. Park
   `clank wfw --author lloyd --role master --timeout 5s`.
   Assert it exits within ~1s with stdout containing
   `next=Implement` and `reason=ready_to_start_implementation` —
   NOT `next=Finalize`. Repeat the same scenario adding a
   `[foo] code work` commit (touches `src/lib.rs`) after the
   plan approval but before parking wfw, and alice approving
   that too; assert wfw exits with `next=Finalize` /
   `reason=ready_to_finalize`. The two cases pin the
   plan-only-vs-impl routing fix.

## Acceptance

- A parked `clank wfw` exits 0 with a finished notice after
  `clank finish` lands. Both human and JSON output paths are
  covered.
- `clank wfw --plan <stem>` woken by that same plan finishing
  exits with a finished notice, not a timeout.
- `clank wfw --plan <stem>` against a plan that's already in
  `finished_plans` at startup exits 0 immediately with a single
  `Finished` item, not an error. Scoped to explicit `--plan` —
  unfiltered `wfw` never replays historical finished plans.
- When work and finished both apply on one wake, the emitted
  `items[]` carries both — the mixed-case test catches a
  regression here.
- After a reviewer approves a plan-only commit, `clank status`
  reads "implement under [<stem>]" (not "run clank finish") and
  `clank wfw --role master` emits `next=Implement`. After a
  reviewer approves a code-touching commit, both surfaces flip
  to the finalize wording / `next=Finalize`.
- `WaitItem` is the single outward type. No caller infers a
  variant from strings; no nested `WaitOutcome` wrapper.
- `crates/core/src/wait.rs` has no `std::fs::` /
  `std::process::Command` / `tokio::fs::` calls (grep-verifiable).
- Existing wfw integration tests (reviewer wake on commit, master
  wake on REQUEST_CHANGES, code-only wake, linked-worktree wake)
  still pass after the WorkItem→WaitItem rename.
- `cargo fmt --check` clean; `cargo test --workspace` green.

## Out of Scope

- **Plan deletion** (`TouchKind::Delete`). A watched plan that
  disappears via Delete rather than Finalize still keeps the
  process blocked. Separate plan if we want to surface deletes
  as a third terminal kind.
- **New plans introduced mid-wait.** They get covered by
  `derive_work` automatically; finished notices don't extend to
  them by design.
- **NDJSON streaming.** `wfw` still returns one round of
  outcomes per invocation and exits.
- **Hooks / external notification channels.** The signal is the
  process exit + stdout. The agent loop reads it and decides
  what to do next.

## Open Questions

None outstanding. The "`--plan` against an already-finished plan"
question is resolved in the CLI Wiring section (emit a one-shot
`Finished` and exit 0, scoped to explicit `--plan`).
