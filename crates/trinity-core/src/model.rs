//! Daemon-side fold-state types. The shapes the daemon stores in
//! memory live here; the daemon also publishes them on the wire —
//! newtypes serialize transparently, so daemon-side validation
//! survives round-trip.
//!
//! After `wasm-markdown-rendering.md` Phase 2, `model::Feedback`
//! and `model::CommitGate` are re-exported as `api::Feedback` /
//! `api::CommitGate` — one type per concept, no projection step.
//! Rendered HTML lives in the wasm frontend (`frontend::markdown`);
//! the wire ships raw markdown only.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, ContentHash, PlanKey};
use crate::vocab::{CommitGateState, CommitKind, PlanLifecycle, PlanWorktreeStatus};

/// One reviewer's verdict file. Single canonical Feedback type —
/// `api::Feedback` re-exports this, so daemon storage and wire
/// response carry the same shape.
///
/// The `author` field is redundant with the `CommitGate.feedback`
/// map key (`BTreeMap<AgentLabel, Feedback>` where the key always
/// equals the value's `author`). The redundancy is bounded — one
/// writer (`disk_snapshot::apply_commit` building the gate from
/// feedback files) enforces the invariant; reads can rely on it.
/// The wire has already been carrying this redundancy under
/// `api::Feedback`; storage just stops being the odd one out.
///
/// `body` is raw markdown. The wasm frontend renders to HTML at
/// display time — no `body_html` field on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct Feedback {
    pub author: AgentLabel,
    pub verdict: crate::vocab::Verdict,
    pub body: String,
    /// Repo-relative path of the feedback file.
    pub path: String,
    /// File mtime as unix seconds at ingest. Used by the UI to
    /// sort feedback chronologically when SHA + author alone don't
    /// establish order.
    pub created_at: i64,
}

/// Folded review state for one commit. Cumulative-participant set
/// + per-commit verdict breakdown + each participant's feedback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct CommitGate {
    pub state: CommitGateState,
    /// Every reviewer who has ever posted on any reviewable commit
    /// of this plan (cumulative).
    pub participants: Vec<AgentLabel>,
    /// Approvers ON THIS COMMIT.
    pub approvers: Vec<AgentLabel>,
    /// Requesters of changes ON THIS COMMIT.
    pub requesters: Vec<AgentLabel>,
    /// Authors who posted Unmarked verdicts on this commit.
    pub ambiguous: Vec<AgentLabel>,
    /// Participants who haven't voted on this commit yet.
    pub missing: Vec<AgentLabel>,
    /// Feedback bodies on this commit, keyed by author.
    pub feedback: BTreeMap<AgentLabel, Feedback>,
}

/// One commit in a plan's life as observed by the fold. Tagged
/// by per-plan classification; the reviewable variants
/// (`PlanOnly | CodeOnly | Mixed`) carry a `gate`, the
/// non-reviewable ones (`MultiPlan`, `Finalize`) don't.
///
/// Use the accessor methods (`sha()`, `kind()`, `gate()`, etc.)
/// for the common value-form reads. Variant matching is for the
/// rare cases that need the kind structurally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub enum PlanTimelineEvent {
    PlanOnly {
        sha: CommitSha,
        author_ts: i64,
        subject: String,
    },
    CodeOnly {
        sha: CommitSha,
        author_ts: i64,
        subject: String,
    },
    Mixed {
        sha: CommitSha,
        author_ts: i64,
        subject: String,
    },
    MultiPlan {
        sha: CommitSha,
        author_ts: i64,
        subject: String,
    },
    Finalize {
        sha: CommitSha,
        author_ts: i64,
        subject: String,
    },
}

impl PlanTimelineEvent {
    pub fn sha(&self) -> &CommitSha {
        match self {
            PlanTimelineEvent::PlanOnly { sha, .. }
            | PlanTimelineEvent::CodeOnly { sha, .. }
            | PlanTimelineEvent::Mixed { sha, .. }
            | PlanTimelineEvent::MultiPlan { sha, .. }
            | PlanTimelineEvent::Finalize { sha, .. } => sha,
        }
    }

    pub fn author_ts(&self) -> i64 {
        match self {
            PlanTimelineEvent::PlanOnly { author_ts, .. }
            | PlanTimelineEvent::CodeOnly { author_ts, .. }
            | PlanTimelineEvent::Mixed { author_ts, .. }
            | PlanTimelineEvent::MultiPlan { author_ts, .. }
            | PlanTimelineEvent::Finalize { author_ts, .. } => *author_ts,
        }
    }

    pub fn subject(&self) -> &str {
        match self {
            PlanTimelineEvent::PlanOnly { subject, .. }
            | PlanTimelineEvent::CodeOnly { subject, .. }
            | PlanTimelineEvent::Mixed { subject, .. }
            | PlanTimelineEvent::MultiPlan { subject, .. }
            | PlanTimelineEvent::Finalize { subject, .. } => subject,
        }
    }

    pub fn kind(&self) -> CommitKind {
        match self {
            PlanTimelineEvent::PlanOnly { .. } => CommitKind::PlanOnly,
            PlanTimelineEvent::CodeOnly { .. } => CommitKind::CodeOnly,
            PlanTimelineEvent::Mixed { .. } => CommitKind::Mixed,
            PlanTimelineEvent::MultiPlan { .. } => CommitKind::MultiPlan,
            PlanTimelineEvent::Finalize { .. } => CommitKind::Finalize,
        }
    }

    pub fn is_reviewable(&self) -> bool {
        self.kind().is_reviewable()
    }
}

/// Per-cycle summary surfaced in the plan-detail wire under
/// `archived_cycles`. One entry per freeze event (today: 0 or 1).
/// Lives in `model` because the fold-state stores it directly on
/// `Plan.archived_cycles`; api response shapes re-export it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct ArchivedCycle {
    /// The commit at which the cycle was closed (the freeze event).
    pub closer: CommitSha,
    /// Number of approving-reviewer files in
    /// `.trinity/finished/<stem>/` at that freeze commit's tree.
    pub approver_count: u32,
}

/// One plan's full fold state. The daemon stores this directly and
/// the wire response shape is built by projecting selected fields
/// at the boundary. Markdown stays raw on the wire; the wasm
/// frontend renders at display time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct Plan {
    pub id: PlanKey,
    /// Repo-relative path: `.trinity/plans/<stem>.md`. The daemon
    /// resolves to absolute via `repo_root.join(&plan.plan_path)`
    /// at IO time. Never absolute on this struct.
    pub plan_path: String,
    /// Plan-file body. For unfrozen plans, this tracks HEAD. For
    /// frozen plans, this is the body at the freeze commit.
    pub body: String,
    pub body_hash: ContentHash,
    /// First commit that introduced the plan file (in this plan
    /// instance's life — if a same-stem plan was deleted and
    /// re-introduced, `plan_intro` is the re-introduction commit).
    pub plan_intro: CommitSha,
    /// First-parent of `plan_intro`, or `None` for the root commit.
    pub plan_intro_parent: Option<CommitSha>,
    /// `max(author_ts of attributed commits, mtime of feedback
    /// files)`. Powers the /api/plans sort order.
    pub last_activity_ts: i64,
    /// Chronological per-commit log of this plan's life. Carries
    /// the SHA + per-plan kind for each commit attributed to this
    /// plan; the authoritative per-commit gate (Phase 2 of
    /// `commit-first-review-model`) lives on
    /// `RepoState.commits[sha].gate`. Use this list to enumerate the
    /// plan's chronological commit history, then call
    /// `RepoState::gate_for(sha)` for the gate.
    pub timeline: Vec<PlanTimelineEvent>,
    /// Per-cycle summaries derived from the fold's freeze events
    /// (one entry per freeze). Today the monotone rule means this
    /// has length 0 or 1.
    pub archived_cycles: Vec<ArchivedCycle>,
}

impl Plan {
    /// Find the timeline event for a SHA, if this plan has one.
    pub fn event_for(&self, sha: &CommitSha) -> Option<&PlanTimelineEvent> {
        self.timeline.iter().find(|e| e.sha() == sha)
    }

    pub fn event_for_mut(&mut self, sha: &CommitSha) -> Option<&mut PlanTimelineEvent> {
        self.timeline.iter_mut().find(|e| e.sha() == sha)
    }

    /// The latest reviewable commit event, if any. Reverse scan.
    pub fn latest_reviewable_event(&self) -> Option<&PlanTimelineEvent> {
        self.timeline.iter().rev().find(|e| e.is_reviewable())
    }

    /// The freeze commit's SHA, if this plan has frozen. Derived
    /// from the timeline — the last `CommitKind::Finalize` event is
    /// the freeze.
    pub fn frozen_at(&self) -> Option<&CommitSha> {
        self.timeline
            .iter()
            .rev()
            .find_map(|e| matches!(e, PlanTimelineEvent::Finalize { .. }).then_some(e.sha()))
    }

    /// True iff the plan has frozen.
    pub fn is_frozen(&self) -> bool {
        self.frozen_at().is_some()
    }

    /// Lifecycle derived from `is_frozen()`.
    pub fn lifecycle(&self) -> PlanLifecycle {
        if self.is_frozen() {
            PlanLifecycle::Finished
        } else {
            PlanLifecycle::Active
        }
    }

    /// True iff this plan should surface across Trinity's response
    /// shapes given the current `worktree_status`. A plan is HIDDEN
    /// when its file is missing from the working tree AND it has
    /// not frozen.
    pub fn is_visible(&self, worktree_status: PlanWorktreeStatus) -> bool {
        self.is_frozen() || !matches!(worktree_status, PlanWorktreeStatus::PlanFileMissing)
    }
}

/// How a commit relates to plans in the repo. Phase 1 of
/// `commit-first-review-model` adds this alongside the existing
/// per-plan timeline so the fold can carry one
/// authoritative-per-commit attribution value. Reviewable variants
/// in the (Phase-2-onwards) commit-first matcher key off this.
///
/// `AdHoc` replaces the legacy `Unattributed` category: today's
/// `Unattributed` commits (no plan touch, no inherited parent) are
/// the same set the commit-first model surfaces as ad hoc.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitAttribution {
    /// Single-plan commit. `PlanOnly | CodeOnly | Mixed` kinds
    /// reviewable; the gate lives on the matching `CommitNode`.
    Plan { plan: PlanKey },
    /// Commit touched more than one plan (file-level). Non-
    /// reviewable: per-plan reviewer sets would conflict.
    MultiPlan { plans: std::collections::BTreeSet<PlanKey> },
    /// Commit touched no plan and inherited no active/last-touched
    /// plan context. First-class reviewable category once Phase 4
    /// lands; non-reviewable until then.
    AdHoc,
    /// Freeze commit for a plan. Non-reviewable; the approving
    /// files snapshot lives at the commit's tree under
    /// `.trinity/finished/<stem>/`.
    Finalize { plan: PlanKey },
}

/// One commit in the repo-wide chronological stream. Phase 1 of
/// `commit-first-review-model` builds this map alongside the per-
/// plan timeline; the existing matcher continues to read
/// `Plan.timeline`. Phase 2 makes `CommitNode.gate` authoritative
/// and rewrites `PlanTimelineEvent` to carry a SHA pointer instead
/// of an owned gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct CommitNode {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    pub kind: CommitKind,
    pub attribution: CommitAttribution,
    /// Plans this commit appears in (via touches, attribution, or
    /// finalize). Superset of any single plan named by
    /// `attribution`. Empty for `AdHoc` until Phase 4 attaches
    /// ad hoc participant sets.
    pub plans: std::collections::BTreeSet<PlanKey>,
    /// Per-commit review gate. `Some` iff `kind.is_reviewable()`
    /// AND `attribution` names exactly one plan (`Plan(_)`). All
    /// other variants are non-reviewable and carry `None`.
    pub gate: Option<CommitGate>,
    /// Phase 5 of `commit-first-review-model`: non-blocking
    /// attribution warning attached by the title-prefix
    /// classifier. `Some` when the commit's `[…]` prefix names a
    /// plan that doesn't exist (degrades to `AdHoc`), or when no
    /// prefix is present but file-touched / active-plan
    /// inference assigned attribution that operators should be
    /// warned about. `None` in the clean cases.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution_warning: Option<String>,
}

// ============================================================
// Phase 1 of `core-model-invalid-states-unrepresentable`:
// new canonical types living alongside the legacy `CommitNode`
// + `Plan.timeline` shape. Phase 2 wires the classifier to
// emit them; Phase 3 swaps `RepoState.commits` and deletes the
// old fields. Until then these are compile-only — no fold path
// produces or consumes them.
// ============================================================

/// A `Vec` that cannot be empty. The only constructor returns
/// `Err` for empty inputs. Used where the type's purpose demands
/// at least one element (`ReviewPolicy::Blocking` participants).
///
/// Serde decoding goes through `TryFrom<Vec<T>>` so the invariant
/// holds across the wire. Cache (wincode) encoding is
/// **intentionally not derived**: wincode-derive bypasses the
/// validating constructor by reading the private `inner` field
/// directly. NonEmptyVec is only used by `ReviewPolicy`, which
/// is a projection and never enters the cache, so the gap
/// doesn't matter in practice. If a future change stores a
/// `NonEmptyVec` in the cache, the cache loader must validate at
/// the boundary (Phase 3's responsibility — codex on 0ec224d).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    bound(
        serialize = "T: Clone + Serialize",
        deserialize = "T: Clone + Deserialize<'de>"
    ),
    try_from = "Vec<T>",
    into = "Vec<T>"
)]
pub struct NonEmptyVec<T>
where
    T: Clone,
{
    inner: Vec<T>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonEmptyVecError;

impl std::fmt::Display for NonEmptyVecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NonEmptyVec cannot be constructed from an empty Vec")
    }
}

impl std::error::Error for NonEmptyVecError {}

impl<T> NonEmptyVec<T>
where
    T: Clone,
{
    pub fn new(items: Vec<T>) -> Result<Self, NonEmptyVecError> {
        if items.is_empty() {
            Err(NonEmptyVecError)
        } else {
            Ok(Self { inner: items })
        }
    }
    pub fn first(&self) -> &T {
        &self.inner[0]
    }
    pub fn as_slice(&self) -> &[T] {
        &self.inner
    }
    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.inner.iter()
    }
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

impl<T> TryFrom<Vec<T>> for NonEmptyVec<T>
where
    T: Clone,
{
    type Error = NonEmptyVecError;
    fn try_from(v: Vec<T>) -> Result<Self, Self::Error> {
        NonEmptyVec::new(v)
    }
}

impl<T> From<NonEmptyVec<T>> for Vec<T>
where
    T: Clone,
{
    fn from(v: NonEmptyVec<T>) -> Vec<T> {
        v.inner
    }
}

// PlanBody (text + hash newtype) was sketched in Phase 1 but
// removed before landing. The hash field would have been
// duplicate canonical state — serde/wincode derives can
// reconstruct `PlanBody { text, hash }` without going through
// the validating constructor, so the "text and hash never
// disagree" invariant doesn't survive the decode path. Codex on
// 0ec224d caught this.
//
// Resolution: drop the hash from canonical state entirely. The
// new `Plan` (introduced in Phase 3) stores `body: String`.
// Hash comparisons (worktree-vs-HEAD body checks) recompute the
// hash on demand — the cost is microseconds on plan-size
// markdown files, and git's commit/blob identity already
// handles durable content identity. `blake3` is no longer a
// `trinity-core` dependency.

/// One reviewer's feedback body. The legacy `Feedback.author`
/// field is removed here — the canonical
/// `CommitReviews.feedback` map key IS the author.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct FeedbackBody {
    pub verdict: crate::vocab::Verdict,
    pub body: String,
    pub created_at: i64,
}

/// Canonical review activity for a commit: who wrote what.
/// Files on disk are truth; this mirrors them once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct CommitReviews {
    pub feedback: BTreeMap<AgentLabel, FeedbackBody>,
}

/// One plan file's touch within a commit's diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct PlanTouchSummary {
    plan: PlanKey,
    pub kind: crate::vocab::PlanTouchKind,
    pub new_body: Option<String>,
}

impl PlanTouchSummary {
    pub fn new(
        plan: PlanKey,
        kind: crate::vocab::PlanTouchKind,
        new_body: Option<String>,
    ) -> Self {
        Self {
            plan,
            kind,
            new_body,
        }
    }
    pub fn plan(&self) -> &PlanKey {
        &self.plan
    }
}

/// Validated container: ≥2 touches naming ≥2 distinct plan keys.
///
/// Serde decoding goes through `TryFrom<Vec<PlanTouchSummary>>`
/// so the invariant holds on the wire. Cache (wincode) encoding
/// is **intentionally not derived** — wincode-derive would
/// reconstruct `MultiPlanTouches { touches }` from raw bytes,
/// bypassing `new()`. `MultiPlanCommit` (which contains this)
/// therefore also can't be cache-derived, and so on up to
/// `CommitBody`. Phase 3's cache integration is where this gets
/// resolved (either custom validated wincode impls, or a
/// flatter cache shape with validation at the loader boundary —
/// codex on 0ec224d).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Vec<PlanTouchSummary>", into = "Vec<PlanTouchSummary>")]
pub struct MultiPlanTouches {
    touches: Vec<PlanTouchSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiPlanTouchesError {
    NotMultiPlan,
}

impl std::fmt::Display for MultiPlanTouchesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MultiPlanTouchesError::NotMultiPlan => {
                f.write_str("MultiPlanTouches requires at least two distinct plan keys")
            }
        }
    }
}

impl std::error::Error for MultiPlanTouchesError {}

impl MultiPlanTouches {
    pub fn new(touches: Vec<PlanTouchSummary>) -> Result<Self, MultiPlanTouchesError> {
        let distinct: std::collections::BTreeSet<&PlanKey> =
            touches.iter().map(|t| t.plan()).collect();
        if distinct.len() < 2 {
            return Err(MultiPlanTouchesError::NotMultiPlan);
        }
        Ok(Self { touches })
    }
    pub fn as_slice(&self) -> &[PlanTouchSummary] {
        &self.touches
    }
    pub fn iter(&self) -> std::slice::Iter<'_, PlanTouchSummary> {
        self.touches.iter()
    }
    pub fn plans(&self) -> std::collections::BTreeSet<&PlanKey> {
        self.touches.iter().map(|t| t.plan()).collect()
    }
}

impl TryFrom<Vec<PlanTouchSummary>> for MultiPlanTouches {
    type Error = MultiPlanTouchesError;
    fn try_from(v: Vec<PlanTouchSummary>) -> Result<Self, Self::Error> {
        MultiPlanTouches::new(v)
    }
}

impl From<MultiPlanTouches> for Vec<PlanTouchSummary> {
    fn from(m: MultiPlanTouches) -> Vec<PlanTouchSummary> {
        m.touches
    }
}

/// Tagged commit-attribution warning. Replaces the legacy
/// stringly-typed `attribution_warning: Option<String>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttributionWarning {
    UnknownPlanPrefix { unknown_names: Vec<String> },
    MissingPrefix { suggested_prefix: String },
    AmbiguousPrefix,
    DanglingPlanRef { plan: PlanKey },
}

/// Per-commit metadata shared across every variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct CommitMeta {
    pub sha: CommitSha,
    pub author_ts: i64,
    pub subject: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<AttributionWarning>,
}

/// Plan-scope commit variants. Each carries the minimum set of
/// canonical facts; the plan key for `PlanOnly` / `Mixed` is
/// projected from `touch.plan()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanCommit {
    PlanOnly {
        touch: PlanTouchSummary,
        reviews: CommitReviews,
    },
    CodeOnly {
        plan: PlanKey,
        reviews: CommitReviews,
    },
    Mixed {
        touch: PlanTouchSummary,
        reviews: CommitReviews,
    },
}

impl PlanCommit {
    pub fn plan(&self) -> &PlanKey {
        match self {
            PlanCommit::PlanOnly { touch, .. } | PlanCommit::Mixed { touch, .. } => touch.plan(),
            PlanCommit::CodeOnly { plan, .. } => plan,
        }
    }
    pub fn reviews(&self) -> &CommitReviews {
        match self {
            PlanCommit::PlanOnly { reviews, .. }
            | PlanCommit::CodeOnly { reviews, .. }
            | PlanCommit::Mixed { reviews, .. } => reviews,
        }
    }
    pub fn reviews_mut(&mut self) -> &mut CommitReviews {
        match self {
            PlanCommit::PlanOnly { reviews, .. }
            | PlanCommit::CodeOnly { reviews, .. }
            | PlanCommit::Mixed { reviews, .. } => reviews,
        }
    }
}

/// Transitively excluded from `cache-encoding` because it
/// contains `MultiPlanTouches` whose invariant isn't preserved
/// by wincode-derive. See `MultiPlanTouches` doc; resolved in
/// Phase 3 alongside the cache integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiPlanCommit {
    pub touches: MultiPlanTouches,
    pub reviews: CommitReviews,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct AdHocCommit {
    pub reviews: CommitReviews,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct FinalizeCommit {
    pub plan: PlanKey,
    pub approver_count: u32,
    pub reviews: CommitReviews,
}

/// Tagged enum of all commit-body shapes. The `scope`
/// discriminator is distinct from the inner `kind` on
/// `PlanCommit` to avoid the internally-tagged-enum collision.
///
/// Transitively excluded from `cache-encoding` via
/// `MultiPlanCommit` → `MultiPlanTouches`. Resolved in Phase 3
/// when the cache integration lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CommitBody {
    Plan(PlanCommit),
    MultiPlan(MultiPlanCommit),
    AdHoc(AdHocCommit),
    Finalize(FinalizeCommit),
}

impl CommitBody {
    /// Plans this commit appears in. Each variant computes from
    /// its canonical fields — no stored sidecar set.
    pub fn associated_plans(&self) -> std::collections::BTreeSet<&PlanKey> {
        match self {
            CommitBody::Plan(p) => [p.plan()].into_iter().collect(),
            CommitBody::MultiPlan(m) => m.touches.plans(),
            CommitBody::Finalize(f) => [&f.plan].into_iter().collect(),
            CommitBody::AdHoc(_) => std::collections::BTreeSet::new(),
        }
    }
    pub fn reviews(&self) -> &CommitReviews {
        match self {
            CommitBody::Plan(p) => p.reviews(),
            CommitBody::MultiPlan(m) => &m.reviews,
            CommitBody::AdHoc(a) => &a.reviews,
            CommitBody::Finalize(f) => &f.reviews,
        }
    }
    pub fn reviews_mut(&mut self) -> &mut CommitReviews {
        match self {
            CommitBody::Plan(p) => p.reviews_mut(),
            CommitBody::MultiPlan(m) => &mut m.reviews,
            CommitBody::AdHoc(a) => &mut a.reviews,
            CommitBody::Finalize(f) => &mut f.reviews,
        }
    }
}

/// Why a commit doesn't block master. Projection-time only —
/// never stored on a commit variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NonBlockingReason {
    NoParticipants,
    ConfigDisabledPlanReview,
    ConfigDisabledMiscReview,
    StructurallyNonReviewable,
}

/// Projection: the review policy for a commit, derived from
/// snapshotted config + variant + state. Never stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewPolicy {
    Blocking {
        participants: NonEmptyVec<AgentLabel>,
    },
    NonBlocking {
        reason: NonBlockingReason,
    },
}

/// Projection: the full derived gate view that replaces the
/// stored `CommitGate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReadiness {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
}

/// Phase 2 of `core-model-invalid-states-unrepresentable`: the
/// classifier's output. Single source of truth for body variant +
/// warnings + walk-back update. The fold's caller (`apply_commit`)
/// reads from this, never from a parallel inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedCommit {
    pub body: CommitBody,
    pub warnings: Vec<AttributionWarning>,
    /// What the walk-back chain becomes after this commit. The
    /// fold's `carry.current_effective` should be assigned this
    /// value verbatim — no separate `effective_session()`
    /// computation.
    pub next_effective_plan: Option<PlanKey>,
}

/// Inputs to `classify`. Daemon-side fold builds this from the
/// commit's diff summary + current fold carry; the classifier is
/// then a pure function over the bundle.
#[derive(Debug, Clone)]
pub struct ClassifierInputs<'a> {
    pub subject: &'a str,
    /// Plan files this commit touched. Order doesn't matter
    /// semantically; the classifier inspects keys and counts.
    pub plan_touches: &'a [PlanTouchSummary],
    /// True if this commit modified any non-`.trinity/` file.
    pub has_non_plan_code: bool,
    /// Plans that fire the freeze rule on this commit. Computed
    /// by the fold's finalize-tree walker (step 4 of
    /// `apply_commit`); the classifier doesn't recompute it.
    pub plans_finalized_here: &'a std::collections::BTreeSet<PlanKey>,
    /// Approver counts per plan-key for any plan freezing on
    /// this commit. Used to populate `FinalizeCommit.approver_count`
    /// without re-walking the finalize tree.
    pub finalize_approver_counts: &'a std::collections::BTreeMap<PlanKey, u32>,
    /// Walk-back state coming into this commit: the active plan
    /// chain from the parent. `None` at the root or after a
    /// MultiPlan/AdHoc-with-no-parent break.
    pub current_effective: Option<&'a PlanKey>,
    /// Plans known to exist as of this commit's parent state.
    /// Used to validate `[plan-x]` prefix names.
    pub known_plans: &'a std::collections::BTreeSet<PlanKey>,
}

/// Title-prefix vocabulary the classifier recognizes on
/// `subject`. `parse_title_prefix` is the only entry point; it
/// returns `None` for "no recognized prefix" and the classifier
/// falls back to file-touch / walk-back inference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TitlePrefix {
    /// `[misc]` — explicit opt-out from plan attribution.
    Misc,
    /// `[plan-a]` or `[plan-a,plan-b]` — explicit plan list.
    /// Whether the names are valid plan keys is checked in
    /// `classify`, not in the parser.
    Plans(Vec<String>),
}

/// Pure prefix parser. Recognizes `[…]` at the start of the
/// subject; trims whitespace inside the brackets; treats `misc`
/// (case-insensitive) as the dedicated `Misc` variant.
pub fn parse_title_prefix(subject: &str) -> Option<TitlePrefix> {
    let trimmed = subject.trim_start();
    let rest = trimmed.strip_prefix('[')?;
    let close = rest.find(']')?;
    let inner = &rest[..close];
    if inner.is_empty() {
        return None;
    }
    let names: Vec<String> = inner
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return None;
    }
    if names.len() == 1 && names[0].eq_ignore_ascii_case("misc") {
        return Some(TitlePrefix::Misc);
    }
    Some(TitlePrefix::Plans(names))
}

/// Pure classifier. Phase 2 of
/// `core-model-invalid-states-unrepresentable`: one function
/// produces the entire `ClassifiedCommit`. The fold reads body /
/// warnings / next_effective_plan from this output; no second
/// pass reinterprets the result.
///
/// Attribution priority (matches the plan's "Attribution
/// algorithm (final)" section):
/// 1. Explicit prefix wins: `[misc]` → AdHoc; `[plan]` /
///    `[plans,...]` with all known → Plan / MultiPlan; unknown
///    names → AdHoc + warning.
/// 2. No prefix, ≥1 finalize firing → FinalizeCommit.
/// 3. No prefix, ≥2 distinct plan-file touches → MultiPlanCommit.
/// 4. No prefix, exactly 1 plan-file touch → PlanCommit (PlanOnly
///    or Mixed based on `has_non_plan_code`).
/// 5. No prefix, no touches, code-only, walk-back has effective
///    plan → PlanCommit::CodeOnly attributed to that plan.
/// 6. No prefix, no plan context → AdHoc.
///
/// Walk-back: `[plan-x]` seeds plan-x as the next effective.
/// `[misc]` / unknown prefix preserve the parent's chain (the
/// operator opted out for THIS commit, not for descendants).
/// Single-plan inference seeds that plan. MultiPlan / Finalize /
/// AdHoc preserve the parent verbatim.
pub fn classify(inputs: ClassifierInputs<'_>) -> ClassifiedCommit {
    let prefix = parse_title_prefix(inputs.subject);
    let mut warnings: Vec<AttributionWarning> = Vec::new();

    // 1. Finalize commits sit OUTSIDE the prefix grammar — they're
    // identified by the freeze rule (step 4 of apply_commit). When
    // a finalize fires and the same commit doesn't ALSO touch plan
    // files for a different plan, classify as Finalize. The
    // existing fold's freeze rule is "exactly one plan finalizes"
    // in practice; if multiple plans finalize on one commit,
    // classify the FIRST (in BTreeSet order) and treat the rest
    // as a future-work edge case.
    if !inputs.plans_finalized_here.is_empty() && inputs.plan_touches.is_empty() {
        let plan = inputs
            .plans_finalized_here
            .iter()
            .next()
            .cloned()
            .expect("non-empty checked");
        let approver_count = inputs
            .finalize_approver_counts
            .get(&plan)
            .copied()
            .unwrap_or(0);
        return ClassifiedCommit {
            body: CommitBody::Finalize(FinalizeCommit {
                plan,
                approver_count,
                reviews: CommitReviews::default(),
            }),
            warnings,
            // Finalize doesn't change the walk-back chain.
            next_effective_plan: inputs.current_effective.cloned(),
        };
    }

    // 2. Explicit prefix wins for non-finalize commits.
    if let Some(p) = prefix {
        return classify_with_prefix(p, &inputs, &mut warnings);
    }

    // 3. No prefix → inference path, with a MissingPrefix warning
    // when inference produces a plan attribution (operator can
    // amend to silence; not blocking).
    classify_without_prefix(&inputs, &mut warnings)
}

fn classify_with_prefix(
    prefix: TitlePrefix,
    inputs: &ClassifierInputs<'_>,
    warnings: &mut Vec<AttributionWarning>,
) -> ClassifiedCommit {
    match prefix {
        TitlePrefix::Misc => ClassifiedCommit {
            body: CommitBody::AdHoc(AdHocCommit::default()),
            warnings: std::mem::take(warnings),
            next_effective_plan: inputs.current_effective.cloned(),
        },
        TitlePrefix::Plans(names) => {
            let parsed: Vec<Result<PlanKey, String>> = names
                .iter()
                .map(|n| PlanKey::parse(n).map_err(|_| n.clone()))
                .collect();
            let unknown: Vec<String> = parsed
                .iter()
                .filter_map(|r| match r {
                    Ok(k) if inputs.known_plans.contains(k) => None,
                    Ok(k) => Some(k.as_str().to_string()),
                    Err(s) => Some(s.clone()),
                })
                .collect();
            if !unknown.is_empty() {
                warnings.push(AttributionWarning::UnknownPlanPrefix {
                    unknown_names: unknown,
                });
                return ClassifiedCommit {
                    body: CommitBody::AdHoc(AdHocCommit::default()),
                    warnings: std::mem::take(warnings),
                    next_effective_plan: inputs.current_effective.cloned(),
                };
            }
            let valid: std::collections::BTreeSet<PlanKey> =
                parsed.into_iter().flatten().collect();
            if valid.len() == 1 {
                let plan = valid.iter().next().cloned().expect("len==1");
                let body =
                    plan_commit_for(plan.clone(), inputs.plan_touches, inputs.has_non_plan_code);
                return ClassifiedCommit {
                    body: CommitBody::Plan(body),
                    warnings: std::mem::take(warnings),
                    next_effective_plan: Some(plan),
                };
            }
            // Multi-plan from the prefix. Build a MultiPlanTouches
            // from the commit's plan_touches filtered to the
            // prefix-named plans. If the commit didn't actually
            // touch >=2 of the prefix-named plans, fall back to
            // a synthetic touch list. (The fold today derives
            // PlanTouchSummary from CommitChanges; the prefix
            // might name plans not touched by this commit. For
            // Phase 2's pure-classifier scope, that edge case
            // surfaces as the unknown-prefix path above; valid
            // multi-plan prefixes always correspond to touched
            // plans.)
            let multi_touches: Vec<PlanTouchSummary> = inputs
                .plan_touches
                .iter()
                .filter(|t| valid.contains(t.plan()))
                .cloned()
                .collect();
            // Defensive: if the prefix names plans the commit
            // didn't actually touch, we can't form a valid
            // MultiPlanTouches (≥2 distinct). Degrade to AdHoc
            // with an AmbiguousPrefix warning rather than panic.
            let multi = match MultiPlanTouches::new(multi_touches) {
                Ok(m) => m,
                Err(_) => {
                    warnings.push(AttributionWarning::AmbiguousPrefix);
                    return ClassifiedCommit {
                        body: CommitBody::AdHoc(AdHocCommit::default()),
                        warnings: std::mem::take(warnings),
                        next_effective_plan: inputs.current_effective.cloned(),
                    };
                }
            };
            ClassifiedCommit {
                body: CommitBody::MultiPlan(MultiPlanCommit {
                    touches: multi,
                    reviews: CommitReviews::default(),
                }),
                warnings: std::mem::take(warnings),
                // MultiPlan is transparent to walk-back.
                next_effective_plan: inputs.current_effective.cloned(),
            }
        }
    }
}

fn classify_without_prefix(
    inputs: &ClassifierInputs<'_>,
    warnings: &mut Vec<AttributionWarning>,
) -> ClassifiedCommit {
    let distinct_touched: std::collections::BTreeSet<&PlanKey> =
        inputs.plan_touches.iter().map(|t| t.plan()).collect();

    if distinct_touched.len() >= 2 {
        let touches = inputs.plan_touches.to_vec();
        let multi = MultiPlanTouches::new(touches).expect("≥2 distinct checked");
        let suggested = format!(
            "[{}]",
            distinct_touched
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(",")
        );
        warnings.push(AttributionWarning::MissingPrefix {
            suggested_prefix: suggested,
        });
        return ClassifiedCommit {
            body: CommitBody::MultiPlan(MultiPlanCommit {
                touches: multi,
                reviews: CommitReviews::default(),
            }),
            warnings: std::mem::take(warnings),
            next_effective_plan: inputs.current_effective.cloned(),
        };
    }

    if distinct_touched.len() == 1 {
        let touch = inputs
            .plan_touches
            .iter()
            .find(|t| distinct_touched.contains(t.plan()))
            .cloned()
            .expect("non-empty checked");
        let plan = touch.plan().clone();
        let suggested = format!("[{}]", plan.as_str());
        warnings.push(AttributionWarning::MissingPrefix {
            suggested_prefix: suggested,
        });
        let body = if inputs.has_non_plan_code {
            PlanCommit::Mixed {
                touch,
                reviews: CommitReviews::default(),
            }
        } else {
            PlanCommit::PlanOnly {
                touch,
                reviews: CommitReviews::default(),
            }
        };
        return ClassifiedCommit {
            body: CommitBody::Plan(body),
            warnings: std::mem::take(warnings),
            next_effective_plan: Some(plan),
        };
    }

    // No plan touches.
    if inputs.has_non_plan_code
        && let Some(parent) = inputs.current_effective
    {
        let plan = parent.clone();
        let suggested = format!("[{}]", plan.as_str());
        warnings.push(AttributionWarning::MissingPrefix {
            suggested_prefix: suggested,
        });
        return ClassifiedCommit {
            body: CommitBody::Plan(PlanCommit::CodeOnly {
                plan: plan.clone(),
                reviews: CommitReviews::default(),
            }),
            warnings: std::mem::take(warnings),
            next_effective_plan: Some(plan),
        };
    }

    // Genuine ad hoc: no prefix, no touches, no walk-back chain.
    ClassifiedCommit {
        body: CommitBody::AdHoc(AdHocCommit::default()),
        warnings: std::mem::take(warnings),
        next_effective_plan: inputs.current_effective.cloned(),
    }
}

fn plan_commit_for(
    plan: PlanKey,
    plan_touches: &[PlanTouchSummary],
    has_non_plan_code: bool,
) -> PlanCommit {
    let our_touch = plan_touches.iter().find(|t| t.plan() == &plan).cloned();
    match (our_touch, has_non_plan_code) {
        (Some(touch), true) => PlanCommit::Mixed {
            touch,
            reviews: CommitReviews::default(),
        },
        (Some(touch), false) => PlanCommit::PlanOnly {
            touch,
            reviews: CommitReviews::default(),
        },
        (None, true) => PlanCommit::CodeOnly {
            plan,
            reviews: CommitReviews::default(),
        },
        (None, false) => {
            // Prefix names a plan but the commit neither touches
            // the plan file nor carries code. Rare — treat as
            // CodeOnly with no body update; the projection will
            // emit zero-content events.
            PlanCommit::CodeOnly {
                plan,
                reviews: CommitReviews::default(),
            }
        }
    }
}

#[cfg(test)]
mod phase1_invariant_tests {
    use super::*;

    fn plan(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }
    fn touch(plan_str: &str) -> PlanTouchSummary {
        PlanTouchSummary::new(plan(plan_str), crate::vocab::PlanTouchKind::Intro, None)
    }

    #[test]
    fn nonempty_vec_rejects_empty() {
        let r: Result<NonEmptyVec<i32>, _> = NonEmptyVec::new(Vec::new());
        assert!(r.is_err());
    }

    #[test]
    fn nonempty_vec_accepts_one() {
        let nv: NonEmptyVec<i32> = NonEmptyVec::new(vec![1]).unwrap();
        assert_eq!(nv.len(), 1);
        assert_eq!(nv.first(), &1);
    }

    #[test]
    fn multi_plan_touches_rejects_single_plan() {
        let r = MultiPlanTouches::new(vec![touch("foo")]);
        assert_eq!(r, Err(MultiPlanTouchesError::NotMultiPlan));
    }

    #[test]
    fn multi_plan_touches_rejects_duplicate_plan() {
        let r = MultiPlanTouches::new(vec![touch("foo"), touch("foo")]);
        assert_eq!(r, Err(MultiPlanTouchesError::NotMultiPlan));
    }

    #[test]
    fn multi_plan_touches_accepts_two_distinct_plans() {
        let m = MultiPlanTouches::new(vec![touch("foo"), touch("bar")]).unwrap();
        let plans = m.plans();
        assert_eq!(plans.len(), 2);
        assert!(plans.contains(&plan("foo")));
        assert!(plans.contains(&plan("bar")));
    }

    #[test]
    fn commit_body_associated_plans_plan_only() {
        let body = CommitBody::Plan(PlanCommit::PlanOnly {
            touch: touch("foo"),
            reviews: CommitReviews::default(),
        });
        let plans = body.associated_plans();
        assert_eq!(plans.len(), 1);
        assert!(plans.contains(&plan("foo")));
    }

    #[test]
    fn commit_body_associated_plans_code_only() {
        let body = CommitBody::Plan(PlanCommit::CodeOnly {
            plan: plan("foo"),
            reviews: CommitReviews::default(),
        });
        let plans = body.associated_plans();
        assert_eq!(plans.len(), 1);
        assert!(plans.contains(&plan("foo")));
    }

    #[test]
    fn commit_body_associated_plans_ad_hoc_is_empty() {
        let body = CommitBody::AdHoc(AdHocCommit::default());
        assert!(body.associated_plans().is_empty());
    }

    #[test]
    fn commit_body_associated_plans_multi_plan() {
        let body = CommitBody::MultiPlan(MultiPlanCommit {
            touches: MultiPlanTouches::new(vec![touch("foo"), touch("bar")]).unwrap(),
            reviews: CommitReviews::default(),
        });
        let plans = body.associated_plans();
        assert_eq!(plans.len(), 2);
    }

    #[test]
    fn commit_body_associated_plans_finalize() {
        let body = CommitBody::Finalize(FinalizeCommit {
            plan: plan("foo"),
            approver_count: 1,
            reviews: CommitReviews::default(),
        });
        let plans = body.associated_plans();
        assert_eq!(plans.len(), 1);
        assert!(plans.contains(&plan("foo")));
    }

    /// Serde decoding of `NonEmptyVec` must go through the
    /// validating `TryFrom<Vec<T>>` so wire-level invalid values
    /// are rejected, not silently accepted.
    #[test]
    fn nonempty_vec_serde_rejects_empty_vec() {
        let r: Result<NonEmptyVec<i32>, _> = serde_json::from_str("[]");
        assert!(r.is_err());
        let ok: NonEmptyVec<i32> = serde_json::from_str("[1, 2]").unwrap();
        assert_eq!(ok.len(), 2);
    }

    /// Serde decoding of `MultiPlanTouches` must reject a single-plan
    /// list at the wire boundary too.
    #[test]
    fn multi_plan_touches_serde_rejects_single_plan() {
        let single = serde_json::to_string(&vec![touch("foo")]).unwrap();
        let r: Result<MultiPlanTouches, _> = serde_json::from_str(&single);
        assert!(r.is_err());
        let dual = serde_json::to_string(&vec![touch("foo"), touch("bar")]).unwrap();
        let ok: MultiPlanTouches = serde_json::from_str(&dual).unwrap();
        assert_eq!(ok.plans().len(), 2);
    }

    // ============================================================
    // Phase 2: classifier tests
    // ============================================================

    use std::collections::{BTreeMap, BTreeSet};

    fn known(names: &[&str]) -> BTreeSet<PlanKey> {
        names.iter().map(|n| plan(n)).collect()
    }

    fn empty_finalize() -> BTreeSet<PlanKey> {
        BTreeSet::new()
    }
    fn empty_approvers() -> BTreeMap<PlanKey, u32> {
        BTreeMap::new()
    }

    fn classify_with(
        subject: &str,
        plan_touches: Vec<PlanTouchSummary>,
        has_non_plan_code: bool,
        current_effective: Option<PlanKey>,
        known_plans: BTreeSet<PlanKey>,
    ) -> ClassifiedCommit {
        let finalize_set = empty_finalize();
        let approvers = empty_approvers();
        classify(ClassifierInputs {
            subject,
            plan_touches: &plan_touches,
            has_non_plan_code,
            plans_finalized_here: &finalize_set,
            finalize_approver_counts: &approvers,
            current_effective: current_effective.as_ref(),
            known_plans: &known_plans,
        })
    }

    #[test]
    fn classify_misc_prefix_with_plan_touch_is_ad_hoc() {
        // [misc] commit touching plan-foo's file → AdHoc.
        // current_effective is preserved (parent's chain).
        let r = classify_with(
            "[misc] doc fix",
            vec![touch("foo")],
            false,
            Some(plan("parent")),
            known(&["foo", "parent"]),
        );
        assert!(matches!(r.body, CommitBody::AdHoc(_)));
        assert!(r.warnings.is_empty());
        assert_eq!(r.next_effective_plan, Some(plan("parent")));
    }

    #[test]
    fn classify_plan_prefix_code_only() {
        // [plan-foo] code-only commit → PlanCommit::CodeOnly.
        let r = classify_with(
            "[foo] implement",
            vec![],
            true,
            None,
            known(&["foo"]),
        );
        match &r.body {
            CommitBody::Plan(PlanCommit::CodeOnly { plan: p, .. }) => {
                assert_eq!(p, &plan("foo"));
            }
            other => panic!("expected CodeOnly, got {other:?}"),
        }
        assert!(r.warnings.is_empty());
        assert_eq!(r.next_effective_plan, Some(plan("foo")));
    }

    #[test]
    fn classify_plan_prefix_plan_file_touch_only() {
        let r = classify_with(
            "[foo] intro",
            vec![touch("foo")],
            false,
            None,
            known(&["foo"]),
        );
        assert!(matches!(
            r.body,
            CommitBody::Plan(PlanCommit::PlanOnly { .. })
        ));
        assert!(r.warnings.is_empty());
        assert_eq!(r.next_effective_plan, Some(plan("foo")));
    }

    #[test]
    fn classify_plan_prefix_mixed() {
        let r = classify_with(
            "[foo] revise + code",
            vec![touch("foo")],
            true,
            None,
            known(&["foo"]),
        );
        assert!(matches!(r.body, CommitBody::Plan(PlanCommit::Mixed { .. })));
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn classify_unknown_prefix_is_ad_hoc_with_warning() {
        let r = classify_with(
            "[no-such] thing",
            vec![],
            true,
            Some(plan("parent")),
            known(&["foo", "parent"]),
        );
        assert!(matches!(r.body, CommitBody::AdHoc(_)));
        assert_eq!(r.warnings.len(), 1);
        assert!(matches!(
            &r.warnings[0],
            AttributionWarning::UnknownPlanPrefix { unknown_names }
                if unknown_names == &vec!["no-such".to_string()]
        ));
        // Parent's chain preserved — [unknown] is an opt-out attempt.
        assert_eq!(r.next_effective_plan, Some(plan("parent")));
    }

    #[test]
    fn classify_no_prefix_single_touch_is_plan_only_with_missing_prefix_warning() {
        let r = classify_with(
            "intro foo",
            vec![touch("foo")],
            false,
            None,
            known(&["foo"]),
        );
        assert!(matches!(
            r.body,
            CommitBody::Plan(PlanCommit::PlanOnly { .. })
        ));
        assert_eq!(r.warnings.len(), 1);
        assert!(matches!(
            &r.warnings[0],
            AttributionWarning::MissingPrefix { suggested_prefix }
                if suggested_prefix == "[foo]"
        ));
        assert_eq!(r.next_effective_plan, Some(plan("foo")));
    }

    #[test]
    fn classify_no_prefix_code_with_walkback_attributes_to_parent() {
        let r = classify_with(
            "implement",
            vec![],
            true,
            Some(plan("foo")),
            known(&["foo"]),
        );
        match &r.body {
            CommitBody::Plan(PlanCommit::CodeOnly { plan: p, .. }) => {
                assert_eq!(p, &plan("foo"));
            }
            other => panic!("expected CodeOnly, got {other:?}"),
        }
        // MissingPrefix warning still fires — operator should amend.
        assert!(matches!(
            r.warnings.first(),
            Some(AttributionWarning::MissingPrefix { .. })
        ));
        assert_eq!(r.next_effective_plan, Some(plan("foo")));
    }

    #[test]
    fn classify_no_prefix_no_context_is_ad_hoc_no_warning() {
        let r = classify_with("random code", vec![], true, None, known(&[]));
        assert!(matches!(r.body, CommitBody::AdHoc(_)));
        assert!(r.warnings.is_empty());
        assert_eq!(r.next_effective_plan, None);
    }

    #[test]
    fn classify_no_prefix_multi_touch_is_multi_plan_with_warning() {
        let r = classify_with(
            "double touch",
            vec![touch("foo"), touch("bar")],
            false,
            None,
            known(&["foo", "bar"]),
        );
        assert!(matches!(r.body, CommitBody::MultiPlan(_)));
        assert_eq!(r.warnings.len(), 1);
    }

    #[test]
    fn classify_finalize_event_returns_finalize_commit() {
        let mut finalize: BTreeSet<PlanKey> = BTreeSet::new();
        finalize.insert(plan("foo"));
        let mut approvers: BTreeMap<PlanKey, u32> = BTreeMap::new();
        approvers.insert(plan("foo"), 2);
        let r = classify(ClassifierInputs {
            subject: "Finalize foo",
            plan_touches: &[],
            has_non_plan_code: false,
            plans_finalized_here: &finalize,
            finalize_approver_counts: &approvers,
            current_effective: Some(&plan("foo")),
            known_plans: &known(&["foo"]),
        });
        match &r.body {
            CommitBody::Finalize(f) => {
                assert_eq!(f.plan, plan("foo"));
                assert_eq!(f.approver_count, 2);
            }
            other => panic!("expected Finalize, got {other:?}"),
        }
        // Finalize is transparent to walk-back.
        assert_eq!(r.next_effective_plan, Some(plan("foo")));
    }

    #[test]
    fn classify_misc_prefix_preserves_walkback_through_misc_chain() {
        // Sequence: A=[foo] intro, B=[misc] doc, C=unprefixed code.
        // After A, effective=foo. After B (misc, no touches), effective
        // stays foo. After C (code, no prefix, parent=foo), should
        // inherit foo.
        let after_a = classify_with(
            "[foo] intro",
            vec![touch("foo")],
            false,
            None,
            known(&["foo"]),
        );
        assert_eq!(after_a.next_effective_plan, Some(plan("foo")));
        let after_b = classify_with(
            "[misc] doc fix",
            vec![],
            true,
            after_a.next_effective_plan.clone(),
            known(&["foo"]),
        );
        assert_eq!(after_b.next_effective_plan, Some(plan("foo")));
        let after_c = classify_with(
            "implement",
            vec![],
            true,
            after_b.next_effective_plan.clone(),
            known(&["foo"]),
        );
        match &after_c.body {
            CommitBody::Plan(PlanCommit::CodeOnly { plan: p, .. }) => {
                assert_eq!(p, &plan("foo"), "C should inherit foo from A through B");
            }
            other => panic!("expected CodeOnly(foo), got {other:?}"),
        }
    }
}
