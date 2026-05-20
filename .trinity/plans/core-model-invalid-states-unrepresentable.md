# core-model-invalid-states-unrepresentable

## Summary

Make the core Trinity model unable to represent the contradictions
we keep reviewing by hand. Large mutable folds are acceptable; the
problem is not mutation. The problem is parallel representations of
the same fact:

- `CommitNode.kind`
- `CommitNode.attribution`
- `CommitNode.plans`
- `CommitNode.gate`
- `Plan.timeline`

These fields can disagree. The current implementation has worked
hard to keep them aligned, but the type system still permits states
like "ad hoc commit with plan membership", "reviewable kind with no
gate", "non-reviewable kind with a gate", or "commit attributed to
plan A while plan B's timeline contains it".

This plan replaces those parallel fields with a single canonical
commit stream whose variants carry exactly the data valid for that
commit kind. Plans become metadata plus derived views over the
commit stream, not a second canonical event log.

This plan also absorbs the goal of
`tagged-enums-for-non-orthogonal-fields.md`: when a shape varies by
kind, model it as a tagged enum with variant-specific fields. Some
of that work has already landed for `CommitRow`, `DiffLine`, and
`PlanTimelineEvent`; this plan keeps that direction but extends it
to the core commit model and removes remaining duplicate truth.

## Hard Direction

After this plan:

- `RepoState.commit_order + RepoState.commits` is the only canonical
  chronological stream.
- `Plan` does not store a canonical `timeline: Vec<_>`.
- Plan timelines, latest reviewable commits, plan revisions,
  implementation commits, and UI timeline rows are projections from
  the commit stream.
- `CommitNode` is not a flat struct with `kind`, `attribution`,
  `plans`, and `gate: Option<_>`.
- A reviewable commit's review state is computed from
  `(commit.reviews, commit.review_policy(&config, &state))`,
  not from a stored gate sidecar that can drift.
- A non-reviewable commit (`MultiPlan` / `Finalize`) can still
  carry feedback files (they're truth), but the policy
  projection returns `NonBlocking { StructurallyNonReviewable }`
  so they don't gate master.
- A plan-only commit cannot exist without plan-touch data.
- A code-only commit cannot carry plan-touch data.
- A mixed commit must carry both the plan touch and the non-plan
  code attribution.
- Ad hoc commits cannot accidentally name plans.
- Finalize commits cannot accidentally be review targets.
- Multi-plan commits cannot accidentally be treated as single-plan
  review targets.

The fold may remain one large mutable `apply_commit` function if
that keeps the algorithm readable. The important requirement is
that each commit is classified once into a structurally valid
variant, and every downstream projection reads that variant.

## Target Types

### Commit Metadata

Shared fields stay shared:

```rust
pub struct CommitNode {
    pub meta: CommitMeta,
    pub body: CommitBody,
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
    AdHoc(AdHocCommit),
    Finalize(FinalizeCommit),
}
```

The wire shape becomes `{"scope": "plan", "kind": "plan_only",
"plan": "...", ...}` — operator-readable and conflict-free.

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
pub enum ReviewPolicy<'a> {
    Blocking { participants: NonEmptyVec<AgentLabel> },
    NonBlocking { reason: NonBlockingReason },
}

pub enum NonBlockingReason {
    NoParticipants,
    ConfigDisabledPlanReview,
    ConfigDisabledMiscReview,
    /// The commit's variant is structurally non-reviewable
    /// (`MultiPlan` / `Finalize`). Method projection returns
    /// this for those variants.
    StructurallyNonReviewable,
}

impl CommitNode {
    pub fn review_policy(&self, config: &Config, state: &RepoState)
        -> ReviewPolicy<'_>;
}
```

`Blocking` uses `NonEmptyVec` so "blocking with empty
participants" is unrepresentable. When the discovered participant
set is empty, the projection returns `NonBlocking { NoParticipants }`
instead.

`participants` is also a projection — for plan commits it's
"distinct feedback authors across this plan's chronological
chain up to and including this commit"; for ad hoc commits it's
"branch feedback authors at fold time, or the config override
`ad_hoc_reviewers` if set." Neither is stored canonically.

`state` / `approvers` / `requesters` / `ambiguous` / `missing`
are all method outputs over `(reviews, policy)` — never stored.
This eliminates the `state = Approved && requesters != []` class
of contradiction by construction.

**Separation of storage and gating.** Feedback files on disk are
always truth; they attach to the commit's `CommitReviews`
unconditionally — every variant, including `MultiPlan` and
`Finalize`. The policy projection decides whether they gate
anything. Late feedback on a `MultiPlan` or `Finalize` commit is
preserved (the file IS truth) and surfaces in the UI; it just
returns `StructurallyNonReviewable` from `review_policy()`, so it
doesn't block master. This removes the old "non-blocking means
the file is invisible" bug class.

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
        reviews: CommitReviews,
    },
    CodeOnly {
        plan: PlanKey,             // no touch; key must be stored
        reviews: CommitReviews,
    },
    Mixed {
        touch: PlanTouchSummary,   // touch.plan is the plan key
        reviews: CommitReviews,
    },
}

impl PlanCommit {
    /// Single source of plan identity per variant.
    pub fn plan(&self) -> &PlanKey {
        match self {
            PlanCommit::PlanOnly { touch, .. }
            | PlanCommit::Mixed { touch, .. } => touch.plan(),
            PlanCommit::CodeOnly { plan, .. } => plan,
        }
    }
}
```

No `policy` field on PlanCommit variants. Policy is a projection
method on `CommitNode` driven by the snapshotted config (see
"Review Storage" above and "Convenience Methods" below).

This eliminates:

- `kind == PlanOnly` but no plan touch.
- `kind == CodeOnly` but a plan touch exists.
- The reviewable-but-no-gate / non-reviewable-with-gate split
  (variants enforce reviewability via their existence in
  `PlanCommit` and `AdHocCommit`).
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
    /// Late feedback files written against a multi-plan commit
    /// are preserved (the file IS truth); the policy projection
    /// returns `NonBlocking { StructurallyNonReviewable }` for
    /// the variant, so they're informational only.
    pub reviews: CommitReviews,
}

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
`PlanCommit`/`AdHocCommit`/`FinalizeCommit` if the result would
have fewer than 2 distinct plans — making "MultiPlan with one
plan" unrepresentable.

### Ad Hoc Commits

Ad hoc commits are first-class. Their `reviews` always exist
(feedback files on disk are truth); reviewability/policy is a
method projection from the snapshotted config:

```rust
pub struct AdHocCommit {
    pub reviews: CommitReviews,
}
```

Nothing else. Ad hoc commits do not carry `PlanKey`s; plan
association for the UI is always empty. The discovered
participant set (from branch feedback authors) and the policy
(blocking vs non-blocking) are method outputs over
`(reviews, config, branch_state)`, not stored fields.

### Finalize Commits

```rust
pub struct FinalizeCommit {
    pub plan: PlanKey,
    pub approver_count: u32,
    /// As with multi-plan: late feedback files written against
    /// the freeze commit are preserved but project as
    /// `NonBlocking { StructurallyNonReviewable }`.
    pub reviews: CommitReviews,
}
```

The variant implies non-reviewability structurally. The approving
files themselves remain viewable by loading
`.trinity/finished/<stem>/` from the finalize commit tree.

### Convenience Methods

Methods on `CommitNode` / `CommitBody` give projection code
ergonomic reads without reintroducing parallel fields:

```rust
impl CommitNode {
    pub fn sha(&self) -> &CommitSha;
    pub fn subject(&self) -> &str;
    pub fn associated_plans(&self) -> BTreeSet<&PlanKey>;
    pub fn primary_plan(&self) -> Option<&PlanKey>;

    /// Canonical review activity (feedback files). Always present
    /// on every variant.
    pub fn reviews(&self) -> &CommitReviews;
    pub fn reviews_mut(&mut self) -> &mut CommitReviews;

    /// Policy projection from (variant, snapshotted config,
    /// participant discovery). Always callable; the return value
    /// determines whether this commit blocks master.
    pub fn review_policy(&self, config: &Config, state: &RepoState)
        -> ReviewPolicy;

    /// True iff `review_policy(...)` returns `Blocking`. Sugar
    /// over `review_policy()`; never inspects stored fields
    /// directly.
    pub fn blocks_master(&self, config: &Config, state: &RepoState)
        -> bool;

    /// Full derived gate view: state, approvers, requesters,
    /// ambiguous, missing — all computed from `(reviews,
    /// review_policy(...))`. Replaces the old `CommitGate`
    /// stored sidecar.
    pub fn readiness(&self, config: &Config, state: &RepoState)
        -> ReviewReadiness;

    pub fn commit_kind(&self) -> CommitKind; // value-form only
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
that used `node.gate()` migrate to `node.readiness(&config,
&state)`; the projection is cheap (handful of AgentLabel
comparisons per commit).

`CommitKind` may remain as a value-form vocabulary for filtering,
display, tests, and wire compatibility — but only as a method
return. **It must not appear as a `pub kind:` field on any
canonical struct.** The acceptance criteria below mechanize this
with a grep test.

## Plan State

`Plan` stores plan metadata only. Body + hash get wrapped so they
can't disagree; activity timestamp is a projection method, not
a stored field:

```rust
pub struct Plan {
    pub id: PlanKey,
    pub plan_path: String,
    pub body: PlanBody,
    pub plan_intro: CommitSha,
    // No `plan_intro_parent` — derive from `commit_order` via
    // `RepoState::parent_of(&plan_intro)`. It's the commit
    // immediately before `plan_intro` in fold order (or `None`
    // when plan_intro is the root commit). Same parallel-truth
    // logic that killed `last_activity_ts`.
}

/// Body + hash that can never disagree. The only constructor
/// computes the hash from the body bytes; there is no setter
/// that updates one without the other.
pub struct PlanBody {
    text: String,
    hash: ContentHash,
}

impl PlanBody {
    pub fn new(text: String) -> Self {
        let hash = content_hash(&text);
        Self { text, hash }
    }
    pub fn text(&self) -> &str { &self.text }
    pub fn hash(&self) -> &ContentHash { &self.hash }
}
```

**Removed from canonical `Plan`**:
- `timeline: Vec<PlanTimelineEvent>` — derived from
  `RepoState.commit_order` filtered by `node.associated_plans()`.
- `last_activity_ts: i64` — call
  `RepoState::last_activity_for(&plan)`; walks commits +
  feedback mtimes at projection time.
- `plan_intro_parent: Option<CommitSha>` — call
  `RepoState::parent_of(&plan_intro)`; the commit immediately
  before `plan_intro` in `commit_order`.
- `archived_cycles: Vec<ArchivedCycle>` — derived from the
  Finalize commit variant on the commit stream.

None of these become a stored sidecar. See "Direct Projections"
below for the method shapes; no indexes get added until
profiling demonstrates a hot path.

### Stale Plan References

When a `[plan-x]` prefix names a plan and that plan is later
deleted (its file removed from a non-frozen state), the historical
`PlanCommit::*` variants keep their `touch.plan() == plan-x`
attribution. The fold is forward-only and history is immutable;
no retroactive downgrade happens.

Projection-time behavior:

- `state.commits_for_plan(plan-x)` returns those historical
  commits even though `state.plans.get(plan-x).is_none()`.
- The UI / WFW must treat "plan key with no entry in
  `state.plans`" as a soft signal — surface the commits but
  don't try to look up the plan body. A future plan can decide
  whether to render this as a warning or hide the dangling key.
- `attribution_warning::DanglingPlanRef { plan: PlanKey }` is
  emitted by the relevant projection helpers when the lookup
  fails, NOT stored on the commit. The commit's `touch.plan()`
  remains the canonical fact.

## Direct Projections (No Indexes)

**Direct projections only.** Every status / lifecycle / latest-
reviewable / activity query is a method on `RepoState` that walks
`commit_order`. No cached per-plan maps, no derived indexes, no
sidecar structures. Trinity has tens of plans and hundreds of
commits per repo; a linear scan is microseconds and stays
correct.

Method shape:

```rust
impl RepoState {
    /// Commits associated with `plan` in fold order.
    pub fn commits_for_plan(&self, plan: &PlanKey)
        -> impl Iterator<Item = &CommitNode>;

    /// Latest reviewable commit attributed to `plan` (reverse
    /// scan over `commit_order`).
    pub fn latest_reviewable_for_plan(&self, plan: &PlanKey)
        -> Option<&CommitNode>;

    /// The Finalize commit SHA for `plan`, if it has frozen.
    pub fn finalized_at(&self, plan: &PlanKey) -> Option<&CommitSha>;

    /// Max(commit.author_ts, feedback.created_at) for `plan`.
    pub fn last_activity_for(&self, plan: &PlanKey) -> i64;

    /// First-parent of `sha` in fold order, or `None` when `sha`
    /// is the root commit / not in this state. Replaces
    /// `Plan.plan_intro_parent` as a stored field.
    pub fn parent_of(&self, sha: &CommitSha) -> Option<&CommitSha>;
}
```

If profiling ever surfaces a hot projection on a real-world
repo, *then* a derived index earns its keep — with a co-located
equality test against the direct projection it caches. Until
then, premature index proliferation reintroduces the parallel-
truth problem this plan exists to remove.

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

Keep the fold shape, but make classification return the full commit
variant and next walk-back state:

```rust
pub struct ClassifiedCommit {
    pub body: CommitBody,
    pub warnings: Vec<AttributionWarning>,
    pub next_effective_plan: Option<PlanKey>,
    pub plan_effects: Vec<PlanEffect>,
}
```

`apply_commit` should:

1. Read the current fold carry.
2. Apply plan-file tree changes to plan metadata.
3. Classify the commit once into `ClassifiedCommit`.
4. Insert exactly one `CommitNode`.
5. Update carry from `classified.next_effective_plan`.

No later stage may reinterpret `kind`, attribution, plan
membership, or reviewability from a different source.

## Feedback Attachment

Feedback files on disk are truth and attach **unconditionally**
to the matching commit's `reviews.feedback`. The variant
determines whether that feedback affects readiness — never
whether it's stored.

- Plan-target feedback (path `.trinity/feedback/<plan>/<sha>/<author>.md`)
  attaches to `CommitBody::Plan(_)` whose `plan()` matches the
  path's plan key. If the SHA's variant is `MultiPlan` or
  `Finalize`, attachment ALSO succeeds (the file is truth), but
  the readiness projection ignores it. If the SHA is `AdHoc` or
  doesn't exist, attachment is dropped — the path encodes the
  wrong scope.
- Ad-hoc-target feedback (path `.trinity/feedback/_/<sha>/<author>.md`)
  attaches to `CommitBody::AdHoc(_)`. Plan-attributed SHAs reject
  ad-hoc-target feedback (wrong scope, not "not gating").
- The path/scope mismatch checks happen once at attach time. No
  downstream consumer re-verifies; if it's in
  `commit.reviews.feedback`, it's authoritative.

This removes the current ownership check that has to compare
`FeedbackTarget`, `CommitAttribution`, `CommitKind`, and
`gate.is_some()`. It also removes the "non-blocking means
invisible" bug class — late feedback on a multi-plan or finalize
commit is visible in the UI and the per-commit detail view.

## Wait-For-Work Projection

WFW candidate collection walks `commit_order` and inspects each
commit's *policy projection*, not a variant-encoded
"reviewability" flag:

- Repo scope: for each commit, call
  `node.review_policy(&config, &state)`. Commits where the
  caller (`author`) is in the policy's `participants` and lacks a
  current verdict surface as reviewer work; commits where the
  policy is `Blocking` with unresolved verdicts surface as master
  `AddressChanges` work. Per-plan supersession applies as a
  projection rule (only the latest reviewable commit per plan
  gates).
- Plan scope: filter to commits whose `primary_plan()` or
  `associated_plans()` contains the requested plan. Same policy
  projection runs.
- Strict title-fix work (`require_commit_prefix = true`): commits
  in the same scope, regardless of policy outcome, because title
  hygiene is a commit-level rule independent of reviewability.
  Reviewers do not surface for commits with an outstanding
  `FixCommitTitle`.
- Non-blocking commits with pending feedback may still surface in
  the UI's "feedback you wrote / received" lists (projection over
  `commit.reviews.feedback`), but they don't gate master
  progress.

No WFW path should read `Plan.timeline` or any old `CommitGate`
field after this plan. All readiness decisions go through
`node.review_policy(...)` and `node.readiness(...)`.

## Response Projection

All response builders project from `CommitNode` variants.

Examples:

- Plan page timeline: `state.commits_for_plan(&plan)` and convert
  each `CommitNode` to `api::TimelineEvent`. Includes
  `MultiPlanCommit` entries whose `touches` mention the plan
  (they're informational events in that plan's timeline; same
  as today's behavior — explicit so it doesn't silently
  regress).
- Plan revision list: select `PlanCommit::PlanOnly` and
  `PlanCommit::Mixed` whose `plan()` matches, plus
  `MultiPlanCommit` entries whose `touches` include the plan
  (today's `all_plan_revisions` includes `MultiPlan`; preserve
  that or call out the deliberate behavior change).
- Implementation list: `PlanCommit::CodeOnly` and
  `PlanCommit::Mixed` for the plan.
- Latest reviewable commit: reverse-scan `commits_for_plan(&plan)`
  and return the first `PlanCommit` variant. (MultiPlan and
  Finalize aren't reviewable by their variant existence.)
- Commit details: `match` on `CommitBody` directly; the variant
  determines what fields the detail response carries.

Projection can keep small helper methods for readability, but it
must not rebuild a second persistent model with different facts.

## Implementation Phases

The phase boundaries are designed so each commit leaves the tree
in a compiling, testable state. Phases 3 + 4 + 5 are the
risk-concentrated middle and may land as one atomic commit OR as
sub-steps inside a single PR; the boundaries below are
descriptive, not contractual.

### Phase 1 — Introduce New Types (additive)

- Add `CommitMeta`, `CommitBody`, `PlanCommit`,
  `MultiPlanCommit`, `AdHocCommit`, `FinalizeCommit`,
  `PlanTouchSummary`, `CommitReviews`, `FeedbackBody`,
  `ReviewPolicy`, `NonBlockingReason`, `ReviewReadiness`, and
  `AttributionWarning` (tagged enum) to `trinity_core::model`.
- Old `CommitNode { kind, attribution, plans, gate,
  attribution_warning }` shape stays compiling alongside.
- Tests pass against the old shape.

### Phase 2 — Classifier Returns `ClassifiedCommit` (additive)

- Move prefix parsing, file-touch attribution, finalize
  detection, ad hoc participant discovery, and walk-back update
  into one classifier returning `ClassifiedCommit { body,
  warnings, next_effective_plan, plan_effects }`.
- Old `apply_commit` continues to populate old `CommitNode`
  fields; the classifier output is consumed in parallel and its
  result is asserted equal to the legacy classification.
- Tests for every `CommitBody` variant + walk-back behavior:
  - `[misc]` touching a plan does not seed that plan as the
    effective chain.
  - `[plan]` code-only commit becomes a `PlanCommit::CodeOnly`.
  - unknown prefix becomes `AdHocCommit` with
    `AttributionWarning::UnknownPlanPrefix`.
  - multi-plan touch produces `MultiPlanCommit` (no policy
    field; structurally non-reviewable).
  - finalize commit produces `FinalizeCommit` (no policy field).

### Phase 3 — Replace Canonical `CommitNode` + Remove `Plan.timeline` (atomic)

This phase is the model swap. It MUST be one atomic commit (or
PR landing as one squashed commit) because the deletions and
the consumer migrations don't survive intermediate states.

- Change `RepoState.commits` to store the new `CommitNode`.
- Delete old fields: `kind`, `attribution`, `plans`, `gate`,
  `attribution_warning`. Delete `Plan.timeline`,
  `Plan.last_activity_ts`, `Plan.plan_intro_parent`,
  `Plan.archived_cycles`. Delete `model::PlanTimelineEvent`
  from canonical state (it may move to a projection module if
  still useful for the API).
- Rewrite feedback attachment to match on `CommitBody` and
  write into `commit.reviews.feedback` unconditionally per the
  Feedback Attachment section.
- Rewrite WFW candidate collection, plan-page / commit-detail /
  timeline / preview / diff projections to read from
  `CommitBody` variants and call `node.review_policy(...)` +
  `node.readiness(...)`.
- Preserve API shapes (`CommitRow`, `DiffLine`,
  `TimelineEvent`); they're already tagged enums.
- Bump `state_cache::CACHE_FORMAT_VERSION` once (from v3 → v4).
  v4 carries the new `CommitNode` shape; `Plan.timeline` is
  gone in v4. Old caches invalidate; the loader's delete-on-
  error rule (below) collects them.
- Cache-read error semantics: any `try_load` error (bad magic,
  format-version mismatch, trinity-generation mismatch, head
  mismatch, decode failure) must delete the offending file
  before falling back to a full fold. Today the fallback is
  silent but the stale file rots in the cache dir; bake the
  delete-on-error rule into the loader so the cache is
  self-cleaning. Cache thinning is best-effort housekeeping,
  not correctness — every error path must clean up after
  itself.

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
  variant (Plan / MultiPlan / AdHoc / Finalize) and every
  `PlanCommit` sub-variant.
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
  - creating a `PlanBody` whose hash doesn't match the text
    (the only constructor computes the hash).
- **Runtime invariants** verified by unit tests:
  - `MultiPlanTouches::new(vec![single_touch])` returns
    `Err(NotMultiPlan)` — one-plan "multi-plan" is rejected.
  - `MultiPlanTouches::new(vec![t_a, t_a_again])` returns
    `Err(NotMultiPlan)` — two touches naming the same plan is
    rejected.
  - `MultiPlanTouches::new(vec![t_a, t_b])` succeeds.
- Fixture builders: `CommitFixture::plan_only(touch).reviews(...)`
  — no public construction path that bypasses variant
  requirements.

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
- `grep -rn "pub timeline: " crates/trinity-core/src/model.rs`
  returns no hits on `Plan`.

These run as part of `cargo test` so the next refactor can't
silently regress.

### Regression

- A reviewable plan commit always projects a non-empty
  `participants` from `node.readiness(...)` when its plan has
  at least one historical feedback author (cumulative chain).
- Multi-plan and finalize commits' `node.review_policy(...)`
  always returns
  `NonBlocking { reason: StructurallyNonReviewable }`.
- Late feedback on a multi-plan commit appears in
  `node.reviews().feedback` (file IS truth) but does NOT cause
  `node.blocks_master(...)` to return true.
- Plan-scoped WFW ignores strict-title violations on other plans.
- Repo-scoped WFW still surfaces ad hoc commits per the
  config-derived policy.
- Latest plan revision / latest implementation commit are
  derived from `commits_for_plan(&plan)` and match the previous
  behavior on representative histories.
- Deleting `Plan.timeline` does not change rendered plan-page
  timeline output (golden-file test on representative repo
  fixtures).
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

- One canonical chronological representation:
  `RepoState.commit_order + RepoState.commits`. No second
  canonical event log anywhere in the model.
- `Plan` stores only `{ id, plan_path, body: PlanBody,
  plan_intro }`. No `timeline`, no `last_activity_ts`, no
  `plan_intro_parent`, no `archived_cycles`.
- `CommitNode` stores `meta` + `body`. No `kind`, no
  `attribution`, no `plans`, no `gate`, no `attribution_warning`
  field. Warnings are on `meta.warnings` (typed enum).
- Review storage is universal: every commit variant carries
  `CommitReviews`. Whether a commit blocks master is determined
  by `node.review_policy(&config, &state)`, NOT by whether the
  variant has a `gate` field. `MultiPlanCommit` and
  `FinalizeCommit` exist but their policy projection returns
  `NonBlocking { StructurallyNonReviewable }`.
- Policy is a projection method `node.review_policy(&config,
  &state)`, NOT a stored field. Config snapshotted at trinity
  startup; restart re-folds.
- `ReviewReadiness` is the derived projection type that replaces
  the old `CommitGate` sidecar. No canonical `CommitGate` field
  on any struct.
- Feedback files attach when the SHA exists AND the feedback
  path's target scope matches the commit's variant/plan. After
  attachment, the policy projection decides whether that
  feedback affects readiness; "non-blocking" or "structurally
  non-reviewable" is never a reason to hide or drop well-
  targeted feedback. Path/scope mismatches are dropped (the
  path encodes the wrong commit).
- WFW, response projection, feedback attachment, and lifecycle
  derivation all read through `CommitBody` variants and
  `node.review_policy(...)` / `node.readiness(...)`.
- The grep-mechanized invariants (`pub kind:`, `pub gate:`,
  `pub timeline:` returning zero hits) pass as part of `cargo
  test`.
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
