# core-model-invalid-states-unrepresentable

## Summary

Trinity's job is to fold a commit history into a per-plan
workflow state. The core model has two problems:

1. **Same fact stored in multiple disagreeable places.** Today
   `CommitNode` carries `kind`, `attribution`, `plans`, and
   `gate` as parallel fields that can disagree, plus
   `Plan.timeline` as a second canonical event log next to
   the commit stream.
2. **Bucket framing over composable facts.** Earlier revisions
   of this plan encoded reviewability into a `CommitBody`
   tagged enum (`Plan` / `MultiPlan` / `AdHoc` /
   `LifecycleOnly`). Each new combination of real-world facts
   (multi-plan, finalize-while-revising, pure-lifecycle) kept
   forcing a new bucket. The buckets are a leaky model of the
   underlying facts.

This plan replaces both with a simpler architecture: active
plans only, no global per-commit registry, the fold lives in
`trinity-core` and runs incrementally on top of the cache.

- `RepoState { plans, ad_hoc, current_attribution,
  finalize_files }`. Active plans only — finalized plans
  exit the map. `git log` is the system of record for "what
  happened before."
- `CommitNode` answers "what happened in this commit?" — stored
  as a flat set of *facts* about the commit (which plan files
  were touched, which plans were finalized, whether code
  changed, what plan the master attributed it to). No bucket
  enum.
- `PlanState` answers "what is the current folded state of
  this plan?" — the canonical fold output, holding the SHAs
  and aggregates the workflow needs.
- Gate / readiness / "is this commit reviewable for X" are
  *dynamic projections* over `(commit, plan_state, ad_hoc,
  config)`. Never stored.

The "invalid states unrepresentable" goal is achieved by
giving every fact a single canonical home, not by encoding
bucket choices into types. Facts compose freely; reviewability
is a projection.

This plan also absorbs the goal of
`tagged-enums-for-non-orthogonal-fields.md`. The intent stays
intact for API/projection types where shapes are *genuinely*
mutually exclusive (`api::CommitRow`, `api::DiffLine`,
`api::TimelineEvent`). What it does NOT apply to is the core
commit model — that's flat facts.

## Hard Direction

After this plan:

- `RepoState` is `{ plans, ad_hoc, current_attribution,
  finalize_files }`. No `commit_order`. No `commits` map.
  No `plan_conflicts`.
- `state.plans` holds **only active plans**. A plan exits
  the map on finalize OR on a non-frozen Delete touch. The
  fold does not retain history about finished plans —
  `git log` is the system of record for "what happened
  before."
- Each `PlanState` owns its commits inline:
  `commits: Vec<CommitNode>` in fold order. Multi-plan
  commits clone the `CommitNode` into each touched plan's
  Vec. There is no shared per-SHA registry.
- `state.ad_hoc` is `Vec<CommitNode>` — commits not
  associated with any plan, in fold order. **Cleared when a
  new plan is introduced** (the new plan emerging means the
  prior ad-hoc work is now context, not actionable).
  Future: `.trinity/config` policies can declare "deny ad-hoc
  commits / require review of ad-hoc range / ignore." Not
  implemented in this plan.
- `CommitNode` is `{ meta, touches, finalizes,
  has_code_changes, plan_attribution, reviews }`. Each field
  is a single-source fact. `reviews` is a flat
  `BTreeMap<AgentLabel, FeedbackBody>` — scope is implicit
  from which container the node lives in (a copy in plan X's
  Vec carries plan-X-scoped feedback; the ad_hoc copy
  carries ad-hoc-scoped feedback).
- No `CommitBody`, `PlanCommit`, `MultiPlanCommit`,
  `AdHocCommit`, `FinalizeCommit`, `LifecycleOnly` types.
  Those concepts become projections, not types.
- No `FoldCarry`. The fold IS `RepoState::apply_commit(&mut
  self, &CommitEvent)` — single argument.
- The fold lives in `crates/trinity-core/src/repo_state.rs`.
  Sans-io: no git, no filesystem, no async. Daemon-side IO
  builds `CommitEvent`s and calls `apply_commit` for each.
- `PlanState` is the canonical folded per-plan aggregate:
  `{ id, plan_path, body, stage, commits, intro,
  latest_revision, latest_implementation,
  participants_cumulative, last_activity_ts }`. `stage` is
  `Drafting | Implementing` — `Frozen` doesn't exist because
  frozen plans aren't in `state.plans`.
- The cache stores the entire `RepoState` and supports
  incremental fold: load the cached state at an ancestor
  SHA, walk new commits via `git_io`, call `apply_commit`
  for each, persist at the new HEAD. Warm cache updates are
  O(new commits), not O(history). The cache header records
  the SHA the state was saved at — `RepoState` itself
  doesn't carry that.
- Reviewability is a per-(commit, scope, config) projection,
  not a stored flag.
- Gate / readiness / policy are free functions over
  `(commit, plan_state, ad_hoc, config)`.
- A commit can finalize plan A AND touch plan B in the same
  commit — `finalizes = {A}` and `touches = {B: Revise}` are
  independent facts, no either/or. The CommitNode appears in
  B's `commits` Vec; plan A exits `state.plans`.
- Multi-plan commits are just commits with `touches.len() ≥ 2`.
  Each touched plan's Vec gets the CommitNode; each plan
  independently reviews.

## Target Types

### Commit Facts

```rust
pub struct CommitNode {
    pub meta: CommitMeta,
    /// Plan files modified in this commit's tree. Key = plan
    /// id; value = the kind of touch.
    pub touches: BTreeMap<PlanKey, PlanTouchKind>,
    /// Plans whose freeze threshold was crossed by an
    /// approving file landing under `.trinity/finished/<stem>/`
    /// in this commit.
    pub finalizes: BTreeSet<PlanKey>,
    /// True iff this commit modified any non-plan, non-trinity
    /// file.
    pub has_code_changes: bool,
    /// The plan this commit *says* it belongs to, from either
    /// (a) the explicit title prefix `[plan-x]`, or
    /// (b) walk-back chain inheritance when the title has no
    ///     prefix at all.
    /// `[misc]` makes THIS commit ad-hoc
    /// (`plan_attribution = None`) but does NOT reset the
    /// walk-back carry — descendants without a prefix still
    /// inherit the prior attribution. See "Fold Algorithm"
    /// for `next_effective_attribution` semantics.
    /// Independent of `touches`. Disagreement is a typed
    /// warning.
    pub plan_attribution: Option<PlanKey>,
    /// Universal: every commit can carry feedback files.
    pub reviews: CommitReviews,
}

pub struct CommitMeta {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub warnings: Vec<AttributionWarning>,
}

pub enum PlanTouchKind {
    Intro,    // plan file created
    Revise,   // plan file modified
    Delete,   // plan file removed
}
```

What lives outside `CommitNode`:

- "Is this commit reviewable for plan X?" — projection.
- "Is this an ad-hoc-reviewable commit?" — projection.
- "Multi-plan / lifecycle-only / etc." — classification
  helpers (methods over facts), never stored discriminators.
- "Gate / readiness / state / approvers / requesters" — free
  functions over `(commit, plan_state, ad_hoc, config)`.

### Attribution Warnings — Tagged

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttributionWarning {
    /// Title prefix names plan(s) that don't exist at fold
    /// time.
    UnknownPlanPrefix { unknown_names: Vec<String> },
    /// No prefix (or `[misc]`); classifier inferred attribution
    /// from walk-back. `suggested_prefix` is what the master
    /// would amend the title to.
    MissingPrefix { suggested_prefix: String },
    /// `plan_attribution` and `touches` disagree (e.g.
    /// `[plan-a]` on a commit touching `plan-b`'s file).
    AttributionMismatch {
        attributed: PlanKey,
        touched: Vec<PlanKey>,
    },
    /// Emitted by projection helpers when `plan_attribution`
    /// (or a `touches` key) names a plan that no longer
    /// exists in `state.plans`. Not a fold-time warning — the
    /// fold doesn't know about future deletions; projection
    /// surfaces it on each affected commit.
    DanglingPlanRef { plan: PlanKey },
}
```

One closed vocabulary; every renderer exhaustively matches.
The wire shape `api::AttributionWarning { sha, subject,
message }` is built by projection — the model type is
canonical truth, the wire string is one render of it.

### Review Storage — Minimal Truth, Derived Everything

The canonical fact about a commit's review activity is who
wrote what feedback file on disk. Feedback identity on disk
is `(scope, sha, author)`. The new design makes scope
**implicit from the container** the CommitNode lives in:

- A CommitNode in `state.plans[A].commits` carries A-scoped
  feedback in its `reviews.feedback`.
- A CommitNode in `state.plans[B].commits` carries B-scoped
  feedback (a separate copy of the node, distinct from A's).
- A CommitNode in `state.ad_hoc` carries ad-hoc-scoped
  feedback.

Storage simplifies to a flat author map:

```rust
pub struct CommitReviews {
    /// Author → feedback. Scope is implicit from the
    /// container the CommitNode lives in. A multi-plan
    /// CommitNode is cloned into each touched plan's Vec;
    /// each copy carries that plan's scoped feedback.
    pub feedback: BTreeMap<AgentLabel, FeedbackBody>,
}

pub struct FeedbackBody {
    pub verdict: Verdict,
    pub body: String,
    pub created_at: i64,
}
```

The feedback file's repo-relative path is derived from
`(scope, sha, author)` at projection time; not stored on
`FeedbackBody`. The earlier scope-aware nested-map design
(landed in Phase 2) is reverted in Phase 3 — it was the
right answer for a single-CommitNode-shared-across-scopes
model, but the owned-per-plan model makes scope a property
of the container, not the storage.

Review *policy* is a projection over the commit's facts,
the relevant aggregate, and the snapshotted config:

```rust
pub enum ReviewPolicy {
    Blocking { participants: NonEmptyVec<AgentLabel> },
    NonBlocking { reason: NonBlockingReason },
}

pub enum NonBlockingReason {
    NoParticipants,
    ConfigDisabledPlanReview,
    ConfigDisabledMiscReview,
    /// The commit's facts make it non-reviewable for the
    /// requested scope (asking the plan-scope policy for a
    /// commit not associated with the plan, or asking the
    /// ad-hoc-scope policy for a plan-attributed commit).
    StructurallyNonReviewable,
}

/// Compute the policy for a commit in a given container.
/// Callers know the scope from where they got the CommitNode:
///   - From `state.plans[X].commits` → pass `plan_state =
///     Some(&state.plans[X])`.
///   - From `state.ad_hoc` → pass `plan_state = None`.
pub fn review_policy(
    commit: &CommitNode,
    plan_state: Option<&PlanState>,
    ad_hoc: &[CommitNode],
    config: &Config,
) -> ReviewPolicy;

pub fn readiness(
    commit: &CommitNode,
    plan_state: Option<&PlanState>,
    ad_hoc: &[CommitNode],
    config: &Config,
) -> ReviewReadiness;

pub struct ReviewReadiness {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
}
```

`Blocking` uses `NonEmptyVec` so "blocking with empty
participants" is unrepresentable. When the discovered
participant set is empty, the projection returns
`NonBlocking { NoParticipants }`.

`participants` is also a projection:

- For a plan-scoped CommitNode (`plan_state.is_some()`):
  `plan_state.unwrap().participants_cumulative` — folded
  incrementally during feedback attachment.
- For an ad_hoc CommitNode: distinct feedback authors across
  `ad_hoc` (computed on demand), or
  `config.ad_hoc_reviewers` if set. There is no stored
  `participants_discovered` on RepoState — when feedback
  attaches to an ad_hoc commit, the author goes into that
  commit's `reviews.feedback`; cross-ad-hoc participants are
  derived by iterating the Vec.

`state` / `approvers` / `requesters` / `ambiguous` / `missing`
are all derived outputs over `(reviews, policy)`. This
eliminates the `state = Approved && requesters != []` class
of contradiction by construction.

**Separation of storage and gating.** Feedback files are
truth and attach unconditionally to the matching commit's
`reviews.feedback`. The policy projection decides whether
they gate anything. Late feedback on a structurally
non-reviewable commit (e.g. asking the wrong scope) is
preserved; it just doesn't block master.

**Multi-plan reviewability.** With flat facts, a commit
touching plans A and B IS reviewable for both — A's reviewers
vote from A's scope; B's reviewers vote from B's scope. Each
plan's `PlanState` aggregates its own scope's votes. The old
"MultiPlan is structurally non-reviewable" rule was an
artifact of the bucket model. (If product later wants to
suppress one of the two reviews, that's a policy choice; the
model doesn't preclude it.)

### Convenience Helpers

Methods on `CommitNode` give projection code ergonomic reads
over the facts — no stored discriminators:

```rust
impl CommitNode {
    pub fn sha(&self) -> &CommitSha;
    pub fn subject(&self) -> &str;

    /// Plans this commit "belongs to" for timeline purposes —
    /// union of `touches.keys()`, `finalizes`, and
    /// `plan_attribution`.
    pub fn associated_plans(&self) -> BTreeSet<&PlanKey>;

    /// Iff exactly one entry in `touches` and no other facts
    /// (`!has_code_changes && finalizes.is_empty()`). The
    /// touched plan is the inferred attribution for following
    /// code-only commits walking back through history.
    pub fn plan_only_touch(&self) -> Option<&PlanKey>;

    /// Iff no touches, no code changes, finalizes non-empty.
    /// Used to recognise pure-lifecycle commits in projection
    /// code (e.g. WFW must skip them when listing review
    /// targets). Not a stored bucket.
    pub fn is_pure_lifecycle(&self) -> bool;

    /// Iff `touches.is_empty() && plan_attribution.is_none()
    /// && has_code_changes`. Eligible for ad-hoc review under
    /// `config.enable_misc_review`.
    pub fn is_ad_hoc_eligible(&self) -> bool;
}
```

If a UI needs a single-word label for the commit, it
computes one from the facts at render time — there is no
canonical `CommitKind` field.

## Repo State

```rust
pub struct RepoState {
    /// Active plans only. A plan exits the map on finalize
    /// (its `finalizes` set fires) or on a non-frozen Delete
    /// touch. Frozen plans don't live here — `git log` is
    /// the system of record for "what happened before."
    pub plans: BTreeMap<PlanKey, PlanState>,

    /// Commits not associated with any plan, in fold order.
    /// Cleared on the next plan intro — the new plan
    /// emerging means whatever ad-hoc work preceded it is
    /// now context, not actionable. Future
    /// `.trinity/config` policies (deny / require-review /
    /// ignore) act on this Vec but are not implemented in
    /// this plan.
    pub ad_hoc: Vec<CommitNode>,

    /// Walk-back attribution carry. Updated by every
    /// `apply_commit` from
    /// `ClassifierOutput::next_effective_attribution`.
    pub current_attribution: Option<PlanKey>,

    /// Per-active-plan approving-files map, mirrored from
    /// `.trinity/finished/<plan>/<author>.md`. The freeze
    /// rule fires when an approving file landing in a
    /// commit's tree crosses the configured threshold, and
    /// that decision needs the current count for that plan.
    /// Lives on RepoState because it's canonical state the
    /// fold reads + mutates; not derivable from `state.plans`
    /// or `state.ad_hoc`.
    ///
    /// Inner map: author filename → file body. Entries are
    /// removed when the plan exits `state.plans`.
    pub finalize_files: BTreeMap<PlanKey, BTreeMap<String, String>>,
}
```

What's NOT here (and why):

- `commit_order: Vec<CommitSha>`. Removed — `git log` is the
  canonical chronological commit stream. RepoState's job is
  active-workflow state, not a per-repo commit registry.
- `commits: BTreeMap<CommitSha, CommitNode>`. Removed for
  the same reason. Each plan owns its `CommitNode`s in
  `commits: Vec<CommitNode>`; cross-plan SHA lookup is not a
  supported query.
- `plan_conflicts: BTreeMap<PlanKey, Vec<PathBuf>>` (legacy).
  Removed. If a plan key resolves to multiple paths, that's
  a Phase 3 IO-side error surfaced when the fold can't
  classify a commit's touches.
- Frozen plans. They leave `state.plans` on finalize. If the
  UI wants to render "this plan was finalized at SHA X in
  March," it must re-walk `git log` for that information.
  This is a deliberate trade: simpler model, no
  parallel "active vs archived" world.

## Plan State

`PlanState` is the canonical folded aggregate for one
**active** plan. It owns its commits inline — no global
per-SHA registry; multi-plan commits clone the `CommitNode`
into each touched plan's Vec.

```rust
pub struct PlanState {
    pub id: PlanKey,
    pub plan_path: String,
    /// Current body text (latest revision's text, or the
    /// intro text if no revision happened). Plain `String`:
    /// git is the durable identity layer.
    pub body: String,

    pub stage: PlanStage,

    /// Every commit associated with this plan, in fold
    /// order. **Owned**: multi-plan commits clone the
    /// `CommitNode` into each touched plan's Vec. No shared
    /// registry. Plan-scope feedback lives in
    /// `commits[i].reviews.feedback` (flat author map; scope
    /// is implicit from the owning plan).
    pub commits: Vec<CommitNode>,

    /// The commit whose `touches[id] == Intro` first created
    /// the plan. Maps to `commits[0]` immediately after
    /// intro.
    pub intro: CommitSha,

    /// Latest commit with `touches[id] == Revise`. `None`
    /// until a revision lands (intro is intro, not revision).
    pub latest_revision: Option<CommitSha>,

    /// Latest commit whose `plan_attribution == Some(id) &&
    /// has_code_changes`. `None` until implementation begins.
    pub latest_implementation: Option<CommitSha>,

    /// Distinct feedback authors across this plan's commits.
    /// Folded incrementally during feedback attachment.
    pub participants_cumulative: BTreeSet<AgentLabel>,

    /// max(commit.author_ts, feedback.created_at) across
    /// `commits`. Folded incrementally.
    pub last_activity_ts: i64,
}

pub enum PlanStage {
    /// Intro landed; no implementation yet.
    Drafting,
    /// At least one implementation commit landed.
    Implementing,
}
```

`PlanStage::Frozen` does not exist. A frozen plan has been
removed from `state.plans` entirely.

What's deliberately NOT on `PlanState`:

- `Vec<PlanTimelineEvent>` (the legacy shape). The plan
  timeline is `commits: Vec<CommitNode>`. The plan-page UI
  builds `api::TimelineEvent` by mapping each `CommitNode`
  at projection time.
- `finalized_at`. Plan removal IS the finalize. The commit
  that triggered the freeze isn't preserved on the plan —
  if the UI needs that SHA (rare, e.g. a "recently
  finalized" feed), it's a git-log query.
- `gate` / `readiness` / `policy` fields. Dynamic projections.
- `body_hash`. Git is the durable identity layer.
- `plan_intro_parent`. If the UI needs the parent commit of a
  plan's intro, it's a `git_io::parent_of(&intro)` query at
  projection time. `RepoState` doesn't track first-parent
  links because it has no global commit registry.
- `archived_cycles`. Frozen plans aren't in `state.plans` and
  the fold doesn't retain their history; cycle data is a
  git-log query.

### Stale Plan References

A code-only commit can `plan_attribution == Some(plan)` where
`plan` was finalized in the same incremental-fold run (or in a
prior run not represented in the cached state). The fold is
forward-only; the older attribution doesn't get rewritten when
its plan exits `state.plans`.

Projection-time behaviour:

- `state.plans.get(plan)` returns `None` for a finalized /
  deleted plan.
- Multi-plan or attribution-mismatch commits still carry the
  dead plan key in their `touches` / `plan_attribution`.
- The UI / WFW treats "plan key with no entry in
  `state.plans`" as a soft signal — surface what the commit
  says, don't try to look up a plan body.
- `AttributionWarning::DanglingPlanRef { plan }` is emitted
  by projection helpers when an attribution names a plan not
  in `state.plans`. NOT stored on the commit.

## Projections

Every per-plan query reads from `PlanState`'s owned commits
Vec — O(1) lookups into `state.plans[plan]`, then iteration
over its `commits`.

```rust
impl PlanState {
    /// Iterate the plan's commits in fold order.
    pub fn commits(&self) -> &[CommitNode] { &self.commits }

    /// Latest commit (the candidate review target — by
    /// construction the plan's `commits` only holds
    /// plan-associated nodes; the last one is the working
    /// front).
    pub fn latest(&self) -> Option<&CommitNode> {
        self.commits.last()
    }
}
```

Common queries become direct reads:

- Plan timeline → `state.plans[plan].commits.iter()`.
- Latest reviewable for plan →
  `state.plans[plan].commits.last()`.
- Latest revision → `state.plans[plan].latest_revision`.
- Latest implementation → `state.plans[plan].latest_implementation`.
- Last activity → `state.plans[plan].last_activity_ts`.
- Ad-hoc work pending review → `state.ad_hoc.iter()`.

There is no "look up a commit by SHA" query on `RepoState`.
If a caller has a SHA and needs the `CommitNode`, it must
already know which plan (or ad_hoc) the SHA belongs to. For
the daemon, that's always the case — WFW and projection
contexts walk from a `PlanState` or `state.ad_hoc`
explicitly.

## Tagged Enum Carry-Forward

This plan subsumes the intent of
`tagged-enums-for-non-orthogonal-fields.md` for API/projection
types where shapes are *genuinely* mutually exclusive:

- `api::CommitRow` stays a tagged enum.
- `api::DiffLine` stays a tagged enum (line numbers aren't
  fake `Option`s).
- `api::TimelineEvent` stays a tagged enum.

Where this plan diverges from the prior tagged-enum direction
is the *core commit model itself*. The core model holds facts
that compose freely (`touches` and `finalizes` co-occur; a
commit can have both plan touches and code changes). Packing
those into a tagged enum forced artificial bucket choices.
API projections turn the flat facts into shape-specific UI
types where mutual exclusion is real.

`model::PlanTimelineEvent` is removed from canonical state —
the plan timeline is `PlanState.commits: Vec<CommitNode>`. If
the API still wants a `PlanTimelineEvent` shape, it lives in
the `api` module and is built by projection.

## Fold Algorithm

### Architectural Constraints

The legacy fold has four interrelated design problems that
make the cache useless for incremental work. The flat-facts
rewrite fixes all four, otherwise the new model inherits the
same waste:

1. **`FoldCarry` is throwaway scratch state.** Today the fold
   threads a `FoldCarry { current_effective, plan_in_tree,
   plan_bodies, finalize_tree, previous_commit }` struct
   through every `apply_commit` call. The carry is never
   persisted — so on cache miss, the fold has to start from
   `FoldCarry::new()` at the root commit, walking every commit
   in history regardless of whether it touches `.trinity/`.
2. **`apply_commit` takes two mutable references.** Today's
   signature is `apply_commit(&mut RepoState, &mut FoldCarry,
   &CommitEvent)`. Threading the carry separately means callers
   must pass it (and risk re-using stale carry against
   different state), and there's no single "the fold state
   right now" object you can serialize.
3. **The fold is daemon-side.** `apply_commit` lives in
   `src/disk_snapshot.rs` alongside the IO-heavy feedback
   attachment pass. Trinity-core has the data types but not
   the fold function. That means alternative consumers (tests,
   tooling, alternate runtimes) can't reuse the fold without
   pulling in the daemon's IO surface.
4. **The cache stores a terminal `BaseRepoState`** keyed by
   HEAD. Hit = "skip the fold entirely." Miss = "walk all of
   history from the root commit," every time. There is no
   "load the cached state at SHA X, fold-forward just the new
   commits to HEAD" path, because (a) the cache only stores
   one SHA's state, not any ancestor, and (b) even if it did,
   the carry needed to resume is gone.

The Phase 3 rewrite addresses all four:

1. **`FoldCarry` is deleted.** Anything the next `apply_commit`
   needs to read becomes a field on `RepoState`. The fold
   state IS the repo state — there is no separate scratch.
2. **`apply_commit` is a method on `RepoState` taking a single
   argument**: `impl RepoState { fn apply_commit(&mut self,
   event: &CommitEvent); }`.
3. **The fold lives in `trinity-core::repo_state`.** The
   module is sans-io: it consumes a pre-built `CommitEvent`
   (which the daemon constructs via `git_io`) and mutates
   `self`. No git, no filesystem, no async — pure data
   transformation.
4. **The cache supports incremental fold.** Cached state is a
   `RepoState` (post-`apply_commit` at some SHA). On cache
   hit at a non-HEAD ancestor SHA, the daemon walks just the
   commits from that SHA to HEAD via `git_io` and calls
   `state.apply_commit(&event)` for each. No re-fold from the
   root.

### `RepoState` absorbs the carry

See the "Repo State" section above for the canonical
declaration. The two former-`FoldCarry` fields that survive
as canonical state are `current_attribution` and
`finalize_files`; the rest of `FoldCarry` is reconstructable:

- `plan_in_tree` → derivable from `state.plans.keys()`.
- `plan_bodies` → already on `PlanState.body`.
- `previous_commit` → not needed; the fold doesn't have a
  "previous commit" concept anymore (there's no global
  `commit_order` to index into).

The fold mutates these former-carry fields on `&mut self`
along with `plans` and `ad_hoc`; no second `&mut FoldCarry`
parameter exists.

### `CommitEvent` (the fold's input)

```rust
/// All facts the fold needs about one commit. Sans-io: built
/// by the daemon (or any other caller) before calling
/// `state.apply_commit`. The daemon's `git_io::snapshot`
/// produces a `Vec<CommitEvent>` for cold rebuilds; for
/// incremental updates the daemon constructs CommitEvents
/// one-at-a-time as new commits arrive.
pub struct CommitEvent {
    pub meta: CommitMeta,           // sha, author_ts, subject
    pub plan_touches: Vec<PlanTouchInput>,
    pub finalize_changes: Vec<FinalizeFileChange>,
    pub has_code_changes: bool,
}

pub struct PlanTouchInput {
    pub plan: PlanKey,
    pub kind: TouchKind,
    pub new_body: Option<String>,   // Intro/Revise carry it; Delete is None
}

pub enum FinalizeFileChange {
    Added { plan: PlanKey, filename: String, body: String },
    Modified { plan: PlanKey, filename: String, body: String },
    Removed { plan: PlanKey, filename: String },
}
```

### `apply_commit`

```rust
impl RepoState {
    /// Fold one commit. Pure: no IO, no git access. The
    /// caller pre-extracts the commit's facts into
    /// `CommitEvent` via `git_io` (daemon) or fixture
    /// builders (tests).
    pub fn apply_commit(&mut self, event: &CommitEvent) {
        // 1. Apply finalize-tree mutations from the commit's
        //    diff. These update self.finalize_files; the
        //    threshold-crossings collected here become the
        //    `finalizes` set the classifier sees.
        let mut newly_finalizing: BTreeSet<PlanKey> = BTreeSet::new();
        for change in &event.finalize_changes {
            apply_finalize_change(
                &mut self.finalize_files,
                change,
                &self.plans,
                &mut newly_finalizing,
            );
        }

        // 2. Classify.
        let touches: BTreeMap<PlanKey, TouchKind> = event
            .plan_touches
            .iter()
            .map(|t| (t.plan.clone(), t.kind))
            .collect();
        let known_plans: BTreeSet<PlanKey> =
            self.plans.keys().cloned().collect();
        let classified = classify(ClassifierInputs {
            subject: &event.meta.subject,
            touches: &touches,
            finalizes: &newly_finalizing,
            has_code_changes: event.has_code_changes,
            current_attribution: self.current_attribution.as_ref(),
            known_plans: &known_plans,
        });

        let mut meta = event.meta.clone();
        meta.warnings = classified.warnings;
        let node = CommitNode {
            meta,
            touches: classified.facts.touches,
            finalizes: classified.facts.finalizes.clone(),
            has_code_changes: classified.facts.has_code_changes,
            plan_attribution: classified.facts.plan_attribution.clone(),
            reviews: CommitReviews::default(), // attach pass fills it
        };

        // 3. Apply touches. Intro inserts a new PlanState
        //    AND clears self.ad_hoc — the new plan emerging
        //    means whatever ad-hoc work preceded it is now
        //    context. Revise/Delete mutate the existing plan.
        for touch in &event.plan_touches {
            match touch.kind {
                TouchKind::Intro => {
                    self.ad_hoc.clear();
                    self.insert_plan(touch, &node);
                }
                TouchKind::Revise => self.revise_plan(touch, &node),
                TouchKind::Delete => self.delete_plan(&touch.plan),
            }
        }

        // 4. Append the CommitNode (cloned for multi-plan)
        //    into every associated plan's commits Vec.
        let associated: BTreeSet<PlanKey> =
            node.associated_plans().iter().cloned().cloned().collect();
        for plan in &associated {
            if let Some(ps) = self.plans.get_mut(plan)
                && ps.commits.last().map(|c| &c.meta.sha) != Some(&node.meta.sha)
            {
                ps.commits.push(node.clone());
            }
        }

        // 5. Apply finalize: removes the plan from
        //    self.plans (and self.finalize_files). After
        //    this the plan is GONE — no Frozen variant, no
        //    finalized_at on a leftover entry.
        for plan in &classified.facts.finalizes {
            self.plans.remove(plan);
            self.finalize_files.remove(plan);
        }

        // 6. Code-only attribution: update latest_implementation
        //    on the attributed plan if the plan still exists.
        //    (Finalize in step 5 may have removed it.)
        if let Some(plan) = &node.plan_attribution
            && node.has_code_changes
            && !node.touches.contains_key(plan)
            && let Some(ps) = self.plans.get_mut(plan)
        {
            ps.latest_implementation = Some(node.meta.sha.clone());
            if matches!(ps.stage, PlanStage::Drafting) {
                ps.stage = PlanStage::Implementing;
            }
        }

        // 7. Ad-hoc commits.
        if node.is_ad_hoc_eligible() {
            self.ad_hoc.push(node);
        }

        // 8. Update walk-back carry.
        self.current_attribution = classified.next_effective_attribution;
    }
}
```

Helper invariants:

- `insert_plan(touch, node)` inserts a new `PlanState { id,
  body: new_body, stage: Drafting, commits: vec![node.clone()],
  intro: node.meta.sha, latest_revision: None,
  latest_implementation: None, participants_cumulative:
  Default, last_activity_ts: node.meta.author_ts }`.
- `revise_plan(touch, node)` mutates the existing plan:
  `body = new_body`, `latest_revision = Some(sha)`. The
  CommitNode append happens in step 4.
- `delete_plan(plan)` removes from `self.plans` and
  `self.finalize_files`. Frozen plans were already removed by
  finalize; if a Delete arrives for a plan no longer in
  state, it's a no-op.
- `apply_finalize_change` mutates `self.finalize_files` and
  emits `newly_finalizing` when the post-mutation count
  crosses the freeze threshold for an active plan.

Ordering note: step 4 appends to plans BEFORE step 5
finalizes. That means the finalize commit's CommitNode lives
in the plan's Vec right up until the plan is removed. The
plan exits state with its final commit count visible during
the same `apply_commit`'s subsequent steps — but after the
function returns, that history is gone.

No later stage reinterprets `touches`, `finalizes`,
`has_code_changes`, or `plan_attribution` from a different
source.

### Sans-io location

The fold module lives at `crates/trinity-core/src/repo_state.rs`
(promoting / moving the daemon's current
`src/repo_state.rs::RepoState`). Trinity-core gains a fold
that is:

- pure data → pure data;
- testable from `crates/trinity-core` without spinning up the
  daemon;
- usable by alternate consumers (e.g. a frontend
  state-rebuild path, fuzzers, snapshot tests).

The daemon's `src/disk_snapshot.rs` shrinks to its IO role:
walking the git history to build `Vec<CommitEvent>`, and the
post-fold pass that reads `.trinity/feedback/` files and
calls `state.attach_feedback(...)` (or similar).

### Incremental fold via the cache

The cache stores `RepoState` keyed by HEAD SHA. On rebuild:

1. Resolve current HEAD via `git_io::rev_parse_head`.
2. If a cached `RepoState` exists for HEAD → done; no fold.
3. Else, find the most-recent cached `RepoState` whose
   cache-header SHA is an ancestor of HEAD. (The cache file
   header carries the SHA the state was saved at;
   `RepoState` itself doesn't.) Load it.
4. Walk `git_io::commits_between(cached_head, current_head)`
   building `CommitEvent`s.
5. For each event in order: `state.apply_commit(&event)`.
6. Save the updated `RepoState` under the new HEAD.

`step 3` requires either:
- caching state at multiple SHAs (per-commit or per-N-commit
  checkpoints), OR
- caching only at HEAD and re-folding from root when HEAD
  changes (today's behaviour, no incremental benefit).

For Phase 3, the minimum we ship is **per-HEAD cache that
supports apply_commit on top**: when a new commit lands, the
daemon loads the cached `RepoState` at the previous HEAD,
applies the new commits, and writes the cache at the new
HEAD. Cold start still walks history from the root, but
warm updates are O(new commits) instead of O(history). The
old HEAD's cache file is deleted in the same write.

Future plans can add multi-SHA checkpoints if the cold start
or branch-divergence cost becomes a real problem; the model
doesn't preclude it.

### Cache shape and serialization

The cache stores the entire `RepoState`, including
`current_attribution` and `finalize_files` (both moved off
the carry, both now canonical state). `BaseRepoState` as a
distinct cache wrapper goes away — there is no carry to
strip out.

Validating wrappers (`NonEmptyVec`) need cache-encoding that
goes through validation. wincode-derive bypasses
constructors; either gate cache-encoding behind a serde
boundary that uses `TryFrom`, or hand-write `SchemaRead`
impls. Decide at Phase 3 start; do not ship Phase 3 with a
wincode bypass on validated types.

### Multiple Finalizations in One Commit

A commit that fires the freeze rule for ≥2 plans
simultaneously is rare but representable: `finalizes = {A, B,
C}`. The fold applies each in `BTreeSet` order. No
information is dropped (the prior bucket-model's
`BTreeSet::iter().next()` silent-pick is gone — `finalizes`
is a Set, not a single-plan choice).

## Feedback Attachment

Feedback files attach when the SHA exists AND the feedback
path's scope is admissible for that commit:

- Plan-target feedback
  (`.trinity/feedback/<plan>/<sha>/<author>.md`) attaches
  iff `plan ∈ commit.associated_plans()`. Plan reviewers
  vote from that plan's scope.
- Ad-hoc-target feedback
  (`.trinity/feedback/_/<sha>/<author>.md`) attaches iff
  `commit.is_ad_hoc_eligible()`. The reserved `_` segment
  (PlanKey::parse rejects it) keeps the namespace clean.
- Path/scope mismatches are dropped (the file targets the
  wrong commit).
- After attachment, the policy projection decides whether the
  feedback affects readiness. "Non-blocking" is never a
  reason to hide or drop well-targeted feedback.

Each attached feedback updates the relevant aggregate:

- Plan-target feedback adds the author to
  `state.plans[plan].participants_cumulative` and bumps
  `last_activity_ts` if newer.
- Ad-hoc-target feedback adds the author to
  `state.ad_hoc.participants_discovered`.

Multi-plan commits can receive feedback from multiple plan
scopes (one file per `<plan>/<sha>/<author>` path). Each
scope's reviewers vote independently; each plan state
aggregates its own scope's votes.

## Wait-For-Work Projection

WFW reads from `state.plans` and `state.ad_hoc`, then projects
review policy:

- Plan scope: the candidate is
  `state.plans[plan].timeline.last()`. If
  `stage == Frozen`, skip — no review work. Otherwise call
  `review_policy(commit, ScopePlan(plan), Some(plan_state),
  &state.ad_hoc, &config)`. `Blocking` with unresolved RC
  verdicts surfaces master `address_commit_changes` work;
  reviewer `review_commit` work surfaces for any caller in
  `participants` who lacks a current verdict. Strict
  title-fix work scans `plan_state.commits` for outstanding
  `FixCommitTitle` warnings.
- Repo scope: iterate `state.plans` (one candidate per
  active plan — finalized plans aren't in the map), then
  iterate `state.ad_hoc` (each candidate). Aggregate as
  above.

No WFW path reads a stored `gate` field, a `kind`
discriminator, or `Plan.timeline` event records. All
decisions flow from facts + policy projection.

## Response Projection

All response builders read `state.plans` for plan-level facts
and read `CommitNode` facts directly per commit:

- Plan page timeline: walk `state.plans[plan].commits` and
  convert each `CommitNode` to `api::TimelineEvent` via match
  over the facts.
- Plan revision list: filter `commits` to nodes where
  `touches.contains_key(plan)`.
- Implementation list: filter `commits` to nodes where
  `plan_attribution == Some(plan) && has_code_changes`.
- Latest reviewable commit: `state.plans[plan].commits.last()`
  (the timeline only contains plan-associated commits).
- Commit details: read `CommitNode` facts directly; the API
  shape (`CommitRow` / `DiffLine` / `TimelineEvent`) is a
  tagged enum built by projection.

Projection can keep small helper methods for readability,
but it must not rebuild a second persistent model with
different facts.

## Implementation Phases

Phases 1 and 2 of an earlier revision of this plan landed
under the bucket-enum framing (`CommitBody`, `PlanCommit`,
`MultiPlanCommit`, `AdHocCommit`, `FinalizeCommit`,
`MultiPlanTouches`, `NonEmptyVec`, `phase1_invariant_tests`).
The new direction abandons those types. They get deleted in
Phase 1 below before the flat-facts types come in.

### Phase 1 — Delete the bucket types (atomic)

- Delete from `trinity-core::model`: `CommitBody`,
  `PlanCommit`, `MultiPlanCommit`, `AdHocCommit`,
  `FinalizeCommit`, `PlanTouchSummary`, `MultiPlanTouches`,
  `MultiPlanTouchesError`.
- Keep `NonEmptyVec` — still used by
  `ReviewPolicy::Blocking { participants }` so "blocking
  with empty participants" remains unrepresentable. It's a
  general validating helper, not bucket-specific.
- Delete the prior classifier scaffolding:
  `ClassifiedCommit`, `ClassifierInputs`, `classify`,
  `classify_with_prefix`, `classify_without_prefix`,
  `TitlePrefix`, `parse_title_prefix`, `plan_commit_for`.
- Delete `phase1_invariant_tests` (the structural-invariant
  test module for the bucket types).
- The old fold continues running on the legacy `CommitNode`
  shape it has always used; this phase only removes
  abandoned scaffolding.

### Phase 2 — Introduce flat-facts types + new classifier (additive)

- Add to `trinity-core::model`:
  - `CommitNode` (new flat-fact shape, named so it can
    co-exist with the legacy one — e.g. `FactsCommitNode` or
    inside a `model::v2` module; pick at Phase 2 start).
  - `CommitFacts`, `CommitMeta`, `PlanTouchKind`,
    `CommitReviews`, `FeedbackBody`.
  - `AttributionWarning` (tagged enum, with the new
    `AttributionMismatch` variant).
  - `ReviewPolicy`, `NonBlockingReason`, `ReviewScope`,
    `ReviewReadiness`.
  - `PlanState`, `PlanStage`, `AdHocState`.
  - `ClassifierOutput`, `ClassifierInputs`.
- Write the pure classifier
  `classify(ClassifierInputs) -> ClassifierOutput`. Unit
  tests cover:
  - Plan-only intro / revise / delete touches.
  - Explicit `[plan-x]` prefix sets `plan_attribution`.
  - Walk-back inheritance fills `plan_attribution` when the
    title is bare.
  - `[misc]` → explicit ad-hoc (`plan_attribution = None`).
  - Multi-plan touches → `touches` has multiple entries; no
    bucket choice required.
  - Finalize + touch in one commit: `finalizes = {A}` AND
    `touches = {B: Revise}` co-exist.
  - Multiple simultaneous finalizations: `finalizes = {A,
    B}`.
  - `[plan-a]` on a commit touching `plan-b` → emits
    `AttributionMismatch`.
  - Title-prefix names a plan that doesn't exist →
    `UnknownPlanPrefix`.

### Phase 3 — Replace canonical fold (atomic)

The model swap + the four architectural fixes in "Fold
Algorithm" above. ONE atomic commit (or PR landing as one
squashed commit) because the deletions, the consumer
migrations, and the cache version bump don't survive
intermediate states.

**Move + restructure the fold**:

- Create `crates/trinity-core/src/repo_state.rs`. Move
  `RepoState` (and related types — `BaseRepoState` becomes
  just `RepoState`) into it.
- Define `CommitEvent` (sans-io fold input) in
  `crates/trinity-core/src/repo_state.rs`.
- Implement `RepoState::apply_commit(&mut self, event:
  &CommitEvent)` as a method on `RepoState`. The two-arg
  `apply_commit(state, carry, event)` shape is gone.
- Move classifier `classify(...)` and `CommitFacts` into the
  same module (they're already in `trinity-core::model::facts`;
  move them up — Phase 4 collapses the `facts` namespace).
- Delete `FoldCarry`. Its fields are absorbed:
  - `current_effective` → `RepoState.current_attribution`.
  - `finalize_tree` → `RepoState.finalize_files`.
  - `plan_in_tree` → derived from `RepoState.plans.keys()`.
  - `plan_bodies` → already on `PlanState.body`.
  - `previous_commit` → not needed (no `commit_order`).

**Swap the canonical fold-state types**:

- Switch `RepoState` to `{ plans, ad_hoc,
  current_attribution, finalize_files }`. No `commit_order`,
  no global `commits` map, no `plan_conflicts`.
- Each `PlanState` owns `commits: Vec<CommitNode>` inline.
  Multi-plan commits clone the node into each touched
  plan's Vec.
- `state.ad_hoc` is `Vec<CommitNode>`. Cleared on the next
  plan intro inside `apply_commit`.
- Plans EXIT `state.plans` on finalize (their `finalizes`
  set fires) or on non-frozen Delete. There is no
  `PlanStage::Frozen` and no `finalized_at`.
- `CommitReviews` reverts to `BTreeMap<AgentLabel,
  FeedbackBody>` (flat author map). Scope is implicit from
  the container the CommitNode lives in (plan Vec or
  ad_hoc Vec). The Phase 2 scope-aware nested map was
  designed for a single-CommitNode-shared-across-scopes
  model that the owned-per-plan design replaces. Revert
  `CommitReviews` and `ReviewScope` (the `Plan(PlanKey) |
  AdHoc` enum is no longer needed at the storage layer —
  feedback attachment looks up the target plan or ad_hoc
  container directly).
- Delete the legacy `model::CommitNode` (`kind`,
  `attribution`, `plans`, `gate`, `attribution_warning`).
- Delete the legacy `model::Plan` struct entirely
  (`timeline: Vec<PlanTimelineEvent>`, `last_activity_ts`,
  `plan_intro_parent`, `archived_cycles`, `body_hash`,
  `lifecycle`).
- Delete `model::PlanTimelineEvent` from canonical state
  (move to `api` if the API still uses it for the wire).
- Delete `model::Feedback` and `model::CommitGate` (the
  legacy review storage). `CommitReviews` + `ReviewReadiness`
  replace them.
- Delete `plan_conflicts` and the legacy `model::PlanConflict`
  surfacing — Phase 3 surfaces conflicting plan paths as an
  IO-side error during `CommitEvent` construction, not as
  state.
- Delete `model::facts::AdHocState` (Phase 2 type — replaced
  by plain `Vec<CommitNode>` on `RepoState.ad_hoc`).
- Delete `model::facts::PlanStage::Frozen` (Phase 2 variant
  — replaced by plan-removal-on-finalize).
- Delete `PlanState.finalized_at` (Phase 2 field — same
  reason).

**Daemon-side rewrites**:

- `src/disk_snapshot.rs` shrinks to its IO role: walks git
  history via `git_io`, builds `Vec<CommitEvent>`, calls
  `state.apply_commit(&event)` in fold order. The post-fold
  pass that reads `.trinity/feedback/` files calls
  `state.attach_feedback(...)` (new method on RepoState that
  matches the scope-aware `CommitReviews` shape and updates
  `participants_cumulative` / `participants_discovered`).
- Rewrite WFW candidate collection (`src/server/wait.rs`)
  and all response projections (`src/responses.rs`,
  `src/server/http.rs`, `src/preview.rs`) to read
  `state.plans` / `state.ad_hoc` and project via the free
  functions `review_policy(...)` / `readiness(...)`.
- Preserve existing API shapes (`CommitRow`, `DiffLine`,
  `TimelineEvent`); they're tagged enums built by
  projection.

**Cache + incremental fold**:

- Bump `state_cache::CACHE_FORMAT_VERSION` (v3 → v4).
- v4 stores the entire `RepoState` (including
  `current_attribution` and `finalize_files`). The
  `BaseRepoState` wrapper goes away — there is no carry to
  strip.
- Rebuild flow changes to incremental-on-warm:
  1. Resolve current HEAD.
  2. If a cached `RepoState` exists at HEAD, return it.
  3. Else, attempt to load the cached `RepoState` at the
     most recent ancestor SHA we have cached.
  4. If a usable ancestor is found, walk
     `git_io::commits_between(ancestor, head)` building
     `CommitEvent`s, call `state.apply_commit(&event)` for
     each, and write the cache at the new HEAD (deleting
     the old HEAD's cache file in the same write).
  5. Otherwise (cold start or no usable ancestor) walk
     history from the root commit as today.
- Cache invariant: any `try_load` error (bad magic,
  format-version mismatch, head mismatch, decode failure)
  must delete the offending file before falling back. Today
  the fallback is silent and stale files leak. Bake
  delete-on-error into the loader; cache hygiene is
  best-effort, never correctness.
- Validating wrappers (`NonEmptyVec`) need cache-encoding
  that goes through validation. wincode-derive bypasses
  constructors. Either gate cache-encoding behind a
  serde-with-TryFrom boundary, or hand-write `SchemaRead`
  impls that call the validating constructor. Decide at
  Phase 3 start.

### Phase 4 — Cleanup

- Remove `CommitAttribution` and any other stale helpers
  left over from the legacy shape.
- Remove naming workarounds from Phase 2 (e.g. rename
  `FactsCommitNode` back to `CommitNode` once the legacy
  type is gone, or collapse the `model::v2` module).
- Retire
  `.trinity/plans/tagged-enums-for-non-orthogonal-fields.md`
  (the API-side intent is preserved in this plan's "Tagged
  Enum Carry-Forward" section; the core-model intent is
  superseded by flat facts).

## Tests

### Unit (classifier + fold)

Every fact combination listed in Phase 2 has a unit test.

Fold invariants verified by unit tests:

- `apply_commit` with `touches = {A: Intro}` creates
  `state.plans[A]` with `stage = Drafting`, `intro = sha`,
  `commits = [node]`, `latest_revision = None`. Also
  clears `state.ad_hoc`.
- `apply_commit` with `touches = {A: Revise}` after intro
  sets `latest_revision = Some(sha)` and appends to
  `state.plans[A].commits`.
- `apply_commit` with `finalizes = {A}` REMOVES
  `state.plans[A]` and `state.finalize_files[A]`. The
  CommitNode that triggered the finalize is NOT preserved
  on the plan (the plan is gone).
- `apply_commit` with `finalizes = {A}` AND
  `touches = {B: Revise}`: plan A exits state; plan B's
  `commits` Vec gets the CommitNode; both facts survive in
  the same call.
- Multi-plan commit (`touches = {A: Revise, B: Revise}`):
  the CommitNode is cloned into BOTH plan A's and plan B's
  `commits` Vec.
- `apply_commit` with `plan_attribution = Some(A),
  has_code_changes = true, touches = ∅`: appends the
  CommitNode to `state.plans[A].commits`, sets
  `latest_implementation = Some(sha)`, transitions stage to
  `Implementing` if applicable.
- `apply_commit` with `is_ad_hoc_eligible() == true` appends
  the CommitNode to `state.ad_hoc`.
- `state.ad_hoc` is cleared when a TouchKind::Intro fires.
- `touches = {A: Delete}` on a non-frozen plan removes
  `state.plans[A]`. On a plan no longer in state (already
  finalized earlier in the same fold pass), the delete is a
  no-op.
- `finalizes = {A, B}` removes both plans and both
  finalize_files entries; no silent pick.
- Incremental-fold equivalence: snapshot the `RepoState`
  after N commits, then call `state.apply_commit(&event)`
  for the (N+1)th commit. The result equals a from-root
  re-fold over all N+1 commits.

### Acceptance grep

A CI check (or `tests/architectural_invariants.rs` running
`std::process::Command::new("grep")` /
`std::process::Command::new("rg")`) asserts:

- `grep -rn "pub kind: " crates/trinity-core/src/model.rs src/`
  returns no canonical-struct hits. `kind` only appears as a
  serde tag on enums (`#[serde(tag = "kind")]`).
- `grep -rn "pub gate: " crates/trinity-core/src/model.rs src/`
  returns no hits (gate is projected via `readiness()`).
- `grep -rn "CommitBody\|PlanCommit\|MultiPlanCommit\|FinalizeCommit\|AdHocCommit\|LifecycleOnly\|MultiPlanTouches" crates/trinity-core/src/`
  returns no hits (bucket types are gone).
- `grep -rn "PlanTimelineEvent" crates/trinity-core/src/`
  returns no hits in canonical state.
- `grep -rn "FoldCarry" --include='*.rs' crates/ src/` returns no
  hits — the type is gone, all carry state is on RepoState.
- `grep -rn "plan_conflicts\|PlanConflict" crates/trinity-core/src/`
  returns no hits — plan-path conflicts are an IO-side
  error, not state.
- `grep -rn "pub commit_order:\|pub commits: BTreeMap" crates/trinity-core/src/`
  returns no hits — RepoState has no global commit registry.

These run as part of `cargo test` so the next refactor
can't silently regress.

### Regression

- Plan-scoped WFW finds the right candidate for plans that
  went through revise → implement → revise → implement.
- Multi-plan commits surface review work for each plan they
  touch (the cloned CommitNode in each plan's Vec carries
  that plan's reviewability).
- Finalized plans are absent from `state.plans` (no
  "frozen" state retained); WFW iterating `state.plans`
  doesn't see them.
- An ad-hoc commit followed by an intro: `state.ad_hoc` is
  empty after the intro applies.
- Repo-scoped WFW still surfaces ad-hoc-eligible commits
  in `state.ad_hoc` per the config-derived policy.
- Rendered plan-page timeline matches today's output on
  golden-file fixtures (built from
  `state.plans[plan].commits` → `api::TimelineEvent`).
- A `[plan-a]` commit whose plan is later finalized retains
  `plan_attribution == Some(plan-a)` if it survived in
  another container (multi-plan or ad-hoc); projection
  emits `AttributionWarning::DanglingPlanRef` when the UI
  tries to resolve plan-a.
- An `[plan-a]` commit touching `plan-b`'s file emits
  `AttributionWarning::AttributionMismatch`.
- Warm-cache rebuild test: snapshot a `RepoState` after N
  commits, append one more commit via `apply_commit`, and
  assert the result equals a from-root re-fold over all
  N+1 commits. This is the incremental-fold equivalence
  invariant — the test lives in trinity-core's unit tests,
  not the daemon.

### Wire / shape

- Wire snapshot tests for every response shape that surfaces
  facts: `WaitWorkPayload` (incl. `ReviewReadiness` if it
  appears on the wire), `PlanRow`, `PlanDetailResponse`,
  `CommitDetail`, `TimelineEvent`.

### CI

- `cargo test --workspace --exclude trinity-frontend`.
- `cargo test -p trinity-frontend` if frontend types change.
- `cargo fmt -- --check`.

## Acceptance

- `RepoState` is `{ plans, ad_hoc, current_attribution,
  finalize_files }`. No `commit_order`, no global
  `commits` map, no `plan_conflicts`, no parallel plan
  metadata struct. The former-carry fields are first-class
  state.
- `state.plans` holds only active plans; finalize and
  non-frozen Delete touches remove the plan entirely.
  There is no `PlanStage::Frozen` and no `finalized_at`.
- `state.ad_hoc` is `Vec<CommitNode>`, cleared on the next
  plan intro.
- Each `PlanState` owns its commits inline:
  `commits: Vec<CommitNode>`. Multi-plan commits clone the
  CommitNode into each touched plan's Vec.
- `CommitReviews` is `BTreeMap<AgentLabel, FeedbackBody>` —
  flat, no scope key. Scope is implicit from container.
- `ReviewScope` (the `Plan | AdHoc` enum) is gone — no
  storage layer needs it after the revert. Projection
  callers know scope from where they obtained the
  CommitNode.
- `FoldCarry` does not exist. Any state the fold needs to
  apply the next commit is reachable from `&mut self`.
- `apply_commit` is a single-argument method on `RepoState`:
  `fn apply_commit(&mut self, event: &CommitEvent)`. Sans-io.
- The fold module lives in
  `crates/trinity-core/src/repo_state.rs`. No daemon imports,
  no git, no filesystem, no async.
- The cache stores the full `RepoState` and the rebuild flow
  supports loading a cached state at any ancestor SHA and
  applying new commits via `apply_commit` to reach HEAD.
  Warm rebuilds do NOT re-walk history from the root.
- `CommitNode` is exactly `{ meta, touches, finalizes,
  has_code_changes, plan_attribution, reviews }`. No
  `CommitBody`, no `kind`, no `attribution`, no `plans`, no
  `gate`, no `attribution_warning` field.
- No `CommitBody`, `PlanCommit`, `MultiPlanCommit`,
  `AdHocCommit`, `FinalizeCommit`, `MultiPlanTouches`,
  `LifecycleOnly` types anywhere in `trinity-core::model`.
- Lifecycle changes ride as facts: a single commit can
  finalize plan A AND touch plan B; both facts survive.
- `PlanState` is the canonical folded plan aggregate as
  defined under "Plan State" above. No copied facts, no
  copied gate/readiness, no `body_hash`.
- Review storage is universal: every commit carries
  `reviews: CommitReviews` on the node.
- Reviewability is exclusively a projection over `(commit,
  scope, plan_state, ad_hoc, config)`. No `is_reviewable`
  flag stored on any commit type.
- Policy is a free function over `(commit, scope, plan_state,
  ad_hoc, config)`. Config snapshotted at trinity startup;
  restart re-folds.
- `ReviewReadiness` is the derived projection type that
  replaces the old `CommitGate` sidecar. No canonical
  `CommitGate` field on any struct.
- Feedback files attach when the SHA exists AND the feedback
  path's target scope matches the commit's facts.
  "Non-blocking" or "structurally non-reviewable" is never a
  reason to hide or drop well-targeted feedback.
- WFW, response projection, feedback attachment, and
  lifecycle derivation all read through `state.plans` /
  `state.ad_hoc` and project via `review_policy(...)` /
  `readiness(...)`.
- The grep-mechanized invariants pass as part of `cargo
  test`.
- `tagged-enums-for-non-orthogonal-fields.md` is deleted
  from `.trinity/plans/` as obsolete.

## Out of Scope

- Changing user-facing approval / RC / force-finish
  semantics.
- Live config reload — config snapshotted at trinity startup;
  restart re-folds. A future plan can add live reload.
- Adding any derived-index cache. `PlanState` IS the
  per-plan cache; cross-plan queries iterate
  `state.plans` and `state.ad_hoc`.
- `.trinity/config` policies for ad-hoc commits (deny /
  require-review / ignore). The new design tracks ad-hoc
  commits as `Vec<CommitNode>` cleared on plan intro; the
  policy layer that acts on this Vec is a follow-up.
- Removing the web UI.
- Frontend UX changes around dangling-plan attribution. The
  projection emits `AttributionWarning::DanglingPlanRef`;
  how the UI renders it is a separate question.
- Whether to suppress multi-plan reviews to a single
  "primary plan" — currently every touched plan reviews the
  commit independently. Product can revisit if needed; the
  model doesn't preclude either choice.
