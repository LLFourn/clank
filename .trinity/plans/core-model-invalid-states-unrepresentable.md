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
- A reviewable commit cannot exist without a `CommitGate`.
- A non-reviewable commit cannot carry a `CommitGate`.
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

### Review Storage — Minimal Truth, Derived Readiness

`CommitGate` today is a flat derived-state bag: `state`,
`participants`, `approvers`, `requesters`, `ambiguous`,
`missing`, `feedback`. Those fields can disagree just as badly
as the commit fields do (`state = Approved` with non-empty
`requesters`, or `missing` that doesn't match
`participants - voters`).

Replace it with two minimal-truth structures:

```rust
pub struct CommitReviews {
    /// Only canonical fact: who wrote what on this commit.
    /// Feedback files on disk are truth; we mirror them once.
    pub feedback: BTreeMap<AgentLabel, Feedback>,
}

#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewPolicy {
    /// Master is blocked until the participant set's verdicts
    /// resolve. Default for plan commits and (when configured)
    /// for ad hoc commits with a discovered reviewer set.
    Blocking { participants: Vec<AgentLabel> },
    /// Reviews can be written and surface in the UI, but they
    /// don't gate the master. Used when
    /// `force_review_on_misc_commits = false` or
    /// `force_review_on_plan_commits = false`.
    NonBlocking { reason: NonBlockingReason },
    /// The commit's variant doesn't carry review semantics
    /// (multi-plan, finalize). Feedback files written against
    /// it are preserved structurally — operators can still
    /// inspect them — but they have no policy effect.
    NotReviewable { reason: NotReviewableReason },
}
```

`state` / `approvers` / `requesters` / `ambiguous` / `missing`
are method outputs over `(CommitReviews, ReviewPolicy)`, NOT
stored fields. This eliminates the `state = Approved && requesters
!= []` class of contradiction by construction.

**Separation of storage and gating.** Feedback files on disk are
always truth; they attach to the commit's `CommitReviews`
regardless of `ReviewPolicy`. Late feedback on a
`NotReviewable` (multi-plan/finalize) commit is preserved as
informational metadata — the UI can show it; readiness
projections just ignore it. This removes the old "non-blocking
means the file is invisible" bug class.

### Plan Commits

Use nested variants, not `PlanCommit { kind: PlanCommitKind, ... }`.
The variants imply different data. Each reviewable plan commit
carries both `reviews` (canonical) and `policy` (configurable);
together they project the gate.

```rust
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanCommit {
    PlanOnly {
        plan: PlanKey,
        touch: PlanTouchSummary,
        reviews: CommitReviews,
        policy: ReviewPolicy,
    },
    CodeOnly {
        plan: PlanKey,
        reviews: CommitReviews,
        policy: ReviewPolicy,
    },
    Mixed {
        plan: PlanKey,
        touch: PlanTouchSummary,
        reviews: CommitReviews,
        policy: ReviewPolicy,
    },
}
```

This eliminates:

- `kind == PlanOnly` but no plan touch.
- `kind == CodeOnly` but a plan touch exists.
- The reviewable-but-no-gate / non-reviewable-with-gate split
  (variants enforce reviewability).
- Gate-field contradictions (`state = Approved` with non-empty
  `requesters`, etc.) — those are now projection outputs, not
  stored fields.

### Multi-Plan Commits

```rust
pub struct MultiPlanCommit {
    pub plans: BTreeSet<PlanKey>,
    pub touches: Vec<PlanTouchSummary>,
    /// Late feedback files written against a multi-plan commit
    /// are preserved (the file IS truth) but have no policy
    /// effect — `policy` is implicitly `NotReviewable { reason:
    /// MultiPlan }` and never carried explicitly.
    pub reviews: CommitReviews,
}
```

No `policy` field — the variant itself encodes
`NotReviewable { reason: MultiPlan }`. If a future plan makes
multi-plan commits reviewable, it must introduce a new explicit
variant with a `policy` field and a defined reviewer-set model.
Do not smuggle reviewability in through `Option<CommitGate>`.

### Ad Hoc Commits

Ad hoc commits are first-class. Their `reviews` always exist
(feedback files on disk are truth); their `policy` is what
configuration decides:

```rust
pub struct AdHocCommit {
    pub reviews: CommitReviews,
    pub policy: ReviewPolicy,
}
```

`policy` may be `Blocking { participants }` (default when an
ad-hoc reviewer set is discovered or pinned),
`NonBlocking { reason: ConfigDisabled }` (when
`force_review_on_misc_commits = false`), or
`NonBlocking { reason: NoParticipants }` (when no reviewer set
can be derived AND no override). Either way, feedback files
written manually against the commit are preserved in
`reviews.feedback` — invisible-feedback is the bug we're
removing.

Ad hoc commits do not carry `PlanKey`s. Plan association for the
UI is always empty.

### Finalize Commits

```rust
pub struct FinalizeCommit {
    pub plan: PlanKey,
    pub approver_count: u32,
    /// As with multi-plan: late feedback files written against
    /// the freeze commit are preserved but have no policy effect.
    pub reviews: CommitReviews,
}
```

No `policy`. The variant implies
`NotReviewable { reason: Finalize }`. The approving files
themselves remain viewable by loading `.trinity/finished/<stem>/`
from the finalize commit tree.

### Convenience Methods

Add methods on `CommitNode` / `CommitBody` for value-form reads so
projection code stays simple without reintroducing parallel fields:

```rust
impl CommitNode {
    pub fn sha(&self) -> &CommitSha;
    pub fn subject(&self) -> &str;
    pub fn associated_plans(&self) -> BTreeSet<&PlanKey>;
    pub fn primary_plan(&self) -> Option<&PlanKey>;
    pub fn gate(&self) -> Option<&CommitGate>;
    pub fn gate_mut(&mut self) -> Option<&mut CommitGate>;
    pub fn commit_kind(&self) -> CommitKind; // value-form only
    pub fn is_reviewable(&self) -> bool;
}
```

`CommitKind` may remain as a value-form vocabulary for filtering,
display, tests, and wire compatibility. It must not be stored next
to a variant that already determines the kind.

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
    pub plan_intro_parent: Option<CommitSha>,
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
- `archived_cycles: Vec<ArchivedCycle>` — derived from the
  Finalize commit variant on the commit stream.

None of these become a stored sidecar. See "Direct Projections"
below for the method shapes; no indexes get added until
profiling demonstrates a hot path.

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

Feedback attachment should pattern-match on commit variants:

- Plan feedback may attach only to
  `CommitBody::Plan(PlanCommit::{PlanOnly, CodeOnly, Mixed})`
  whose `plan` matches the feedback path.
- Ad hoc feedback may attach only to
  `CommitBody::AdHoc(AdHocCommit::Reviewable { .. })`.
- Multi-plan and finalize commits reject live feedback
  structurally.

This removes the current ownership check that has to compare
`FeedbackTarget`, `CommitAttribution`, `CommitKind`, and
`gate.is_some()`.

## Wait-For-Work Projection

WFW candidate collection should walk `commit_order` and inspect
`CommitBody`:

- Repo scope: all reviewable `PlanCommit` and reviewable
  `AdHocCommit` variants in fold order, with per-plan supersession
  applied as a projection rule.
- Plan scope: commits whose `primary_plan()` or
  `associated_plans()` names the requested plan.
- Strict title-fix work: commits in the same scope, regardless of
  reviewability, because title hygiene is a commit-level rule.

No WFW path should read `Plan.timeline` after this plan.

## Response Projection

All response builders should project from `CommitNode` variants.

Examples:

- Plan page timeline: select commits associated with that plan and
  convert each to `api::TimelineEvent`.
- Plan revision list: select `PlanCommit::PlanOnly` and
  `PlanCommit::Mixed` for the plan.
- Implementation list: select `PlanCommit::CodeOnly` and
  `PlanCommit::Mixed` for the plan.
- Latest reviewable commit: reverse-scan the derived plan commit
  list and return the latest variant with a gate.
- Commit details: switch on `CommitBody` directly.

Projection can keep small helper methods for readability, but it
must not rebuild a second persistent model with different facts.

## Implementation Phases

### Phase 1 — Introduce New Types

- Add `CommitMeta`, `CommitBody`, `PlanCommit`,
  `MultiPlanCommit`, `AdHocCommit`, `FinalizeCommit`,
  `PlanTouchSummary`, and `AttributionWarning` to
  `trinity_core::model`.
- Add accessor methods that cover current call sites.
- Keep old `CommitNode` fields temporarily only behind conversion
  helpers if needed for compilation. The conversion must be
  one-way and short-lived.

### Phase 2 — Classifier Returns `ClassifiedCommit`

- Move prefix parsing, file-touch attribution, finalize detection,
  ad hoc reviewability, and walk-back update into one classifier.
- `ClassifiedCommit` is the only source of:
  - commit body variant,
  - warnings,
  - next effective plan,
  - plan metadata effects.
- Add tests for every variant and for walk-back behavior:
  - `[misc]` touching a plan does not seed that plan.
  - `[plan]` code-only commit becomes a reviewable plan commit.
  - unknown prefix becomes ad hoc with warning.
  - multi-plan touch cannot carry a gate.
  - finalize commit cannot carry a gate.

### Phase 3 — Replace Canonical `CommitNode`

- Change `RepoState.commits` to store the new `CommitNode`.
- Delete old fields: `kind`, `attribution`, `plans`, `gate`.
- Update feedback attachment to match on variants.
- Update gate rebuild helpers so `gate_mut()` only exists for
  reviewable variants.
- Bump `state_cache::CACHE_FORMAT_VERSION` so the on-disk
  `BaseStatePayload` schema change invalidates older caches.
  `Plan.timeline` is still present in the v4 payload; Phase 4
  bumps again when that field disappears.
- Cache-read error semantics: any `try_load` error (bad magic,
  format-version mismatch, trinity-generation mismatch,
  head mismatch, decode failure) must delete the offending file
  before falling back to a full fold. Today the fallback is
  silent but the stale file rots in the cache dir; bake the
  delete-on-error rule into the loader so the cache is
  self-cleaning. Cache thinning is best-effort housekeeping,
  not correctness — every error path must clean up after itself.

### Phase 4 — Remove Canonical `Plan.timeline`

- Delete `Plan.timeline` from model state.
- Replace `Plan::event_for`, `latest_reviewable_event`,
  `frozen_at`, and similar methods with projection helpers over
  `RepoState`.
- Project plan lifecycle through `RepoState` methods that walk
  `commit_order`. No `RepoIndexes` is introduced; if Trinity is
  ever measured to be slow on these projections, that's the
  signal — not a guess — and the cache earns a follow-up plan.
- Ensure plan lifecycle/finalization is derived from finalize
  commit variants.
- Bump `CACHE_FORMAT_VERSION` again now that `Plan.timeline`
  drops out of the cached payload.
- Order-of-operations note: Phase 5 rewrites the consumers
  (WFW, response builders) to read from `CommitBody` variants.
  Phase 4's `Plan.timeline` deletion is unblocked by Phase 5
  — these two phases may land in either order or together,
  but the deletion cannot precede the consumer migration.

### Phase 5 — Update WFW and Responses

- Rewrite WFW candidate collection to inspect `CommitBody`.
- Rewrite plan-page, commit-detail, timeline, preview, and diff
  projections to use commit variants.
- Preserve the already-good tagged enum API shapes for
  `CommitRow`, `DiffLine`, and `TimelineEvent`.
- Remove any leftover code that constructs
  `PlanTimelineEvent` as canonical model state.

### Phase 6 — Delete Compatibility Shims

- Remove old `CommitAttribution` if it is no longer needed.
  If a value-form is still useful for display, make it a method
  return or projection enum, not stored canonical state.
- Remove stale helpers that derive `plans`, `kind`, or `gate`
  from old fields.
- Delete or retire the older
  `tagged-enums-for-non-orthogonal-fields.md` plan if it is now
  fully subsumed by this one.

## Tests

- Unit tests for `ClassifiedCommit` covering every `CommitBody`
  variant.
- Compile-time structural tests where possible: fixture builders
  should make impossible old states impossible to construct.
- Regression: a reviewable plan commit always has a gate because
  the variant requires it.
- Regression: multi-plan and finalize commits cannot have gates.
- Regression: plan-scoped WFW ignores unrelated strict-title
  violations.
- Regression: repo-scoped WFW still sees ad hoc commits.
- Regression: latest plan revision / latest implementation commit
  are derived from the commit stream and match previous behavior.
- Regression: deleting `Plan.timeline` does not change rendered
  plan-page timeline output for representative histories.
- Wire snapshot tests for response shape changes.
- `cargo test --workspace --exclude trinity-frontend`.
- `cargo test -p trinity-frontend` if frontend types change.
- `cargo fmt -- --check`.

## Acceptance

- There is exactly one canonical chronological representation:
  `RepoState.commit_order + RepoState.commits`.
- `Plan` no longer stores `timeline`.
- `CommitNode` no longer stores parallel `kind`,
  `attribution`, `plans`, and `gate: Option<_>` fields.
- Reviewability is encoded by variants that carry `CommitGate`.
- Non-reviewability is encoded by variants that do not carry
  `CommitGate`.
- WFW, response projection, feedback attachment, and lifecycle
  derivation all read from `CommitBody` variants.
- No flat core struct remains where a `kind` discriminator controls
  whether sibling fields are valid.
- The old tagged-enum plan's remaining intent is either implemented
  here or explicitly marked obsolete.

## Out of Scope

- Changing the user-facing workflow semantics of approvals,
  request-changes, or force-finish.
- Adding a new database or migration layer.
- Adding any derived-index cache (per-plan or otherwise). Direct
  projections only; performance work is its own future plan if
  Trinity is ever measured to be slow.
- Removing the web UI.
