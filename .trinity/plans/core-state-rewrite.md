# core-state-rewrite

## Summary

The trinity-core fold state has accumulated structural decay
that the previous plan (`core-model-invalid-states-unrepresentable`)
tried to fix in flight. Mid-stream it kept widening — bucket
enums → flat facts → active plans only → drop the commit
registry → drop CommitNode from state. None of that is a
"making invalid states unrepresentable" change anymore; it's
a rewrite of the fold's data model.

This plan does the rewrite cleanly, from scratch, with one
declarative center. After it lands:

- Fold + state types live in `crates/trinity-core/src/repo_state.rs`.
  Sans-io.
- `RepoState` carries only active workflow facts. Finished
  plans, the global commit registry, the global commit order
  list, and `plan_conflicts` are all gone — `git log` is the
  system of record for "what happened in this repo."
- The fold has no separate `FoldCarry`. `apply_commit` is a
  single-arg method.
- The cache stores the entire `RepoState`. Warm rebuilds load
  the cached state at an ancestor SHA and apply just the new
  commits — no re-walk from the root commit.
- A plan's stored data is its per-commit timeline of small
  per-(plan, commit) booleans. Body, intro SHA, last
  activity, lifecycle stage, participants — all derived from
  the timeline + `git_io` + feedback files.

The previous plan (`core-model-invalid-states-unrepresentable`)
is superseded. Its Phase 1 + Phase 2 commits stay (they
landed useful cleanup — deleted bucket enums, introduced the
classifier + AttributionWarning enum). What didn't land
(Phase 3, atomic fold swap) is what this plan now executes,
with the smaller and more honest scope.

## Hard Direction

- `RepoState` is `{ plans, ad_hoc, current_attribution,
  finalize_files }`. Nothing else.
- `state.plans` holds only active plans. Finalize and
  non-frozen Delete REMOVE the plan from the map.
- `PlanState` is `{ commits: Vec<PlanTimelineEvent> }`. No
  body, no body_hash, no plan_intro, no plan_intro_parent,
  no last_activity_ts, no archived_cycles, no
  latest_revision / latest_implementation, no
  participants_cumulative, no stage. All derived at
  projection time.
- `PlanTimelineEvent` is `{ sha, ts, touched_plan,
  touched_code, finished }`. Five fields.
- `state.ad_hoc: Vec<AdHocEvent>`. Cleared on the next plan
  intro.
- `apply_commit(&mut self, event: &CommitEvent)`. One arg.
- The fold module is sans-io. No git, no filesystem, no
  async.
- `CommitNode` (the rich projection view) is built on
  demand by the projection layer from `PlanTimelineEvent`
  + `git_io` + feedback file scans. Never stored.
- Cache supports incremental fold via a cache file whose
  header records the SHA the state was saved at.
- No `FoldCarry`, no `BaseRepoState` / `LiveRepoState`
  wrapper distinction (live overlays are applied on top of
  the loaded `RepoState` by daemon code, not by a separate
  type).

## Core State

The complete inventory of state types lives in
`crates/trinity-core/src/repo_state.rs`. Every struct + enum
below has exactly one declaration; the rest of this plan
refers back here.

```rust
/// Top-level fold output. The cache stores this directly.
pub struct RepoState {
    /// Active plans only. A plan exits the map on finalize
    /// or non-frozen Delete. `git log` is the system of
    /// record for finished plans.
    pub plans: BTreeMap<PlanKey, PlanState>,

    /// Out-of-plan commits in fold order. Cleared on the
    /// next plan intro.
    pub ad_hoc: Vec<AdHocEvent>,

    /// Walk-back attribution carry. The chain the
    /// classifier will use for the next commit's
    /// `plan_attribution` inference if the title is bare.
    pub current_attribution: Option<PlanKey>,

    /// Per-active-plan approving-files map, mirrored from
    /// `.trinity/finished/<plan>/<author>.md`. The freeze
    /// rule fires when an approving file landing in a
    /// commit's tree crosses the configured threshold.
    /// Inner map: author filename → file body.
    pub finalize_files: BTreeMap<PlanKey, BTreeMap<String, String>>,
}

/// One active plan. Just its timeline. All derived facts
/// (body, intro SHA, last activity, stage,
/// participants_cumulative, latest_revision,
/// latest_implementation) are projections.
pub struct PlanState {
    pub commits: Vec<PlanTimelineEvent>,
}

/// One entry in a plan's timeline: how a single commit
/// affected this plan. Multi-plan commits emit one event
/// per touched plan.
pub struct PlanTimelineEvent {
    pub sha: CommitSha,
    pub ts: i64,
    /// True iff this commit modified
    /// `.trinity/plans/<plan>.md`.
    pub touched_plan: bool,
    /// True iff this commit had non-plan, non-trinity code
    /// changes attributed to this plan (explicit `[plan]`
    /// prefix or walk-back).
    pub touched_code: bool,
    /// True iff this commit's finalize threshold fired for
    /// this plan. Set by the fold immediately before the
    /// plan exits `state.plans`, so a serialized state
    /// never contains `finished: true` events.
    pub finished: bool,
}

/// One entry in `state.ad_hoc`.
pub struct AdHocEvent {
    pub sha: CommitSha,
    pub ts: i64,
    /// True iff this commit had non-plan code changes.
    pub touched_code: bool,
}

/// All facts the fold needs about one commit. Built by
/// the daemon (or any other caller) before calling
/// `state.apply_commit`. Sans-io.
pub struct CommitEvent {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub plan_touches: Vec<PlanTouchInput>,
    pub finalize_changes: Vec<FinalizeFileChange>,
    pub has_code_changes: bool,
}

pub struct PlanTouchInput {
    pub plan: PlanKey,
    pub kind: TouchKind,
    /// New body text for Intro/Revise; None for Delete.
    pub new_body: Option<String>,
}

pub enum TouchKind {
    Intro,
    Revise,
    Delete,
}

pub enum FinalizeFileChange {
    Added { plan: PlanKey, filename: String, body: String },
    Modified { plan: PlanKey, filename: String, body: String },
    Removed { plan: PlanKey, filename: String },
}

/// Classifier inputs: built from a CommitEvent + the fold's
/// current state (carry + known active plans).
pub struct ClassifierInputs<'a> {
    pub subject: &'a str,
    pub touches: &'a BTreeMap<PlanKey, TouchKind>,
    pub finalizes: &'a BTreeSet<PlanKey>,
    pub has_code_changes: bool,
    pub current_attribution: Option<&'a PlanKey>,
    pub known_plans: &'a BTreeSet<PlanKey>,
}

/// Classifier output: per-commit attribution decision +
/// warnings + the walk-back update.
pub struct ClassifierOutput {
    pub plan_attribution: Option<PlanKey>,
    pub warnings: Vec<AttributionWarning>,
    pub next_effective_attribution: Option<PlanKey>,
}

/// Typed attribution warning. Emitted by the classifier
/// but NOT stored in `RepoState` — the projection layer
/// re-classifies the latest commit per plan when it needs
/// to surface warnings.
pub enum AttributionWarning {
    UnknownPlanPrefix { unknown_names: Vec<String> },
    MissingPrefix { suggested_prefix: String },
    AttributionMismatch {
        attributed: Vec<PlanKey>,
        touched: Vec<PlanKey>,
    },
    DanglingPlanRef { plan: PlanKey },
}
```

### Projection types (NOT stored)

These types are used by WFW + response builders to
synthesize "everything about this commit" views from
`PlanTimelineEvent` + `git_io` + feedback scans. They live
in `trinity-core` for sharing across consumers but they
never appear as fields on `RepoState`.

```rust
/// Rich per-commit view, synthesized on demand.
pub struct CommitNode {
    pub sha: CommitSha,
    pub ts: i64,
    pub subject: String,
    pub touches: BTreeMap<PlanKey, TouchKind>,
    pub finalizes: BTreeSet<PlanKey>,
    pub has_code_changes: bool,
    pub plan_attribution: Option<PlanKey>,
    pub warnings: Vec<AttributionWarning>,
    pub reviews: CommitReviews,
}

pub struct CommitReviews {
    pub feedback: BTreeMap<AgentLabel, FeedbackBody>,
}

pub struct FeedbackBody {
    pub verdict: Verdict,
    pub body: String,
    pub created_at: i64,
}

pub enum ReviewPolicy {
    Blocking { participants: NonEmptyVec<AgentLabel> },
    NonBlocking { reason: NonBlockingReason },
}

pub enum NonBlockingReason {
    NoParticipants,
    ConfigDisabledPlanReview,
    ConfigDisabledMiscReview,
    StructurallyNonReviewable,
}

pub struct ReviewReadiness {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
}
```

### Notes on the shape

- **Plan metadata is implicit.** `PlanKey` is the
  `BTreeMap<PlanKey, PlanState>` key. The plan path is
  `.trinity/plans/<key>.md`. The body comes from
  `git show <intro_sha>:.trinity/plans/<key>.md` (or the
  tree at the latest commit with `touched_plan == true`).
- **`participants_cumulative` is a projection.** Scan
  `.trinity/feedback/<key>/*/` at projection time.
- **`last_activity_ts` is a projection.** `max(commits.last().ts,
  max feedback mtime)`.
- **`PlanStage` is a projection.** `Drafting` if every event
  has `touched_code == false`; `Implementing` once any
  event has `touched_code == true`. Frozen plans aren't in
  `state.plans`.
- **`intro` is `commits[0].sha`.** No separate field.
- **`latest_revision` / `latest_implementation` are
  projections** off the timeline.
- **Attribution warnings are not stored.** Projection re-runs
  the classifier on the latest event per plan when needed.
- **Cache shape.** The cache file's header records the HEAD
  SHA the state was saved at. `RepoState` itself doesn't
  carry that field.

## Fold Algorithm

```rust
impl RepoState {
    pub fn apply_commit(&mut self, event: &CommitEvent) {
        // 1. Apply finalize-file mutations. These update
        //    self.finalize_files; threshold crossings
        //    populate `newly_finalizing`.
        let mut newly_finalizing: BTreeSet<PlanKey> = BTreeSet::new();
        for change in &event.finalize_changes {
            apply_finalize_change(
                &mut self.finalize_files,
                &self.plans,
                change,
                &mut newly_finalizing,
            );
        }

        // 2. Classify the commit (computes plan_attribution
        //    + warnings + next_attribution).
        let touches: BTreeMap<PlanKey, TouchKind> = event
            .plan_touches
            .iter()
            .map(|t| (t.plan.clone(), t.kind))
            .collect();
        let known_plans: BTreeSet<PlanKey> =
            self.plans.keys().cloned().collect();
        let classified = classify(ClassifierInputs {
            subject: &event.subject,
            touches: &touches,
            finalizes: &newly_finalizing,
            has_code_changes: event.has_code_changes,
            current_attribution: self.current_attribution.as_ref(),
            known_plans: &known_plans,
        });

        // 3. Apply touches.
        //    - Intro inserts a new PlanState AND clears
        //      self.ad_hoc.
        //    - Revise / Delete operate on existing plans.
        for touch in &event.plan_touches {
            match touch.kind {
                TouchKind::Intro => {
                    self.ad_hoc.clear();
                    self.plans.insert(
                        touch.plan.clone(),
                        PlanState { commits: Vec::new() },
                    );
                }
                TouchKind::Revise => { /* nothing structural to do */ }
                TouchKind::Delete => {
                    self.plans.remove(&touch.plan);
                    self.finalize_files.remove(&touch.plan);
                }
            }
        }

        // 4. Build per-plan event for every touched plan.
        for touch in &event.plan_touches {
            if let Some(ps) = self.plans.get_mut(&touch.plan) {
                ps.commits.push(PlanTimelineEvent {
                    sha: event.sha.clone(),
                    ts: event.author_ts,
                    touched_plan: true,
                    touched_code: event.has_code_changes
                        && matches!(touch.kind, TouchKind::Revise),
                    finished: false,
                });
            }
        }

        // 5. Code-only attribution: if the commit has code
        //    changes attributed to a plan it didn't touch,
        //    emit a touched_code event on that plan.
        if let Some(plan) = &classified.plan_attribution
            && event.has_code_changes
            && !touches.contains_key(plan)
            && let Some(ps) = self.plans.get_mut(plan)
        {
            ps.commits.push(PlanTimelineEvent {
                sha: event.sha.clone(),
                ts: event.author_ts,
                touched_plan: false,
                touched_code: true,
                finished: false,
            });
        }

        // 6. Apply finalize: emit `finished: true` events
        //    (callers/tests can observe them) THEN remove
        //    the plans from state.
        for plan in &newly_finalizing {
            if let Some(ps) = self.plans.get_mut(plan) {
                ps.commits.push(PlanTimelineEvent {
                    sha: event.sha.clone(),
                    ts: event.author_ts,
                    touched_plan: false,
                    touched_code: false,
                    finished: true,
                });
            }
        }
        for plan in &newly_finalizing {
            self.plans.remove(plan);
            self.finalize_files.remove(plan);
        }

        // 7. Ad-hoc bucket: commits with no plan touch, no
        //    plan attribution, code changes present.
        if touches.is_empty()
            && classified.plan_attribution.is_none()
            && event.has_code_changes
        {
            self.ad_hoc.push(AdHocEvent {
                sha: event.sha.clone(),
                ts: event.author_ts,
                touched_code: true,
            });
        }

        // 8. Update walk-back carry.
        self.current_attribution =
            classified.next_effective_attribution;
    }
}
```

Helper invariants:

- `apply_finalize_change` mutates `self.finalize_files`
  for the named plan + emits the plan into
  `newly_finalizing` when the post-mutation count crosses
  the freeze threshold for a plan currently in
  `state.plans`.
- A commit with `touches = {A: Intro}` clears
  `self.ad_hoc` before doing anything else for A. This is
  the "new plan absorbs/discards prior ad-hoc work" rule.
- A commit with `touches = {A: Delete}` removes A and its
  `finalize_files[A]` entry. If A doesn't exist (already
  finalized), the delete is a no-op.
- Multi-plan commits (`touches.len() ≥ 2`) emit one event
  per touched plan, each with `touched_plan: true`.
- `finished: true` events are emitted before plan removal
  so apply_commit's downstream observers (tests, debug
  prints) see them. They never persist in a saved
  `RepoState`.

## Cache & Incremental Fold

The cache stores `RepoState` directly. The file format:

```
[header: magic + format_version + saved_at_head_sha]
[payload: wincode-encoded RepoState]
```

Rebuild flow:

1. Resolve current HEAD via `git_io::rev_parse_head`.
2. If a cached file exists for HEAD → load + return; no
   fold.
3. Else, attempt to load a cache file whose
   `saved_at_head_sha` is an ancestor of HEAD. (The daemon
   keeps the most recent cached HEAD; if it's not an
   ancestor of the new HEAD, fall back to cold start.)
4. If a usable cache is found:
   - Walk `git_io::commits_between(saved, head)` building
     `CommitEvent`s in fold order.
   - For each event: `state.apply_commit(&event)`.
   - Write the cache at the new HEAD; delete the previous
     cache file in the same write.
5. Otherwise (cold start, or no usable ancestor): walk
   history from the root commit, building `CommitEvent`s
   per commit, applying each.

Cache error handling:

- Any `try_load` error (bad magic, format-version
  mismatch, head mismatch, decode failure) MUST delete
  the offending file before falling back. Today the
  fallback is silent and stale files leak. The new loader
  bakes delete-on-error in.

Validating wrappers (`NonEmptyVec`) need cache encoding
that goes through their validation. wincode-derive
bypasses constructors. Either:

- Gate cache encoding behind a serde-with-TryFrom
  boundary, or
- Hand-write `SchemaRead` impls that call the validating
  constructor.

The new code must not ship with a wincode bypass on
validated types. Phase 1 picks the approach.

## Projection Layer

WFW and response builders synthesize the "everything about
this commit" view on demand. The projection layer lives in
the daemon (`src/responses.rs`, `src/server/wait.rs`,
`src/server/http.rs`, `src/preview.rs`) and consumes both
`RepoState` + live IO.

Projection inputs:

- `&RepoState` — folded state.
- `git_io` reads — commit subjects, diffs, trees (for body
  text).
- Feedback scan — `.trinity/feedback/<scope>/<sha>/<author>.md`
  files.
- Snapshotted `Config` — review-policy settings.

Common queries:

- Plan timeline → `state.plans[plan].commits.iter()`.
- Plan body → `git show <commits[0].sha>:.trinity/plans/<plan>.md`
  (or the tree at the most recent `touched_plan` event).
- Latest reviewable for plan → `state.plans[plan].commits.last()`
  (the event's `touched_plan || touched_code` determines
  what kind of review).
- Plan stage → derived from `commits`
  (Drafting/Implementing).
- Last activity → `max(commits.last().ts, max feedback
  mtime)`.
- Participants cumulative → distinct authors across
  feedback files under `.trinity/feedback/<plan>/`.
- Per-commit warnings → re-run `classify(...)` on the
  classifier inputs the projection rebuilds from the
  commit's facts (re-fetched via `git_io` for the commits
  the UI is currently displaying).

`review_policy` / `readiness` are free functions:

```rust
pub fn review_policy(
    event: &PlanTimelineEvent,
    plan_state: Option<&PlanState>,
    ad_hoc_participants: &[AgentLabel],
    feedback: &CommitReviews,
    config: &Config,
) -> ReviewPolicy;

pub fn readiness(/* same inputs */) -> ReviewReadiness;
```

The projection layer is daemon-side because it needs IO.
The core fold is sans-io.

## Implementation Phases

Phase 0 was the prior plan's Phase 1 + Phase 2 (deleted
bucket types + added flat-facts scaffolding). That work
landed; this plan reuses what's salvageable
(`AttributionWarning` typed enum, the classifier's logic
shape) and replaces the rest.

### Phase 1 — Move + restructure (atomic)

ONE atomic commit (or PR landing as one squashed commit).

- Create `crates/trinity-core/src/repo_state.rs` with the
  Core State types from above.
- Move the classifier (`classify`, `ClassifierInputs`,
  `parse_title_prefix`) from `model::facts` into
  `repo_state`. Reshape `ClassifierOutput` to `{
  plan_attribution, warnings, next_effective_attribution }`
  (drop the bundled `CommitFacts`; the fold passes the
  facts directly into the events it emits).
- Implement `RepoState::apply_commit(&mut self,
  event: &CommitEvent)` per the Fold Algorithm above.
- Delete from `trinity-core::model::facts`:
  - `CommitNode` (legacy facts shape) — replaced by the
    projection-only `CommitNode` in `repo_state`.
  - `CommitFacts` — folded into apply_commit's local
    variables.
  - `PlanState` (Phase 2's shape with timeline, intro,
    latest_revision, etc.) — replaced by the minimal
    `PlanState`.
  - `AdHocState` — replaced by `Vec<AdHocEvent>`.
  - `PlanStage` enum — projection only; defined in
    `repo_state` if a UI consumer needs it.
- Delete `ReviewScope` enum and revert `CommitReviews` to
  `BTreeMap<AgentLabel, FeedbackBody>` (scope is implicit
  from container in the projection layer).
- Delete from `trinity-core::model`:
  - Legacy `CommitNode` (`kind`, `attribution`, `plans`,
    `gate`, `attribution_warning`).
  - Legacy `Plan` (`timeline: Vec<PlanTimelineEvent>`,
    `last_activity_ts`, `plan_intro_parent`,
    `archived_cycles`, `body_hash`, `body`, `lifecycle`,
    `plan_intro`).
  - `model::PlanTimelineEvent` (the legacy enum-of-rows;
    the new `PlanTimelineEvent` struct lives in
    `repo_state`).
  - `Feedback`, `CommitGate` (legacy review storage).
  - `plan_conflicts` field + `PlanConflict` API type.
- Daemon-side `src/repo_state.rs` becomes a re-export
  shim or is deleted outright (consumers import from
  `trinity_core::repo_state`).
- Daemon-side `src/disk_snapshot.rs` shrinks to its IO
  role: walk git history → `Vec<CommitEvent>`, call
  `state.apply_commit(&event)` per event. The post-fold
  feedback-attach pass (current `attach_live_feedback`)
  becomes a projection-layer concern, not part of fold
  state.
- Rewrite WFW (`src/server/wait.rs`) and response
  projections (`src/responses.rs`, `src/server/http.rs`,
  `src/preview.rs`) to read `state.plans[plan].commits`
  directly and call projection functions for body /
  participants / readiness.
- Preserve existing API shapes (`CommitRow`, `DiffLine`,
  `TimelineEvent`); they're tagged enums built by
  projection.
- Bump `state_cache::CACHE_FORMAT_VERSION` (v3 → v4).
  - v4 stores the new `RepoState` shape. Old caches
    invalidate.
  - Bake delete-on-error into the loader.
- The cache continues to work in cold-start mode at the
  end of this phase. Incremental rebuild lands in Phase 2.

### Phase 2 — Incremental cache loading (additive)

- Change the cache file format to include a header with
  `saved_at_head_sha`.
- Rebuild logic: try to load the cache; if its
  `saved_at_head_sha` is an ancestor of current HEAD,
  walk `git_io::commits_between(saved, head)`, build
  `CommitEvent`s, call `apply_commit` for each, write the
  new cache.
- Cold start (no usable cache) still walks from the root
  commit.
- Tests:
  - Apply N commits via `apply_commit`, snapshot
    `RepoState`. Apply one more via `apply_commit`. The
    result equals a from-root re-fold over all N+1
    commits.
  - Cache load at ancestor SHA + fold-forward equals the
    cold-start equivalent.

### Phase 3 — Cleanup

- Remove `CommitAttribution` and any other stale helpers
  left behind from the legacy shape.
- Delete the now-superseded prior plan file
  `.trinity/plans/core-model-invalid-states-unrepresentable.md`
  (if it hasn't been finalized already by Trinity).
- Retire `.trinity/plans/tagged-enums-for-non-orthogonal-fields.md`
  — its intent is preserved for API/projection types in
  this plan's Projection Layer section; the core-model
  intent is fully absorbed.

## Tests

### Unit (trinity-core)

- Classifier: every attribution path covered (already
  landed in Phase 0). Tests stay green after the move to
  `repo_state`.
- Fold invariants:
  - `apply_commit` with `touches = {A: Intro}` inserts
    `state.plans[A]` with `commits = [event]` (touched_plan
    true, touched_code false, finished false). Also
    clears `state.ad_hoc`.
  - `touches = {A: Revise}` after intro appends an event
    with `touched_plan: true`.
  - `touches = {A: Delete}` on an active plan removes it
    + its `finalize_files[A]` entry.
  - `finalizes = {A}` (from threshold crossing) appends a
    `finished: true` event then removes A.
  - `finalizes = {A}, touches = {B: Revise}`: A exits
    state with its finished event having appeared; B's
    commits gains a touched_plan event.
  - Multi-plan touch: `touches = {A: Revise, B: Revise}`
    appends one event to each.
  - Code-only attribution: `plan_attribution = Some(A),
    has_code_changes = true, touches = ∅` appends a
    `touched_code: true` event to A's commits.
  - Ad-hoc eligible commit appends to `state.ad_hoc`.
  - Intro after ad-hoc commits clears `state.ad_hoc`.
- Incremental-fold equivalence: snapshot `RepoState`
  after N commits; apply commit N+1 via `apply_commit`.
  Result equals a from-root re-fold over all N+1
  commits.

### Acceptance grep (CI)

- `grep -rn "pub kind: " crates/trinity-core/src/` returns
  no canonical-struct hits.
- `grep -rn "pub gate: " crates/trinity-core/src/` returns
  no hits.
- `grep -rn "FoldCarry" crates/ src/` returns no hits.
- `grep -rn "commit_order\|plan_conflicts" crates/trinity-core/src/repo_state.rs`
  returns no hits.
- `grep -rn "model::Plan\b\|model::CommitNode\b\|model::Feedback\b\|model::CommitGate\b\|model::PlanTimelineEvent"
  crates/ src/` returns no hits.

### Regression

- Plan-scoped WFW finds the right candidate for plans
  going through revise → implement → revise → implement.
- Multi-plan commits surface review work for each touched
  plan.
- Finalized plans surface no review work (absent from
  `state.plans`).
- Late feedback on a finalized plan does not produce WFW
  work (the plan isn't in state).
- Repo-scoped WFW still surfaces ad-hoc-eligible commits
  per config.
- Rendered plan-page timeline matches today's output on
  golden-file fixtures.

### CI

- `cargo test --workspace --exclude trinity-frontend`.
- `cargo test -p trinity-frontend` if frontend types
  change.
- `cargo fmt -- --check`.

## Acceptance

- `RepoState` is `{ plans, ad_hoc, current_attribution,
  finalize_files }`. Lives in
  `crates/trinity-core/src/repo_state.rs`.
- `PlanState` is `{ commits: Vec<PlanTimelineEvent> }`.
  No body, body_hash, intro, last_activity, archived_cycles,
  latest_revision, latest_implementation,
  participants_cumulative, stage fields.
- `PlanTimelineEvent` is `{ sha, ts, touched_plan,
  touched_code, finished }`.
- `state.ad_hoc` is `Vec<AdHocEvent>`. Cleared on plan
  intro.
- `apply_commit` is `RepoState::apply_commit(&mut self,
  event: &CommitEvent)`. No `FoldCarry` exists in the
  codebase.
- The fold module is sans-io: no `use ... git_io`, no
  `tokio::fs`, no IO of any kind.
- The cache stores the entire `RepoState`. Cache file
  header records the saved-at HEAD SHA. Warm rebuilds
  load + apply new commits; cold starts walk from root.
- Cache `try_load` errors delete the offending file.
- WFW and response projection read from `state.plans` /
  `state.ad_hoc` and call projection functions for body /
  participants / readiness / warnings.
- The grep-mechanized invariants pass.
- `core-model-invalid-states-unrepresentable.md` is
  deleted from `.trinity/plans/`.
- `tagged-enums-for-non-orthogonal-fields.md` is deleted
  from `.trinity/plans/`.

## Out of Scope

- Changing user-facing approval / RC / force-finish
  semantics.
- Live config reload — config snapshotted at startup.
- `.trinity/config` policies for ad-hoc commits (deny /
  require-review / ignore). The new design tracks ad-hoc
  events; the policy layer is a follow-up plan.
- Adding any per-plan derived-index cache. PlanState IS
  the per-plan cache; cross-plan queries iterate
  `state.plans` and `state.ad_hoc`.
- Multi-SHA checkpoint caching. This plan ships per-HEAD
  cache with single-ancestor fold-forward. If branch
  divergence or long history becomes a problem, a future
  plan adds checkpoints.
- Removing the web UI.
