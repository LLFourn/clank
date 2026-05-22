# wfw-finish-notification

## Summary

Wake parked `clank wfw` processes when a plan they were watching
becomes finished. Today the watcher fires, the refold sees the
plan moved into `finished_plans`, `derive_work` returns no
actionable items, and the process keeps blocking forever. From
the agent's perspective the plan never ends.

Fix: introduce a flat tagged `WaitItem` enum carrying actionable
work AND terminal events ("a watched plan finished") in one list.
`wfw` emits whatever non-empty `Vec<WaitItem>` the refold
produces.

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

## Types

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
pub enum MasterNext { Revise, Commit, Finalize }

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

/// Agent-perspective work derivation (same rules as today's
/// `derive_work`). Only emits `Master` and `Reviewer` variants —
/// `Finished` is detected separately by `detect_finished`.
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

1. After the initial fold + plan-filter validation, build the
   `StartupSnapshot` from `initial_state.fold` + the resolved
   filter. Stash it for the lifetime of the run.
2. Each refold:

   ```rust
   let state = refold();
   let mut items = derive_work(&views, &author, role);
   items.extend(detect_finished(&snapshot, &state.fold));
   if items.is_empty() { return Ok(None); }   // keep blocking
   Ok(Some(items))
   ```
3. Remove the current `Some(_) => return Ok(None)` early-exit
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

- `crates/core/src/wait.rs` with `WaitItem`, `Role`, `MasterNext`,
  `StartupSnapshot`, `derive_work`, `detect_finished` + tests.
- Deletes `crates/core/src/work.rs` (its types/function migrate
  into `wait.rs` under the new names).
- Updates `crates/cli/src/cli/wfw.rs` to build the snapshot at
  startup, switch to the combined `derive_work ++ detect_finished`
  flow, and widen `emit` for the new variant.
- Three integration tests for the finished-wake paths.

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

### Core: `derive_work` (regression)

The existing `derive_work` tests survive the WorkItem→WaitItem
rename. Update the `matches!` patterns + variant names but keep
the same coverage matrix (master-flavored / reviewer-eligible /
ineligible).

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

## Acceptance

- A parked `clank wfw` exits 0 with a finished notice after
  `clank finish` lands. Both human and JSON output paths are
  covered.
- `clank wfw --plan <stem>` woken by that same plan finishing
  exits with a finished notice, not a timeout.
- When work and finished both apply on one wake, the emitted
  `items[]` carries both — the mixed-case test catches a
  regression here.
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

- Should `wfw --plan <stem>` against a plan that's ALREADY
  finished at startup fail at startup, or immediately emit a
  finished `WaitItem` and exit 0? Today it'd fail (the
  validation checks `state.fold.plans.contains_key(k)`). Lean:
  keep the startup failure; a plan finished before you started
  watching isn't something `wfw` should pretend it just
  observed. Surfacing as a one-shot notice is defensible too —
  pick at impl time.
