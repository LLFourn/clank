# core-model-invalid-states-unrepresentable

## Summary

Trinity's job is to fold a commit history into a per-plan workflow
state. The current core model carries the same per-commit fact in
several parallel forms — `CommitNode.kind`, `.attribution`,
`.plans`, `.gate`, plus `Plan.timeline` — and the type system
permits states like "ad hoc commit with plan membership",
"reviewable kind with no gate", or "commit attributed to plan A
while plan B's timeline contains it." We keep these aligned by
hand; codex keeps catching the gaps.

This plan splits the model into three layers:

1. **Canonical commit facts.** Each commit gets one `CommitNode`
   with one tagged `CommitBody` (Plan / MultiPlan / AdHoc) plus
   shared `meta` and `reviews`. No `kind`-discriminator-with-
   sibling-Options. No `gate` field — readiness is projected.
2. **Canonical folded plan aggregates.** Each plan gets one
   `PlanState` — the fold's plan-level output: ordered commits,
   intro/latest-revision/latest-impl/finalized SHAs, lifecycle
   stage, body, cumulative participants. PlanState is NOT
   "metadata only"; it is the workflow state that all of Trinity
   reads to answer "where is this plan?"
3. **Dynamic projections.** Gate/readiness/policy are functions
   of `(commit, plan_state, config)`. Never stored.

The bad parallel truth is "same commit's classification or review
gate stored in two contradictory forms." A folded plan aggregate
is NOT bad parallel truth — it's the fold's product. Trinity's
job is exactly to compute it. We just need each fact to have one
canonical home, and we need the type system to make the
disagreements unrepresentable.

This plan also absorbs the goal of
`tagged-enums-for-non-orthogonal-fields.md`: when a shape varies
by kind, model it as a tagged enum with variant-specific fields.
Some of that work has already landed for `CommitRow`, `DiffLine`,
and `PlanTimelineEvent`; this plan keeps that direction but
extends it to the core commit model.

## Hard Direction

After this plan:

- `RepoState` has four canonical fields: `commit_order`,
  `commits`, `plans`, `ad_hoc`.
- `commit_order` is the only chronological stream.
- `commits` is the per-SHA fact map.
- `plans` is the per-plan folded aggregate map (`PlanState`).
- `ad_hoc` is the parallel aggregate for commits with no plan.
- `CommitNode` is `{ meta, body, reviews }` — no `kind` /
  `attribution` / `plans` / `gate` fields.
- A reviewable commit's review state is computed from
  `(commit.reviews, plan_state.participants_cumulative, config)`
  via `gate_for(...)`. Never stored.
- A `MultiPlan` commit can still carry feedback files (they're
  truth); the gate projection returns
  `NonBlocking { StructurallyNonReviewable }` so they don't
  block master.
- A plan-only commit cannot exist without plan-touch data
  (variant requires `touch`).
- A code-only commit cannot carry plan-touch data (variant has
  no `touch` field).
- A mixed commit must carry both the plan touch and the
  non-plan code flag (encoded by variant).
- Ad hoc commits cannot accidentally name plans (no `plan`
  field in `AdHoc` variant).
- Finalize is not a `CommitBody` variant — it's a `PlanEffect`.
  A commit can finalize plan A AND touch plan B in the same
  commit; both facts survive.
- Multi-plan commits cannot accidentally be treated as
  single-plan review targets (`MultiPlanTouches` requires ≥2
  distinct plans by construction).

The fold remains one large mutable `apply_commit` function. The
important requirement is that each commit is classified once
into a structurally valid `(body, plan_effects)` pair, the
effects are applied to `PlanState`, and every downstream
projection reads from `(commit, plan_state, config)` — never
from a parallel cached classification.

## Target Types

### Commit Metadata

Shared fields stay shared:

```rust
pub struct CommitNode {
    pub meta: CommitMeta,
    pub body: CommitBody,
    /// Universal: every commit can carry feedback files. The
    /// gate projection decides whether the feedback affects
    /// readiness.
    pub reviews: CommitReviews,
}

pub struct CommitMeta {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub warnings: Vec<AttributionWarning>,
}
```

Warnings are shared metadata because any commit can carry
operator-facing warnings such as "unknown plan prefix" or "missing
commit-title prefix".

`reviews` is hoisted to the node because every variant — Plan,
MultiPlan, AdHoc — can carry feedback files on disk. Storing
`reviews` per-variant would mean repeating the same field in
three places; storing it once on the node makes "every commit
has reviews" a structural fact instead of a convention.

### Attribution Warnings — Tagged

`AttributionWarning` today is a single `Option<String>` blob. An
"invalid states unrepresentable" plan should not introduce a new
stringly-typed sidecar:

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttributionWarning {
    /// Commit subject's `[…]` prefix names plan(s) that don't
    /// exist at fold time. Classifier degrades to AdHoc.
    UnknownPlanPrefix { unknown_names: Vec<String> },
    /// No `[…]` prefix on the subject. Classifier inferred
    /// attribution from file touches or walk-back. The inferred
    /// suggestion is what the master would amend the title to.
    MissingPrefix { suggested_prefix: String },
    /// Strict-mode placeholder: prefix names a multi-plan list
    /// that can't be disambiguated without operator input. The
    /// classifier has no single suggested fix.
    AmbiguousPrefix,
    /// Emitted by projection helpers when a commit's
    /// `touch.plan()` (or `Plan(plan)` from CodeOnly) names a
    /// plan that no longer exists in `state.plans`. Not a fold-
    /// time warning — the fold doesn't know about future
    /// deletions; the projection layer surfaces it on each
    /// affected commit.
    DanglingPlanRef { plan: PlanKey },
}
```

The enum is the single closed vocabulary for attribution
warnings; projection renderers exhaustively match on it to
produce wire/UI strings. Adding a variant forces a compile
error at every renderer call site.

`api::AttributionWarning` (the wire shape) is built by
projection: `{ sha, subject, message }` where `message` is a
human-readable rendering of the model variant. The model type is
the canonical truth; the wire string is one render of it. There
is exactly one place that knows how to render each variant.

### Commit Body

The body is where invalid combinations become unrepresentable.
**Serde tagging detail**: nested internally-tagged enums collide
on the `tag` field name, and `PlanCommit` below ALSO uses `kind`
internally. So the outer enum uses a different discriminator
(`scope`) than the inner one (`kind`):

```rust
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CommitBody {
    Plan(PlanCommit),
    MultiPlan(MultiPlanCommit),
    AdHoc,
}
```

The wire shape becomes `{"scope": "plan", "kind": "plan_only",
"plan": "...", ...}` — operator-readable and conflict-free.

Note: lifecycle events (Finalize, plan Intro/Revise/Delete) are
NOT body variants — they live on the parallel `plan_effects`
channel of `ClassifiedCommit`. See "Lifecycle Effects vs. Body"
below.

### Review Storage — Minimal Truth, Derived Everything

`CommitGate` today is a flat derived-state bag: `state`,
`participants`, `approvers`, `requesters`, `ambiguous`,
`missing`, `feedback`. Those fields can disagree just as badly
as the commit fields do (`state = Approved` with non-empty
`requesters`, or `missing` that doesn't match
`participants - voters`).

The canonical fact about a commit's review activity is exactly
one thing — who wrote what feedback file on disk:

```rust
pub struct CommitReviews {
    /// Canonical: the map key IS truth for author identity.
    /// `FeedbackBody` carries the verdict + body + mtime, NOT a
    /// duplicated author field.
    pub feedback: BTreeMap<AgentLabel, FeedbackBody>,
}

pub struct FeedbackBody {
    pub verdict: Verdict,
    pub body: String,
    pub created_at: i64,
}
```

The feedback file's repo-relative path is derived from
`(target, sha, author)` at projection time; it's not stored on
`FeedbackBody`.

**Policy is a projection, not a stored field.** Whether master is
blocked on a commit depends on (a) which variant the commit is
and (b) the trinity-startup config snapshot — both inputs to a
method, not a fold-time decision baked into the commit. The
config is read once at trinity startup and held in the runtime;
config changes require a restart (which re-folds), so policy
never goes stale relative to the snapshot it was computed from.

```rust
pub enum ReviewPolicy {
    Blocking { participants: NonEmptyVec<AgentLabel> },
    NonBlocking { reason: NonBlockingReason },
}

pub enum NonBlockingReason {
    NoParticipants,
    ConfigDisabledPlanReview,
    ConfigDisabledMiscReview,
    /// The commit's variant is structurally non-reviewable
    /// (`MultiPlan`). Pure-finalize commits (body = AdHoc with a
    /// `Finalize` plan_effect) are categorized by their body
    /// variant; the finalize is a plan-aggregate effect, not a
    /// review fact.
    StructurallyNonReviewable,
}
```

`Blocking` uses `NonEmptyVec` so "blocking with empty
participants" is unrepresentable. When the discovered participant
set is empty, the projection returns `NonBlocking { NoParticipants }`
instead.

`participants` is also a projection — for plan commits it's
the `PlanState`'s cumulative participants up to that commit
(`plan_state.participants_cumulative` snapshotted as of `sha`);
for ad hoc commits it's `ad_hoc.participants_discovered` or the
config override `ad_hoc_reviewers` if set. Neither is stored
on `CommitNode`.

`state` / `approvers` / `requesters` / `ambiguous` / `missing`
are all derived outputs over `(reviews, policy)` — never stored.
This eliminates the `state = Approved && requesters != []` class
of contradiction by construction.

**Separation of storage and gating.** Feedback files on disk are
always truth; they attach to the commit's `CommitReviews`
unconditionally — every variant. The policy projection decides
whether they gate anything. Late feedback on a `MultiPlan` commit
(or any non-reviewable commit) is preserved (the file IS truth)
and surfaces in the UI; the policy just returns
`StructurallyNonReviewable`, so it doesn't block master. This
removes the old "non-blocking means the file is invisible" bug
class.

### Plan Commits

Use nested variants, not `PlanCommit { kind: PlanCommitKind, ... }`.
Each variant carries the *minimum* set of canonical facts; the
plan key for `PlanOnly` and `Mixed` is *projected* out of `touch`
because the touch already names it. Storing both fields would
reintroduce the parallel-truth problem this plan exists to remove.

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanCommit {
    PlanOnly {
        touch: PlanTouchSummary,   // touch.plan is the plan key
    },
    CodeOnly {
        plan: PlanKey,             // no touch; key must be stored
    },
    Mixed {
        touch: PlanTouchSummary,   // touch.plan is the plan key
    },
}

impl PlanCommit {
    /// Single source of plan identity per variant.
    pub fn plan(&self) -> &PlanKey {
        match self {
            PlanCommit::PlanOnly { touch }
            | PlanCommit::Mixed { touch } => touch.plan(),
            PlanCommit::CodeOnly { plan } => plan,
        }
    }
}
```

Reviews live on `CommitNode`, not per-variant. See "Commit
Metadata" above.

No `policy` field on PlanCommit variants. Policy is a projection
method on `CommitNode` driven by the snapshotted config (see
"Review Storage" above and "Convenience Methods" below).

This eliminates:

- `kind == PlanOnly` but no plan touch.
- `kind == CodeOnly` but a plan touch exists.
- The reviewable-but-no-gate / non-reviewable-with-gate split
  (variants enforce reviewability via the body taxonomy:
  `PlanCommit` and `CommitBody::AdHoc` are reviewable;
  `MultiPlanCommit` is not).
- Gate-field contradictions (`state = Approved` with non-empty
  `requesters`, etc.) — those are now projection outputs, not
  stored fields.
- Plan-key / plan-touch disagreement — there's one canonical
  source per variant.

### Multi-Plan Commits

The structural invariant for "multi-plan" is "touches at least
two DISTINCT plans" — not "non-empty list of touches." Encode
that in the type via a `MultiPlanTouches` wrapper whose only
constructor validates the invariant:

```rust
pub struct MultiPlanCommit {
    pub touches: MultiPlanTouches,   // canonical, validated
}
// Reviews live on `CommitNode`. Late feedback on a multi-plan
// commit is preserved (the file IS truth); the gate projection
// returns `NonBlocking { StructurallyNonReviewable }` so it
// doesn't gate master.

/// Validated container: holds ≥2 touches naming ≥2 distinct
/// plan keys. The only constructor enforces both invariants;
/// there's no way to mutate it into an invalid state.
pub struct MultiPlanTouches {
    touches: Vec<PlanTouchSummary>,
}

impl MultiPlanTouches {
    pub fn new(touches: Vec<PlanTouchSummary>)
        -> Result<Self, MultiPlanTouchesError>
    {
        let distinct: BTreeSet<&PlanKey> =
            touches.iter().map(|t| t.plan()).collect();
        if distinct.len() < 2 {
            return Err(MultiPlanTouchesError::NotMultiPlan);
        }
        Ok(Self { touches })
    }
    pub fn as_slice(&self) -> &[PlanTouchSummary] { &self.touches }
    pub fn plans(&self) -> BTreeSet<&PlanKey> {
        self.touches.iter().map(|t| t.plan()).collect()
    }
}

impl MultiPlanCommit {
    /// Sugar over `touches.plans()`. No stored sidecar set.
    pub fn plans(&self) -> BTreeSet<&PlanKey> { self.touches.plans() }
}
```

No `plans` set field — `touches` is the canonical fact; the
plan-set is a projection. No `policy` field — the variant
encodes non-reviewability structurally. The classifier
constructs `MultiPlanTouches::new(...)` and falls back to
`PlanCommit` or `CommitBody::AdHoc` if the result would have
fewer than 2 distinct plans — making "MultiPlan with one
plan" unrepresentable.

### Ad Hoc Commits

Ad hoc commits are first-class but carry no per-variant data:
the `CommitBody::AdHoc` unit variant says everything the model
needs. Reviews live on `CommitNode.reviews` like every other
variant.

Ad hoc commits do not carry `PlanKey`s; plan association for
the UI is always empty. The discovered participant set (from
`state.ad_hoc.participants_discovered` or `config.ad_hoc_reviewers`)
and the policy (blocking vs non-blocking) are method outputs
over `(reviews, ad_hoc_state, config)`, not stored fields.

### Lifecycle Effects vs. Body

Finalize is NOT a `CommitBody` variant. A single commit can
finalize plan A and ALSO touch plan B; collapsing finalize into
a body variant forces an either-or choice and silently drops
one fact (codex on a8b19ae).

The clean split: `CommitBody` describes the commit's REVIEW
state (Plan / MultiPlan / AdHoc); lifecycle facts ride as a
separate `plan_effects: Vec<PlanEffect>` channel on
`ClassifiedCommit`. The fold applies effects to plan metadata
orthogonally from the body's review state.

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanEffect {
    /// New plan introduced in this commit's tree.
    Intro {
        plan: PlanKey,
        path: String,
        body: String,
    },
    /// Existing plan's body revised.
    Revise {
        plan: PlanKey,
        body: String,
    },
    /// Plan file removed (non-frozen plans only — frozen plans
    /// stay in state by the monotone-finished rule).
    Delete { plan: PlanKey },
    /// Finalize commit fires the freeze rule for this plan.
    /// Coexists with any body — a commit can finalize plan A
    /// AND touch plan B in the same commit.
    Finalize {
        plan: PlanKey,
        approver_count: u32,
    },
}
```

A pure-finalize commit (just lands an approving file under
`.trinity/finished/`, no plan touches, no code) has `body =
CommitBody::AdHoc(_)` and `plan_effects = [Finalize { plan: A }]`.
A "finalize A + revise B" commit has `body = CommitBody::Plan(
PlanCommit::PlanOnly { touch: B })` AND `plan_effects = [Finalize
{ plan: A }]`. Both facts survive.

The approving files themselves remain viewable by loading
`.trinity/finished/<stem>/` from the finalize commit tree.

### Convenience Methods

Methods on `CommitNode` / `CommitBody` give projection code
ergonomic reads without reintroducing parallel fields:

Projection helpers live as free functions, not methods on
`CommitNode` — they need the relevant `PlanState` (for Plan-
scoped commits) or `AdHocState` (for AdHoc), and forcing them
through `&RepoState` would just hide that dependency. Free
functions make the inputs explicit at every call site:

```rust
/// Policy projection. The relevant aggregate is `Some(plan_state)`
/// for `CommitBody::Plan(_)`, `None` for `MultiPlan` (always
/// non-reviewable), and `None` for `AdHoc` (caller pairs the
/// commit with `state.ad_hoc` itself).
pub fn review_policy(
    commit: &CommitNode,
    plan_state: Option<&PlanState>,
    ad_hoc: &AdHocState,
    config: &Config,
) -> ReviewPolicy;

/// True iff `review_policy` returns `Blocking`.
pub fn blocks_master(
    commit: &CommitNode,
    plan_state: Option<&PlanState>,
    ad_hoc: &AdHocState,
    config: &Config,
) -> bool;

/// Full derived gate view: state, approvers, requesters,
/// ambiguous, missing — all computed from `(reviews, policy)`.
/// Replaces the old `CommitGate` stored sidecar.
pub fn readiness(
    commit: &CommitNode,
    plan_state: Option<&PlanState>,
    ad_hoc: &AdHocState,
    config: &Config,
) -> ReviewReadiness;
```

`CommitNode` keeps only the cheap pure-data conveniences:

```rust
impl CommitNode {
    pub fn sha(&self) -> &CommitSha;
    pub fn subject(&self) -> &str;
    pub fn associated_plans(&self) -> BTreeSet<&PlanKey>;
    pub fn primary_plan(&self) -> Option<&PlanKey>;
    pub fn commit_kind(&self) -> CommitKind;  // value-form only
}
```

The readiness projection's output type:

```rust
pub struct ReviewReadiness {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
}
```

This is the wire/UI shape today's `api::CommitGate` is, *rebuilt
on demand* from the canonical `(reviews, policy)` pair. The
existing `api::CommitGate` type either becomes `ReviewReadiness`
(rename) or is built by projection from it — but there is no
stored field of either shape on `CommitNode`. Old call sites
that used `node.gate()` migrate to `readiness(&node, plan_state,
&state.ad_hoc, &config)`; the projection is cheap (handful of
`AgentLabel` comparisons per commit).

`CommitKind` may remain as a value-form vocabulary for filtering,
display, tests, and wire compatibility — but only as a method
return. **It must not appear as a `pub kind:` field on any
canonical struct.** The acceptance criteria below mechanize this
with a grep test.

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
    /// Parallel aggregate for commits with no plan attribution.
    pub ad_hoc: AdHocState,
}
```

`commits` answers "what happened in this commit?" `plans`
answers "what is the current folded state of this plan?" These
are NOT parallel truth — `PlanState` stores SHAs that point
into `commits`, not copies of `CommitBody` or readiness data.

## Plan State

`PlanState` is a first-class folded aggregate. It carries
exactly the per-plan facts the fold materializes as it walks
commits: ordered SHAs into `commits`, lifecycle stage,
intro/revision/implementation/finalization boundaries, plan
path + body text, and cumulative participants for the gate
projection. It does NOT copy `CommitBody`, store gate fields,
or maintain its own timeline events.

```rust
pub struct PlanState {
    pub id: PlanKey,
    pub plan_path: String,
    /// Current body text (latest revision's text, or the intro
    /// text if no revision happened). Plain `String`: git is
    /// already the durable identity layer; in-memory equality
    /// suffices for comparisons.
    pub body: String,

    pub stage: PlanStage,

    /// The plan's timeline: every commit associated with this
    /// plan in fold order. SHAs only — actual `CommitNode`s
    /// live in `RepoState.commits`. This replaces the old
    /// `Plan.timeline: Vec<PlanTimelineEvent>` (which stored
    /// duplicated event records); the SHA list points back to
    /// the canonical commit instead of copying it.
    pub timeline: Vec<CommitSha>,

    /// The commit that introduced the plan file.
    pub intro: CommitSha,

    /// Latest commit whose body revised the plan
    /// (`PlanCommit::PlanOnly | Mixed` whose `plan()` matches,
    /// or `MultiPlanCommit` whose `touches.plans()` contains
    /// this plan). `None` until the first revision lands.
    pub latest_revision: Option<CommitSha>,

    /// Latest implementation commit
    /// (`PlanCommit::CodeOnly | Mixed`).
    pub latest_implementation: Option<CommitSha>,

    /// SHA of the commit whose `plan_effects` carried
    /// `Finalize { plan: self.id }`. `Some(_)` iff the plan has
    /// frozen.
    pub finalized_at: Option<CommitSha>,

    /// Distinct feedback authors who have written on ANY
    /// commit in `timeline` so far. Seeds the participants set
    /// for the gate projection on the latest reviewable
    /// commit of this plan.
    pub participants_cumulative: BTreeSet<AgentLabel>,

    /// Max(commit.author_ts, feedback.created_at) across this
    /// plan's timeline. Folded incrementally — not a projection
    /// scan.
    pub last_activity_ts: i64,
}

pub enum PlanStage {
    /// Intro landed; no `CodeOnly`/`Mixed` commit yet.
    Drafting,
    /// At least one `PlanCommit::CodeOnly` or `Mixed` has landed.
    Implementing,
    /// `finalized_at.is_some()`.
    Frozen,
}

pub struct AdHocState {
    /// All commits with `CommitBody::AdHoc` in fold order.
    pub commits: Vec<CommitSha>,
    /// Distinct feedback authors across all ad-hoc commits —
    /// seeds the participants set for AdHoc gate projection
    /// when `config.ad_hoc_reviewers` is not set.
    pub participants_discovered: BTreeSet<AgentLabel>,
}
```

What's deliberately NOT on `PlanState`:

- `Vec<PlanTimelineEvent>` (the old shape). The plan timeline
  is `timeline: Vec<CommitSha>` — SHAs into `state.commits`,
  not duplicated event records. The plan-page UI builds
  `api::TimelineEvent` by mapping each `CommitNode` at
  projection time.
- `gate` / `readiness` / `policy` fields. Those are dynamic
  projections over `(commit, plan_state, ad_hoc_state, config)`.
- `body_hash`. Git is already the durable content-identity
  layer; in-memory body equality is a single string compare.
- `plan_intro_parent`. Derived from `RepoState.parent_of(&intro)`
  when needed.
- `archived_cycles`. Derived from `finalized_at` plus history
  if the UI ever needs cycles.

### Stale Plan References

When a `[plan-x]` prefix names a plan and that plan is later
deleted (the file removed from a non-frozen state), the
historical `PlanCommit::*` variants keep their `touch.plan() ==
plan-x` attribution. The fold is forward-only and history is
immutable; no retroactive downgrade happens. The fold removes
the corresponding `PlanState` from `state.plans` on `Delete`,
but the commits still mention the dangling key.

Projection-time behavior:

- `state.plans.get(plan-x)` returns `None`.
- Walking `state.commit_order` and matching on
  `node.body.associated_plans()` still surfaces those commits.
- The UI / WFW treats "plan key with no entry in `state.plans`"
  as a soft signal — surface the commits but don't try to look
  up the plan body.
- `AttributionWarning::DanglingPlanRef { plan }` is emitted by
  projection helpers when the lookup fails, NOT stored on the
  commit. The commit's `touch.plan()` remains the canonical
  fact.

## Projections

`PlanState` answers the common questions directly — no scans.
A handful of helpers on `RepoState` cover the cross-plan cases
the fold doesn't materialize:

```rust
impl RepoState {
    /// First-parent of `sha` in fold order, or `None` when
    /// `sha` is the root commit. Used for "what came before
    /// plan intro?" queries; not stored on `PlanState`.
    pub fn parent_of(&self, sha: &CommitSha) -> Option<&CommitSha>;

    /// Dereference a `PlanState.commits[i]` SHA into the actual
    /// `CommitNode`. Returns `None` only on internal corruption
    /// (PlanState SHA missing from `commits` map).
    pub fn commit(&self, sha: &CommitSha) -> Option<&CommitNode>;
}

impl CommitBody {
    /// Plans this commit appears in. Each variant computes
    /// from its canonical fields — no stored sidecar set.
    pub fn associated_plans(&self) -> BTreeSet<&PlanKey> {
        match self {
            CommitBody::Plan(PlanCommit::PlanOnly { touch })
            | CommitBody::Plan(PlanCommit::Mixed { touch }) => {
                [touch.plan()].into_iter().collect()
            }
            CommitBody::Plan(PlanCommit::CodeOnly { plan }) => {
                [plan].into_iter().collect()
            }
            CommitBody::MultiPlan(m) => m.touches.plans(),
            CommitBody::AdHoc => BTreeSet::new(),
        }
    }
}
```

Common queries become direct reads:

- `timeline_for_plan(plan)` → `state.plans[plan].timeline.iter()`.
- `latest_revision_for(plan)` →
  `state.plans[plan].latest_revision`.
- `latest_implementation_for(plan)` →
  `state.plans[plan].latest_implementation`.
- `finalized_at(plan)` → `state.plans[plan].finalized_at`.
- `last_activity_for(plan)` →
  `state.plans[plan].last_activity_ts`.

These are O(1) lookups on `PlanState` plus an O(1) hash hit on
`state.plans`. The fold pays for them once at insert time, not
once per query. Stays correct because the fold is the single
writer of these fields, and the fields point to the canonical
`CommitNode` via SHA — there's no second copy of body or
readiness data to drift.

If a query genuinely needs to walk a plan's timeline (e.g. for
a historical render), it dereferences `PlanState.timeline`
against `state.commits`; the `CommitNode` is the single source
of body truth.

## Tagged Enum Carry-Forward

This plan subsumes the intent of
`tagged-enums-for-non-orthogonal-fields.md`.

Already-good direction to preserve:

- `api::CommitRow` is a tagged enum of reviewable variants only.
- `api::DiffLine` is a tagged enum, so line numbers are not fake
  `Option`s.
- `api::TimelineEvent` is a tagged enum.

Do not regress these shapes back into flat structs with `kind` and
kind-dependent `Option` fields.

For `model::PlanTimelineEvent`, the final answer is stronger than
"make it a better tagged enum": remove it from canonical storage.
If a timeline event type is still useful, move it to an API or
projection module and build it from `CommitNode` variants. Do not
put gates back onto plan timeline events; the canonical review gate
belongs to the commit variant.

## Fold Algorithm

The classifier returns the full commit fact set in one
structure. Body, warnings, plan_effects, and the walk-back
update are ALL outputs of the same classifier call — no later
stage reinterprets any of them.

```rust
pub struct ClassifiedCommit {
    /// Review-body classification: which CommitBody variant this
    /// commit takes. Independent of lifecycle effects.
    pub body: CommitBody,
    /// Non-blocking attribution warnings to display to the
    /// master.
    pub warnings: Vec<AttributionWarning>,
    /// Lifecycle effects to apply to plan aggregates. Orthogonal
    /// to `body` — a commit can both finalize plan A
    /// (`plan_effects = [Finalize { plan: A, .. }]`) and revise
    /// plan B (`body = Plan(PlanOnly { touch: B })`) in the same
    /// commit. See "Lifecycle Effects vs. Body" above.
    pub plan_effects: Vec<PlanEffect>,
    /// What the walk-back chain becomes after this commit. The
    /// fold's `carry.current_effective` is assigned this verbatim.
    pub next_effective_plan: Option<PlanKey>,
}
```

`apply_commit` shape:

```rust
fn apply_commit(state: &mut RepoState, carry: &mut FoldCarry, raw: RawCommit) {
    let classified = classify(ClassifierInputs {
        raw: &raw,
        plans: &state.plans,
        carry,
    });

    let sha = raw.sha.clone();
    let node = CommitNode {
        meta: build_meta(&raw, classified.warnings),
        body: classified.body,
        reviews: CommitReviews::default(),  // filled by attach pass
    };

    // 1. Apply lifecycle effects to plan aggregates.
    for effect in &classified.plan_effects {
        apply_plan_effect(&mut state.plans, effect, &sha, &node);
    }

    // 2. Update plan aggregates from the body (commits list,
    //    latest_revision, latest_implementation, last_activity_ts).
    update_plan_aggregates(&mut state.plans, &mut state.ad_hoc, &sha, &node);

    // 3. Insert canonical commit facts.
    state.commit_order.push(sha.clone());
    state.commits.insert(sha, node);

    // 4. Update walk-back carry.
    carry.current_effective = classified.next_effective_plan;
}
```

Where:

- `apply_plan_effect` mutates `state.plans`:
  - `Intro { plan, path, body }` → insert new `PlanState` with
    `intro = sha`, `stage = Drafting`.
  - `Revise { plan, body }` → update `state.plans[plan].body`
    and `latest_revision = Some(sha)` (only if the plan exists;
    a `Revise` on a missing plan is a classifier bug).
  - `Delete { plan }` → remove `state.plans[plan]` iff the plan
    isn't frozen (`finalized_at.is_none()`); frozen plans
    survive the monotone-finished rule.
  - `Finalize { plan, approver_count }` → set
    `state.plans[plan].finalized_at = Some(sha)` and
    `stage = Frozen`.
- `update_plan_aggregates` walks `node.body.associated_plans()`
  and, for each plan, appends `sha` to `state.plans[plan].timeline`
  and updates `latest_revision` / `latest_implementation` based
  on the variant. For `CommitBody::AdHoc`, appends to
  `state.ad_hoc.commits`.
- `participants_cumulative` and `last_activity_ts` are folded
  during the feedback-attach pass (the only pass that knows
  about reviews); each attached feedback updates the relevant
  `PlanState` / `AdHocState` fields.

No later stage may reinterpret `body`, attribution, plan
membership, reviewability, or lifecycle state from a different
source.

### Multiple Finalizations in One Commit

A commit that fires the freeze rule for ≥2 plans simultaneously
is rare but representable: `plan_effects = [Finalize { plan: A,
... }, Finalize { plan: B, ... }, ...]`. The fold applies each
in `BTreeSet` order. No information is dropped (codex on
a8b19ae's `BTreeSet::iter().next()` silent-pick was rejected).

## Feedback Attachment

Feedback files on disk are truth and attach to the matching
commit's `node.reviews.feedback`. The variant determines whether
that feedback affects readiness — never whether it's stored.

- Plan-target feedback (path `.trinity/feedback/<plan>/<sha>/<author>.md`)
  attaches to `CommitBody::Plan(_)` whose `plan()` matches the
  path's plan key. If the SHA's variant is `MultiPlan` and the
  target plan is in `touches.plans()`, attachment ALSO succeeds
  (the file is truth), but the readiness projection ignores it.
  If the SHA is `AdHoc` or doesn't exist, attachment is dropped
  — the path encodes the wrong scope.
- Ad-hoc-target feedback (path `.trinity/feedback/_/<sha>/<author>.md`)
  attaches to `CommitBody::AdHoc`. Plan-attributed SHAs reject
  ad-hoc-target feedback (wrong scope, not "not gating").
- The path/scope mismatch checks happen once at attach time. No
  downstream consumer re-verifies; if it's in
  `commit.reviews.feedback`, it's authoritative.

Each attached feedback updates the relevant aggregate:

- Plan-target feedback adds the author to
  `state.plans[plan].participants_cumulative` and bumps
  `last_activity_ts` if newer.
- Ad-hoc-target feedback adds the author to
  `state.ad_hoc.participants_discovered`.

This removes the current ownership check that has to compare
`FeedbackTarget`, `CommitAttribution`, `CommitKind`, and
`gate.is_some()`. It also removes the "non-blocking means
invisible" bug class — late feedback on a multi-plan commit
(or any non-reviewable commit) is visible in the UI and the
per-commit detail view.

## Wait-For-Work Projection

WFW candidate collection reads from `PlanState` and per-commit
gate projections — not from a stored `reviewable` flag.

- Plan scope: WFW reads `state.plans[plan].latest_revision` to
  find the candidate reviewable commit (per-plan supersession
  applies by construction — only the latest revision gates).
  Calls `review_policy(node, Some(plan_state), &state.ad_hoc,
  &config)` to decide whether it's `Blocking`. Strict title-fix
  work scans `plan_state.timeline` for outstanding
  `FixCommitTitle` warnings.
- Repo scope: WFW iterates `state.plans` for the per-plan
  reviewable-commit pass (one commit per plan), then iterates
  `state.ad_hoc.commits` for ad-hoc reviews. Commits where the
  caller is in `policy.participants` and lacks a current verdict
  surface as reviewer work; `Blocking` commits with unresolved
  verdicts surface as master `AddressChanges` work.
- Non-blocking commits with pending feedback may still surface
  in the UI's "feedback you wrote / received" lists (projection
  over `commit.reviews.feedback`), but they don't gate master
  progress.

No WFW path should read `Plan.timeline` or any old `CommitGate`
field after this plan. All readiness decisions go through
`review_policy(...)` and `readiness(...)`.

## Response Projection

All response builders read `PlanState` for plan-level facts and
match on `CommitBody` for per-commit details.

Examples:

- Plan page timeline: walk `state.plans[plan].timeline` and
  convert each `CommitNode` to `api::TimelineEvent`. Includes
  `MultiPlanCommit` entries whose `touches` mention the plan
  (they're informational events in that plan's timeline; same
  as today's behavior — explicit so it doesn't silently
  regress).
- Plan revision list: same walk, filtered to
  `PlanCommit::PlanOnly | Mixed` whose `plan()` matches, plus
  `MultiPlanCommit` entries whose `touches` include the plan
  (today's `all_plan_revisions` includes `MultiPlan`; preserve
  that or call out the deliberate behavior change).
- Implementation list: same walk, filtered to
  `PlanCommit::CodeOnly | Mixed`.
- Latest reviewable commit: read
  `state.plans[plan].latest_revision`.
- Commit details: `match` on `CommitBody` directly; the variant
  determines what fields the detail response carries.

Projection can keep small helper methods for readability, but
it must not rebuild a second persistent model with different
facts.

## Implementation Phases

Each phase commits in a compiling, testable state. Phase 3 is
the fold-state replacement — the risky middle where the model
swap happens — and lands as one atomic commit. Phases 1 and 2
have already landed; their bullets are retained here for the
historical record.

### Phase 1 — Introduce New Types (additive) — LANDED

- Added `CommitMeta`, `CommitBody`, `PlanCommit`,
  `MultiPlanCommit`, `AdHocCommit` (will be deleted in Phase 3
  in favor of the unit variant), `PlanTouchSummary`,
  `CommitReviews`, `FeedbackBody`, `ReviewPolicy`,
  `NonBlockingReason`, `ReviewReadiness`, `AttributionWarning`
  (tagged enum) to `trinity_core::model`.
- `FinalizeCommit` / `CommitBody::Finalize` were added in
  Phase 1 but must be DELETED in Phase 3 — finalize is a
  `PlanEffect`, not a body variant.
- Old `CommitNode { kind, attribution, plans, gate,
  attribution_warning }` stays compiling alongside.

### Phase 2 — Classifier Returns `ClassifiedCommit` (additive) — LANDED

- Pure-function `classify(ClassifierInputs) -> ClassifiedCommit`.
- Phase 2 landed with `ClassifiedCommit { body, warnings,
  next_effective_plan }`. Phase 3 must extend it to include
  `plan_effects: Vec<PlanEffect>` and rewrite the classifier
  branches: pure-finalize commits become `body = AdHoc` plus
  `plan_effects = [Finalize { plan }]`; finalize-plus-touch
  commits carry both facts; multiple finalizations produce
  multiple effects (no silent BTreeSet pick).
- Phase 2 also needs the `AmbiguousPrefix` cleanup: when a
  named prefix lists plans but the commit doesn't touch them,
  emit a typed warning that preserves the recognized prefix
  rather than degrading to `AdHoc + AmbiguousPrefix`.

### Phase 3 — Fold-State Replacement (atomic)

This phase materializes `PlanState` as a first-class fold
output and swaps the canonical model. ONE atomic commit (or PR
landing as one squashed commit) — the deletions and consumer
migrations don't survive intermediate states.

Type-level changes:

- Introduce `PlanState`, `PlanStage`, `AdHocState`,
  `PlanEffect` to `trinity_core::model`.
- Hoist `reviews: CommitReviews` from `PlanCommit::*` /
  `MultiPlanCommit` / `AdHocCommit` to `CommitNode`. Drop the
  per-variant fields.
- Drop `FinalizeCommit` and `CommitBody::Finalize`. Replace
  `CommitBody::AdHoc(AdHocCommit)` with the unit variant
  `CommitBody::AdHoc`.
- Extend `ClassifiedCommit` with `plan_effects: Vec<PlanEffect>`.
- Change `RepoState` to `{ commit_order, commits, plans, ad_hoc }`.
  Delete `Plan` (the old metadata struct) — `PlanState` is the
  canonical replacement.
- Delete old fields on `CommitNode`: `kind`, `attribution`,
  `plans`, `gate`, `attribution_warning`.
- Delete `model::PlanTimelineEvent` from canonical state (move
  to a projection module if still useful for the API). The
  plan timeline is now `PlanState.timeline: Vec<CommitSha>` —
  same concept, no duplicated event records.
- Delete the rest of the old `Plan` fields: `last_activity_ts`,
  `plan_intro_parent`, `archived_cycles`. Each is now either a
  `PlanState` field folded incrementally
  (`last_activity_ts`), a `RepoState` helper
  (`parent_of(&intro)`), or projected on demand
  (`archived_cycles`).

Fold rewrite:

- Rewrite `apply_commit` to the shape in "Fold Algorithm" above:
  classify, apply effects to `state.plans`, update aggregates
  from the body, insert the canonical commit node, update carry.
- Rewrite feedback attachment to match on `CommitBody` and
  write into `node.reviews.feedback`, and to update
  `state.plans[plan].participants_cumulative` /
  `state.ad_hoc.participants_discovered` as a side-effect.

Consumer migration:

- Rewrite WFW candidate collection to read from `state.plans`
  (`latest_revision`, etc.) and call the free `review_policy` /
  `readiness` functions.
- Rewrite plan-page / commit-detail / timeline / preview /
  diff projections to read `PlanState.commits` for plan-scoped
  walks and `match` on `CommitBody` directly.
- Preserve API shapes (`CommitRow`, `DiffLine`,
  `TimelineEvent`); they're already tagged enums.

Cache:

- Bump `state_cache::CACHE_FORMAT_VERSION` once (from v3 → v4).
  v4 carries the new `RepoState` shape (`commits` + `plans` +
  `ad_hoc` aggregates). Old caches invalidate.
- Cache-read error semantics: any `try_load` error (bad magic,
  format-version mismatch, trinity-generation mismatch, head
  mismatch, decode failure) must delete the offending file
  before falling back to a full fold. Today the fallback is
  silent but the stale file rots in the cache dir; bake the
  delete-on-error rule into the loader so the cache is
  self-cleaning. Cache thinning is best-effort housekeeping,
  not correctness — every error path must clean up after
  itself.
- Validating wrappers (`NonEmptyVec`, `MultiPlanTouches`) need
  cache-encoding support that goes through their constructors
  (today's `wincode-derive` reads private fields and bypasses
  validation). Either gate cache-encoding behind a serde-based
  path that uses `TryFrom`, or hand-write `SchemaRead` impls
  that call the validating constructor. Decide at Phase 3
  start; don't ship Phase 3 with a wincode bypass on validated
  types.

### Phase 4 — Delete Compatibility Shims

- Remove old `CommitAttribution` if no consumer needs it. If a
  value-form is still useful for display, make it a method
  return from `CommitBody::scope()` or similar, not a stored
  field.
- Remove stale helpers that derived `plans`, `kind`, or `gate`
  from old fields (they were deleted in Phase 3 along with the
  fields; this phase just catches any residual conversion
  helpers).
- Retire `.trinity/plans/tagged-enums-for-non-orthogonal-fields.md`
  as obsolete — its intent is fully subsumed by this plan, so
  the document is deleted (not "may be deleted"). Recorded in
  this plan's `Out of Scope` as an explicit close.

## Tests

### Unit + structural

- Unit tests for `ClassifiedCommit` covering every `CommitBody`
  variant (Plan / MultiPlan / AdHoc) and every `PlanCommit`
  sub-variant, plus every `PlanEffect` shape.
- **Structural impossibilities** that must NOT compile (added as
  doc-tests with `compile_fail` or as `// @compile_fail` comment
  markers near the fixture builders):
  - constructing `PlanCommit::PlanOnly` without a touch.
  - constructing `PlanCommit::CodeOnly` with a touch (the
    variant has no `touch` field).
  - constructing `MultiPlanCommit` without going through
    `MultiPlanTouches::new(...)` (the field is private; no
    direct-field-init bypass).
  - constructing `ReviewPolicy::Blocking` with empty
    participants (use `NonEmptyVec`).
- **Runtime invariants** verified by unit tests:
  - `MultiPlanTouches::new(vec![single_touch])` returns
    `Err(NotMultiPlan)` — one-plan "multi-plan" is rejected.
  - `MultiPlanTouches::new(vec![t_a, t_a_again])` returns
    `Err(NotMultiPlan)` — two touches naming the same plan is
    rejected.
  - `MultiPlanTouches::new(vec![t_a, t_b])` succeeds.
- **Fold invariants** verified by unit tests:
  - After `apply_commit(Intro { plan: A })`,
    `state.plans[A].stage == Drafting` and
    `state.plans[A].timeline == [sha]`.
  - After `apply_commit(Finalize { plan: A })`,
    `state.plans[A].finalized_at == Some(sha)` and
    `state.plans[A].stage == Frozen`.
  - "Finalize A + revise B" commit: `state.plans[A].finalized_at
    == Some(sha)` AND `state.plans[B].latest_revision ==
    Some(sha)` AND `state.plans[B].timeline.last() ==
    Some(&sha)`. Both facts survive.
  - `Delete { plan }` removes a non-frozen plan; `Delete` on a
    frozen plan is a no-op (monotone-finished rule).
- Fixture builders: `CommitFixture::plan_only(touch)` — no
  public construction path that bypasses variant requirements.

### Acceptance grep

A CI check (or a `tests/architectural_invariants.rs` file using
`std::process::Command::new("grep")` /
`std::process::Command::new("rg")`) that asserts:

- `grep -rn "pub kind: " crates/trinity-core/src/model.rs src/`
  returns no canonical-struct hits. `kind` only appears as a
  serde tag on enums (`#[serde(tag = "kind")]`).
- `grep -rn "pub gate: " crates/trinity-core/src/model.rs src/`
  returns no hits (gate is projected via `readiness()`, never
  stored).
- `grep -rn "PlanTimelineEvent" crates/trinity-core/src/model.rs`
  returns no hits in canonical state (the type is gone; the
  ordered SHA list `PlanState.timeline` replaces it).

These run as part of `cargo test` so the next refactor can't
silently regress.

### Regression

- A reviewable plan commit always projects a non-empty
  `participants` from `readiness(...)` when its plan's
  `participants_cumulative` is non-empty.
- `MultiPlan` commits' `review_policy(...)` always returns
  `NonBlocking { reason: StructurallyNonReviewable }`.
- Late feedback on a `MultiPlan` commit appears in
  `node.reviews.feedback` (file IS truth) but does NOT cause
  `blocks_master(...)` to return true.
- Plan-scoped WFW ignores strict-title violations on other
  plans.
- Repo-scoped WFW still surfaces ad hoc commits per the
  config-derived policy.
- `PlanState.latest_revision` / `latest_implementation` match
  the previous behavior on representative histories.
- Rendered plan-page timeline output (built from
  `state.plans[plan].timeline` → `api::TimelineEvent` per
  commit) matches today's output on golden-file fixtures.
- A `[plan-x]` commit whose plan is later deleted retains its
  `touch.plan() == plan-x` attribution; the projection emits
  `AttributionWarning::DanglingPlanRef` rather than rewriting
  history.

### Wire / shape

- Wire snapshot tests for every response shape that touches a
  `CommitBody` variant: `WaitWorkPayload` (incl. `ReviewReadiness`
  if it appears on the wire), `PlanRow`, `PlanDetailResponse`,
  `CommitDetail`, `TimelineEvent`.
- Doc-comments on `CommitBody` explain the `scope` vs. inner
  `kind` discriminator choice. (Reviewer ergonomics — the
  rationale lives next to the code, not just in this plan.)

### CI

- `cargo test --workspace --exclude trinity-frontend`.
- `cargo test -p trinity-frontend` if frontend types change.
- `cargo fmt -- --check`.

## Acceptance

- `RepoState` is exactly four fields: `commit_order`, `commits`,
  `plans`, `ad_hoc`. No second canonical commit event log; no
  parallel plan metadata struct.
- `CommitNode` stores `meta` + `body` + `reviews`. No `kind`,
  no `attribution`, no `plans`, no `gate`, no
  `attribution_warning` field. Warnings are on `meta.warnings`
  (typed enum).
- `CommitBody` is `Plan(PlanCommit) | MultiPlan(MultiPlanCommit)
  | AdHoc` (the AdHoc variant is a unit variant; finalize is
  not a body variant).
- Lifecycle changes ride the `plan_effects: Vec<PlanEffect>`
  channel on `ClassifiedCommit`. A commit can finalize plan A
  AND touch plan B in the same commit; both facts survive in
  the fold output (`state.plans[A].finalized_at == Some(sha)`
  AND `state.plans[B].latest_revision == Some(sha)`).
- `PlanState` is the canonical folded plan aggregate, holding
  `{ id, plan_path, body, stage, timeline: Vec<CommitSha>,
  intro, latest_revision, latest_implementation, finalized_at,
  participants_cumulative, last_activity_ts }`. No copied
  `CommitBody`, no copied gate/readiness, no `body_hash`.
- Review storage is universal: every commit carries
  `reviews: CommitReviews` on the node. Whether a commit blocks
  master is determined by `review_policy(node, plan_state,
  ad_hoc, config)`, NOT by whether the variant has a `gate`
  field. `MultiPlanCommit`'s policy projection returns
  `NonBlocking { StructurallyNonReviewable }`.
- Policy is a free function over `(commit, plan_state, ad_hoc,
  config)`, NOT a stored field. Config snapshotted at trinity
  startup; restart re-folds.
- `ReviewReadiness` is the derived projection type that
  replaces the old `CommitGate` sidecar. No canonical
  `CommitGate` field on any struct.
- Feedback files attach when the SHA exists AND the feedback
  path's target scope matches the commit's variant/plan. After
  attachment, the policy projection decides whether that
  feedback affects readiness; "non-blocking" or "structurally
  non-reviewable" is never a reason to hide or drop well-
  targeted feedback. Path/scope mismatches are dropped.
- WFW, response projection, feedback attachment, and lifecycle
  derivation all read through `state.plans` / `state.ad_hoc`
  and `CommitBody` variants via `review_policy(...)` /
  `readiness(...)`.
- The grep-mechanized invariants (`pub kind:`, `pub gate:`,
  `PlanTimelineEvent` returning zero hits in `model.rs`) pass
  as part of `cargo test`.
- `tagged-enums-for-non-orthogonal-fields.md` is deleted from
  `.trinity/plans/` as obsolete.

## Out of Scope

- Changing the user-facing workflow semantics of approvals,
  request-changes, or force-finish.
- Adding a new database or migration layer.
- Adding any derived-index cache (per-plan or otherwise). Direct
  projections only; performance work is its own future plan if
  Trinity is ever measured to be slow.
- Live config reload — config is snapshotted at trinity startup.
  Changing config requires restart. A future plan can add live
  reload if the workflow demands it; the model design here
  doesn't preclude it (policy is a projection from config).
- Frontend UX changes around dangling-plan attribution
  (commits whose `[plan-x]` prefix names a deleted plan). The
  projection emits `AttributionWarning::DanglingPlanRef`; how
  the UI renders it is a separate question.
- Removing the web UI.
