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

This plan replaces both with the same architecture codex laid
out:

- `RepoState { commit_order, commits, plans, ad_hoc }`.
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

- `RepoState` is exactly `{ commit_order, commits, plans,
  ad_hoc }`. No second canonical commit log.
- `CommitNode` is `{ meta, touches, finalizes,
  has_code_changes, plan_attribution, reviews }`. Each field
  is a single-source fact.
- No `CommitBody`, `PlanCommit`, `MultiPlanCommit`,
  `AdHocCommit`, `FinalizeCommit`, `LifecycleOnly` types.
  Those concepts become projections, not types.
- `PlanState` is the canonical folded per-plan aggregate:
  `{ id, plan_path, body, stage, timeline, intro,
  latest_revision, latest_implementation, finalized_at,
  participants_cumulative, last_activity_ts }`. Built by the
  fold; the product of the model.
- Reviewability is a per-(commit, scope, config) projection,
  not a stored flag.
- Gate / readiness / policy are free functions over
  `(commit, plan_state, ad_hoc, config)`.
- Feedback files are truth: every commit can carry
  `CommitReviews`. The feedback path's scope (plan stem vs
  reserved `_` for ad-hoc) decides which commits a feedback
  file can attach to, not whether the commit is "reviewable."
- A commit can finalize plan A AND touch plan B in the same
  commit — `finalizes = {A}` and `touches = {B: Revise}` are
  independent facts, no either/or.
- Multi-plan commits are just commits with `touches.len() ≥ 2`.
  Each touched plan independently reviews the commit; no
  special bucket.

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

The canonical fact about a commit's review activity is exactly
who wrote what feedback file on disk. Feedback identity on
disk is `(scope, sha, author)` — a single SHA can receive
independent feedback from multiple plan scopes (when the
commit touches multiple plans). Storage matches:

```rust
pub struct CommitReviews {
    /// Outer key = scope (plan or ad-hoc), inner key = author.
    /// `(scope, author)` together is canonical identity; the
    /// same author can write independent feedback for two
    /// plan scopes on the same SHA without one overwriting
    /// the other.
    pub feedback: BTreeMap<ReviewScope, BTreeMap<AgentLabel, FeedbackBody>>,
}

pub enum ReviewScope {
    Plan(PlanKey),
    AdHoc,
}

pub struct FeedbackBody {
    pub verdict: Verdict,
    pub body: String,
    pub created_at: i64,
}
```

The feedback file's repo-relative path is derived from
`(scope, sha, author)` at projection time; not stored on
`FeedbackBody`. Readiness for `ReviewScope::Plan(A)` reads
only `commit.reviews.feedback.get(&ReviewScope::Plan(A))` —
B-scope feedback on the same SHA never affects A's
readiness.

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

pub fn review_policy(
    commit: &CommitNode,
    scope: &ReviewScope,
    plan_state: Option<&PlanState>,
    ad_hoc: &AdHocState,
    config: &Config,
) -> ReviewPolicy;

pub fn readiness(
    commit: &CommitNode,
    scope: &ReviewScope,
    plan_state: Option<&PlanState>,
    ad_hoc: &AdHocState,
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

- For `ReviewScope::Plan(X)`: the `PlanState`'s cumulative
  participants up to `sha` — i.e.
  `plan_state.participants_cumulative` snapshotted at fold
  time. Not stored on the commit.
- For `ReviewScope::AdHoc`: `ad_hoc.participants_discovered`
  or `config.ad_hoc_reviewers` if set.

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
    /// Chronological fold order — the only canonical commit
    /// stream.
    pub commit_order: Vec<CommitSha>,
    /// Per-commit canonical facts.
    pub commits: BTreeMap<CommitSha, CommitNode>,
    /// Per-plan folded aggregates. Built by the fold; the
    /// product of the model.
    pub plans: BTreeMap<PlanKey, PlanState>,
    /// Parallel aggregate for ad-hoc-eligible commits.
    pub ad_hoc: AdHocState,
}
```

`commits` answers "what happened in this commit?" `plans`
answers "what is the current folded state of this plan?"
These are NOT parallel truth — `PlanState` stores SHAs that
point into `commits`, not copies of facts.

## Plan State

`PlanState` is a first-class folded aggregate. It carries
exactly the per-plan facts the fold materializes as it walks
commits: ordered SHAs into `commits`, lifecycle stage,
intro/revision/implementation/finalization boundaries, plan
path + body text, and cumulative participants for the gate
projection.

```rust
pub struct PlanState {
    pub id: PlanKey,
    pub plan_path: String,
    /// Current body text (latest revision's text, or the
    /// intro text if no revision happened). Plain `String`:
    /// git is already the durable identity layer; in-memory
    /// equality suffices.
    pub body: String,

    pub stage: PlanStage,

    /// Every commit associated with this plan in fold order.
    /// SHAs only — actual `CommitNode`s live in
    /// `RepoState.commits`. This is the plan's timeline;
    /// the previous shape `Vec<PlanTimelineEvent>` is gone
    /// (it stored duplicated event records), replaced by SHA
    /// references back to canonical commit facts.
    pub timeline: Vec<CommitSha>,

    /// The commit whose `touches[id] == Intro` first created
    /// the plan.
    pub intro: CommitSha,

    /// Latest commit with `touches[id] == Revise`. `None`
    /// until a revision lands (intro is intro, not revision).
    pub latest_revision: Option<CommitSha>,

    /// Latest commit whose `plan_attribution == Some(id) &&
    /// has_code_changes`. `None` until implementation begins.
    pub latest_implementation: Option<CommitSha>,

    /// Latest commit where `finalizes.contains(id)`.
    /// `Some(_)` iff the plan has frozen.
    pub finalized_at: Option<CommitSha>,

    /// Distinct feedback authors who wrote on ANY commit in
    /// `timeline`. Seeds the participants set for plan-scope
    /// gate projection.
    pub participants_cumulative: BTreeSet<AgentLabel>,

    /// max(commit.author_ts, feedback.created_at) across this
    /// plan's timeline. Folded incrementally — not a
    /// projection scan.
    pub last_activity_ts: i64,
}

pub enum PlanStage {
    /// Intro landed; no Revise, no implementation yet.
    Drafting,
    /// At least one implementation commit landed.
    Implementing,
    /// `finalized_at.is_some()`.
    Frozen,
}

pub struct AdHocState {
    /// All commits where `is_ad_hoc_eligible()` returns true,
    /// in fold order.
    pub commits: Vec<CommitSha>,
    /// Distinct feedback authors across all ad-hoc commits.
    pub participants_discovered: BTreeSet<AgentLabel>,
}
```

What's deliberately NOT on `PlanState`:

- `Vec<PlanTimelineEvent>` (the old shape). The plan timeline
  is `timeline: Vec<CommitSha>`. The plan-page UI builds
  `api::TimelineEvent` by mapping each `CommitNode` at
  projection time.
- `gate` / `readiness` / `policy` fields. Dynamic projections.
- `body_hash`. Git is the durable identity layer.
- `plan_intro_parent`. Derived from
  `RepoState.parent_of(&intro)` when needed.
- `archived_cycles`. Derived from `finalized_at` if needed.

### Stale Plan References

When a plan is deleted (its file removed while not frozen),
`state.plans[plan]` is removed. Historical commits still
reference the plan via `touches[plan]` or
`plan_attribution == Some(plan)`. The fold is forward-only;
history is immutable.

Projection-time behaviour:

- `state.plans.get(plan)` returns `None`.
- Walking `state.commit_order` and matching on
  `node.associated_plans()` still surfaces those commits.
- The UI / WFW treats "plan key with no entry in
  `state.plans`" as a soft signal — surface the commits but
  don't try to look up the plan body.
- `AttributionWarning::DanglingPlanRef { plan }` is emitted
  by projection helpers, NOT stored on the commit.

## Projections

`PlanState` answers the common per-plan queries directly —
O(1) reads. A handful of helpers on `RepoState` cover the
cross-plan cases the fold doesn't materialize:

```rust
impl RepoState {
    /// First-parent of `sha` in fold order, or `None` when
    /// `sha` is the root commit. Replaces the old stored
    /// `Plan.plan_intro_parent`.
    pub fn parent_of(&self, sha: &CommitSha) -> Option<&CommitSha>;
}
```

Common queries become direct reads:

- Plan timeline → `state.plans[plan].timeline.iter()`.
- Latest reviewable for plan →
  `state.plans[plan].timeline.last()` if the plan isn't
  frozen (the timeline only contains commits associated with
  the plan; the last one is the candidate review target).
- Latest revision → `state.plans[plan].latest_revision`.
- Latest implementation → `state.plans[plan].latest_implementation`.
- Finalized at → `state.plans[plan].finalized_at`.
- Last activity → `state.plans[plan].last_activity_ts`.

If a query genuinely needs to walk a plan's timeline (e.g.
for a historical render), it dereferences
`PlanState.timeline` against `state.commits`; the
`CommitNode` is the single source of fact truth.

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
the plan timeline is `PlanState.timeline: Vec<CommitSha>`. If
the API still wants a `PlanTimelineEvent` shape, it lives in
the `api` module and is built by projection.

## Fold Algorithm

The classifier returns the full commit fact set in one
structure. Facts and warnings are ALL outputs of the same
classifier call — no later stage reinterprets any of them.
There is no separate `PlanEffect` channel because the facts
ARE the effects.

```rust
pub struct ClassifierOutput {
    pub facts: CommitFacts,
    pub warnings: Vec<AttributionWarning>,
    /// What the walk-back chain becomes after this commit.
    /// `carry.current_attribution` is assigned this verbatim.
    pub next_effective_attribution: Option<PlanKey>,
}

pub struct CommitFacts {
    pub touches: BTreeMap<PlanKey, PlanTouchKind>,
    pub finalizes: BTreeSet<PlanKey>,
    pub has_code_changes: bool,
    pub plan_attribution: Option<PlanKey>,
}

fn apply_commit(state: &mut RepoState, carry: &mut FoldCarry, raw: RawCommit) {
    let classified = classify(ClassifierInputs {
        raw: &raw,
        plans: &state.plans,
        carry,
    });

    let sha = raw.sha.clone();
    let node = CommitNode {
        meta: build_meta(&raw, classified.warnings),
        touches: classified.facts.touches,
        finalizes: classified.facts.finalizes,
        has_code_changes: classified.facts.has_code_changes,
        plan_attribution: classified.facts.plan_attribution,
        reviews: CommitReviews::default(), // filled by attach pass
    };

    // 1. Drive plan aggregates directly from the facts.
    for (plan, kind) in &node.touches {
        apply_touch(&mut state.plans, plan, kind, &sha, &node);
    }
    for plan in &node.finalizes {
        apply_finalize(&mut state.plans, plan, &sha);
    }
    if let Some(plan) = &node.plan_attribution {
        if node.has_code_changes && !node.touches.contains_key(plan) {
            update_implementation(&mut state.plans, plan, &sha);
        }
    }
    if node.is_ad_hoc_eligible() {
        state.ad_hoc.commits.push(sha.clone());
    }

    // 2. Append to every associated plan's timeline (no
    //    duplicates if a plan appears in multiple fact lists).
    for plan in node.associated_plans() {
        if let Some(ps) = state.plans.get_mut(plan) {
            if ps.timeline.last() != Some(&sha) {
                ps.timeline.push(sha.clone());
            }
        }
    }

    // 3. Insert canonical commit facts.
    state.commit_order.push(sha.clone());
    state.commits.insert(sha, node);

    // 4. Update walk-back carry.
    carry.current_attribution = classified.next_effective_attribution;
}
```

Where:

- `apply_touch(plans, plan, Intro, sha, node)` inserts a new
  `PlanState` with `intro = sha`, `body =` (new plan body
  from the commit tree), `stage = Drafting`,
  `latest_revision = None`.
- `apply_touch(plans, plan, Revise, sha, node)` updates body
  and sets `latest_revision = Some(sha)`.
- `apply_touch(plans, plan, Delete, sha, node)` removes the
  plan iff `finalized_at.is_none()` (monotone-finished rule;
  frozen plans survive deletion).
- `apply_finalize(plans, plan, sha)` sets
  `finalized_at = Some(sha)`, `stage = Frozen`.
- `update_implementation(plans, plan, sha)` sets
  `latest_implementation = Some(sha)`, transitions `stage`
  from `Drafting` → `Implementing` when applicable.

No later stage reinterprets `touches`, `finalizes`,
`has_code_changes`, or `plan_attribution` from a different
source.

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
  title-fix work scans `plan_state.timeline` for outstanding
  `FixCommitTitle` warnings.
- Repo scope: iterate `state.plans` (one candidate per
  non-frozen plan), then iterate `state.ad_hoc.commits`
  (each candidate). Aggregate as above.

No WFW path reads a stored `gate` field, a `kind`
discriminator, or `Plan.timeline` event records. All
decisions flow from facts + policy projection.

## Response Projection

All response builders read `state.plans` for plan-level facts
and read `CommitNode` facts directly per commit:

- Plan page timeline: walk `state.plans[plan].timeline` and
  convert each `CommitNode` to `api::TimelineEvent` via match
  over the facts.
- Plan revision list: filter timeline to commits where
  `touches.contains_key(plan)`.
- Implementation list: filter timeline to commits where
  `plan_attribution == Some(plan) && has_code_changes`.
- Latest reviewable commit: `state.plans[plan].timeline.last()`
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

- Switch `RepoState` to `{ commit_order, commits, plans,
  ad_hoc }` using the new types.
- Delete the legacy `CommitNode` (`kind`, `attribution`,
  `plans`, `gate`, `attribution_warning`).
- Delete the legacy `Plan` struct entirely (`timeline:
  Vec<PlanTimelineEvent>`, `last_activity_ts`,
  `plan_intro_parent`, `archived_cycles`). The plan timeline
  is `PlanState.timeline: Vec<CommitSha>` — same concept, no
  duplicated event records.
- Delete `model::PlanTimelineEvent` from canonical state
  (move to `api` if the API still uses it).
- Rewrite `apply_commit` per "Fold Algorithm" above:
  classify, drive plan aggregates from facts, append to
  timelines, insert node.
- Rewrite feedback attachment to match on commit facts, write
  into `node.reviews.feedback`, and update
  `participants_cumulative` / `participants_discovered` as a
  side effect.
- Rewrite WFW candidate collection and all response
  projections to read `state.plans` / `state.ad_hoc` and
  project via `review_policy(...)` / `readiness(...)`.
- Bump `state_cache::CACHE_FORMAT_VERSION` (v3 → v4).
  - v4 carries the new shape; old caches invalidate.
  - Bake delete-on-error into the loader so failed loads
    self-clean (today they leak stale files into the cache
    dir).
- Preserve existing API shapes (`CommitRow`, `DiffLine`,
  `TimelineEvent`); they're tagged enums built by
  projection.

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
  `timeline = [sha]`, `latest_revision = None`.
- `apply_commit` with `touches = {A: Revise}` after intro
  sets `latest_revision = Some(sha)` and appends to A's
  timeline.
- `apply_commit` with `finalizes = {A}` sets
  `state.plans[A].finalized_at = Some(sha)`,
  `stage = Frozen`, and appends to A's timeline.
- `apply_commit` with `finalizes = {A}` AND
  `touches = {B: Revise}` finalizes A AND updates B in one
  commit. Both facts survive in the fold output.
- `apply_commit` with `plan_attribution = Some(A),
  has_code_changes = true, touches = ∅` sets
  `state.plans[A].latest_implementation = Some(sha)`,
  transitions stage to `Implementing` if applicable, and
  appends to A's timeline.
- `apply_commit` with `is_ad_hoc_eligible() == true` appends
  to `state.ad_hoc.commits`.
- `touches = {A: Delete}` on a non-frozen plan removes
  `state.plans[A]`. On a frozen plan, the delete is a no-op
  (monotone-finished rule).
- `finalizes = {A, B}` applies both finalizations; no silent
  pick.

### Acceptance grep

A CI check (or `tests/architectural_invariants.rs` running
`std::process::Command::new("grep")` /
`std::process::Command::new("rg")`) asserts:

- `grep -rn "pub kind: " crates/trinity-core/src/model.rs src/`
  returns no canonical-struct hits. `kind` only appears as a
  serde tag on enums (`#[serde(tag = "kind")]`).
- `grep -rn "pub gate: " crates/trinity-core/src/model.rs src/`
  returns no hits (gate is projected via `readiness()`).
- `grep -rn "CommitBody\|PlanCommit\|MultiPlanCommit\|FinalizeCommit\|AdHocCommit\|LifecycleOnly\|MultiPlanTouches" crates/trinity-core/src/model.rs`
  returns no hits (bucket types are gone).
- `grep -rn "PlanTimelineEvent" crates/trinity-core/src/model.rs`
  returns no hits in canonical state.

These run as part of `cargo test` so the next refactor
can't silently regress.

### Regression

- Plan-scoped WFW finds the right candidate for plans that
  went through revise → implement → revise → implement.
- Multi-plan commits surface review work for each plan they
  touch.
- Frozen plans surface no review work (WFW skips
  `stage == Frozen`).
- Late feedback on a frozen plan attaches but doesn't
  produce WFW work.
- Repo-scoped WFW still surfaces ad-hoc-eligible commits
  per the config-derived policy.
- Rendered plan-page timeline matches today's output on
  golden-file fixtures (built from
  `state.plans[plan].timeline` → `api::TimelineEvent`).
- A `[plan-a]` commit whose plan is later deleted retains
  `plan_attribution == Some(plan-a)`; projection emits
  `AttributionWarning::DanglingPlanRef`.
- An `[plan-a]` commit touching `plan-b`'s file emits
  `AttributionWarning::AttributionMismatch`.

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

- `RepoState` is exactly four fields: `commit_order`,
  `commits`, `plans`, `ad_hoc`. No second canonical commit
  log; no parallel plan metadata struct.
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
  per-plan cache; cross-plan queries walk `state.commits` /
  `state.commit_order` directly.
- Removing the web UI.
- Frontend UX changes around dangling-plan attribution. The
  projection emits `AttributionWarning::DanglingPlanRef`;
  how the UI renders it is a separate question.
- Whether to suppress multi-plan reviews to a single
  "primary plan" — currently every touched plan reviews the
  commit independently. Product can revisit if needed; the
  model doesn't preclude either choice.
