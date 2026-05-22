# wfw-finish-notification

## Summary

Wake parked `clank wfw` processes when a plan they were watching
becomes finished. Today the watcher fires, the refold sees the
plan moved into `finished_plans`, `derive_work` returns no
actionable items, and the process keeps blocking forever. From
the agent's perspective the plan never ends.

Fix: introduce a `WaitOutcome::PlansFinished` terminal event
distinct from `WorkItem`. `wfw` exits 0 either with at least one
work item OR at least one finished notice; only "neither" means
keep blocking.

## Hard Direction

- **Terminal event is not work.** Don't bolt a third variant onto
  `WorkItem` ("you have something to do") to describe a plan
  that's over ("there's nothing more to do here"). Those are
  different semantic categories — split them at the type level.
- **Startup snapshot.** The set of "plans this wfw is watching"
  is captured once, at startup. New plans introduced mid-wait
  still get covered by `derive_work` (it recomputes against
  current state on every refold), but they're not in the
  finished-detection set. This avoids ambiguity about whether a
  watched plan that disappeared "ended" or was simply never
  watched.
- **Work outranks finished.** If on the same wake the projection
  yields both actionable work AND a watched plan transitioned to
  finished, emit the work. The finished notice can land on the
  next loop iteration after the agent acts. (`wfw` only returns
  one round of outcomes per invocation.)
- **No new envelope shape on the wire.** The CLI JSON envelope
  stays `{"items": [...]}`; each item is tagged with `kind` and
  the set of kinds widens to include `finished`. Existing parsers
  that branch on `kind` keep working — they just need a new arm.

## Types

In `crates/core/src/wait.rs` (new module):

```rust
use crate::ids::{CommitSha, PlanKey};
use crate::repo_state::NonEmptyVec;
use crate::work::WorkItem;

/// What one round of `wfw` returns. Either positive ("here's
/// what you should do") or terminal ("these plans you were
/// watching are over"). Both variants are non-empty by
/// construction — the empty case is "no outcome, keep waiting"
/// and is `Option<WaitOutcome>::None` at the call site.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitOutcome {
    Work(NonEmptyVec<WorkItem>),
    PlansFinished(NonEmptyVec<FinishedNotice>),
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FinishedNotice {
    pub plan: PlanKey,
    pub finalized_at: CommitSha,
}
```

`derive_work` keeps its current `Vec<WorkItem>` signature. A new
sibling function in core covers the finished-detection rule:

```rust
/// Compare the wfw-startup snapshot to the current state and
/// return notices for every watched plan that transitioned into
/// `finished_plans` since startup. Plan-key order, stable.
pub fn detect_finished(
    snapshot: &StartupSnapshot,
    current: &RepoState,
) -> Vec<FinishedNotice>;

pub struct StartupSnapshot {
    /// Plan keys this wfw is responsible for. With `--plan foo`,
    /// `{foo}`. Without `--plan`, the active plan keys at startup.
    pub watched: BTreeSet<PlanKey>,
    /// Finalize SHAs already present at startup, keyed by plan.
    /// A watched plan is "newly finished" iff it now has a
    /// `FinishedPlan` entry with a finalized_at NOT present here.
    /// This stays correct across the re-intro / re-finalize
    /// cycle.
    pub finished_at_startup: BTreeMap<PlanKey, BTreeSet<CommitSha>>,
}
```

Pure — no IO, no fold mutation. Lives next to `work.rs` in core.

## CLI Wiring

`crates/cli/src/cli/wfw.rs`:

1. After the initial fold + plan-filter validation, build the
   `StartupSnapshot` from `initial_state.fold`. Stash it for the
   lifetime of the run.
2. Replace the work-only loop check with the prioritized flow:

   ```rust
   let state = refold();
   let work = derive_work(&views, &author, role);
   if let Ok(nev) = NonEmptyVec::new(work) {
       return Ok(Some(WaitOutcome::Work(nev)));
   }
   let finished = detect_finished(&snapshot, &state.fold);
   if let Ok(nev) = NonEmptyVec::new(finished) {
       return Ok(Some(WaitOutcome::PlansFinished(nev)));
   }
   Ok(None) // keep blocking
   ```
3. Remove the current `Some(_) => return Ok(None)` early-exit
   for "filtered plan disappeared mid-watch" — the snapshot-based
   detection handles it correctly.

`emit(outcome, json)` widens to two variants:

- Human: existing per-work-item lines for `WaitOutcome::Work`;
  one `finished  <plan>  <short-sha>` line per notice for
  `WaitOutcome::PlansFinished`.
- JSON: still `{"items": [...]}`. The `kind` discriminator gains
  `finished`; finished entries serialize as
  `{"kind":"finished","plan":"foo","plan_path":".clank/plans/foo.md","finalized_at":"abcd..."}`.

## Sequencing

One commit. Adds:

- `crates/core/src/wait.rs` with `WaitOutcome`, `FinishedNotice`,
  `StartupSnapshot`, `detect_finished` + tests.
- Wires `clank-core::wait` into `crates/cli/src/cli/wfw.rs`:
  builds the snapshot at startup, switches to the prioritized
  outcome flow, widens `emit`.
- Adds three integration tests for the finished-wake path.

The pure-core / IO-CLI boundary stays exactly as it is today.

## Tests

### Core: `detect_finished`

Table-driven against synthetic `StartupSnapshot` + `RepoState`:

- Watched plan moves into `finished_plans` for the first time →
  one notice with the new `finalized_at`.
- Watched plan already in `finished_plans` at startup, same SHA
  in the new state → no notice.
- Watched plan re-finalized at a different SHA (re-intro / re-
  finalize cycle) → one notice with the new SHA.
- Unwatched plan finished → no notice. (Finished-detection is
  scoped to the watched set; new plans don't get this signal.)
- Multiple watched plans finished on the same fold → notices in
  ascending `PlanKey` order.

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

## Acceptance

- A parked `clank wfw` exits 0 with a finished notice after
  `clank finish` lands. Both human and JSON output paths are
  covered.
- `clank wfw --plan <stem>` woken by that same plan finishing
  exits with a finished notice, not a timeout.
- `WorkItem` is unchanged. `WaitOutcome` is the new wait-surface
  type; no caller infers a variant from strings.
- `crates/core/src/wait.rs` has no `std::fs::` /
  `std::process::Command` / `tokio::fs::` calls (grep-verifiable).
- Existing wfw integration tests (reviewer wake on commit, master
  wake on REQUEST_CHANGES, code-only wake, linked-worktree wake)
  still pass.
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
  finished notice and exit 0? Today it'd fail (the validation
  checks `state.fold.plans.contains_key(k)`). Lean: keep the
  startup failure; a plan finished before you started watching
  isn't something `wfw` should pretend it just observed. The
  caller passed stale state. Surfacing as a one-shot notice is
  defensible too — pick at impl time.
