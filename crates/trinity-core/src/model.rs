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
    MultiPlan {
        plans: std::collections::BTreeSet<PlanKey>,
    },
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
// Scaffolding for the `core-model-invalid-states-unrepresentable`
// rewrite. Compile-only until Phase 2 wires the flat-facts
// classifier output to `CommitMeta` / `AttributionWarning`;
// Phase 3 swaps `RepoState` over and deletes the legacy
// `CommitNode` and `Plan` timeline.
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

/// Identity of a review scope on a commit. Matches the
/// `.trinity/feedback/<scope>/<sha>/<author>.md` filesystem
/// shape: a plan stem (`<plan>`) or the reserved `_` segment
/// for ad-hoc reviews.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewScope {
    Plan(PlanKey),
    AdHoc,
}

/// Canonical review activity for a commit: who wrote what,
/// per scope. Feedback identity on disk is
/// `(scope, sha, author)` — a single SHA can receive
/// independent feedback from multiple plan scopes when the
/// commit touches multiple plans. Storage matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct CommitReviews {
    pub feedback: BTreeMap<ReviewScope, BTreeMap<AgentLabel, FeedbackBody>>,
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
    /// Title prefix names plan(s) that don't exist at fold
    /// time. The classifier sets `plan_attribution = None`
    /// and emits this; reviewers see the unknown names.
    UnknownPlanPrefix { unknown_names: Vec<String> },
    /// No prefix; classifier inferred attribution from
    /// touches or walk-back. `suggested_prefix` is what the
    /// master would amend the title to. String, because the
    /// suggestion may name multiple plans (e.g.
    /// `[plan-a,plan-b]`).
    MissingPrefix { suggested_prefix: String },
    /// Explicit prefix names plan(s) that disagree with the
    /// commit's `touches`. The prefix wins for
    /// `plan_attribution`; reviewers see both sides.
    AttributionMismatch {
        attributed: Vec<PlanKey>,
        touched: Vec<PlanKey>,
    },
    /// Emitted by projection helpers when a `plan_attribution`
    /// (or `touches` key) names a plan that no longer exists
    /// in `state.plans`. Not a fold-time warning — the
    /// classifier doesn't know about future deletions;
    /// projection surfaces it on each affected commit.
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

/// Flat-facts core model. Phase 2 of
/// `core-model-invalid-states-unrepresentable`: the new
/// canonical commit + plan shapes plus the pure classifier.
/// Phase 3 swaps `RepoState.commits` over to these and deletes
/// the legacy `CommitNode` / `Plan` / `PlanTimelineEvent`. The
/// `facts` namespace disappears in Phase 4 — these types move
/// up to `model` once the legacy shape is gone.
pub mod facts {
    use super::{AttributionWarning, CommitMeta, CommitReviews};
    use crate::ids::{AgentLabel, CommitSha, PlanKey};
    use serde::{Deserialize, Serialize};
    use std::collections::{BTreeMap, BTreeSet};

    /// Plan-file touch kinds. Independent of
    /// `vocab::PlanTouchKind` (the legacy diff-walker
    /// vocabulary which has only Intro/Revision and encodes
    /// deletes via "Revision + no new path"). The flat-facts
    /// model promotes deletion to a first-class touch kind.
    /// Phase 3's diff-to-facts mapper folds the legacy quirk
    /// into `TouchKind::Delete`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    #[cfg_attr(
        feature = "cache-encoding",
        derive(wincode::SchemaWrite, wincode::SchemaRead)
    )]
    #[serde(rename_all = "snake_case")]
    pub enum TouchKind {
        Intro,
        Revise,
        Delete,
    }

    /// Composable per-commit facts. The fold drives plan
    /// aggregates from these directly — there is no
    /// intermediate `PlanEffect` channel.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[cfg_attr(
        feature = "cache-encoding",
        derive(wincode::SchemaWrite, wincode::SchemaRead)
    )]
    pub struct CommitFacts {
        pub touches: BTreeMap<PlanKey, TouchKind>,
        pub finalizes: BTreeSet<PlanKey>,
        pub has_code_changes: bool,
        pub plan_attribution: Option<PlanKey>,
    }

    /// Canonical per-commit node. Phase 3 will rename this
    /// `CommitNode` once the legacy type is gone.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[cfg_attr(
        feature = "cache-encoding",
        derive(wincode::SchemaWrite, wincode::SchemaRead)
    )]
    pub struct CommitNode {
        pub meta: CommitMeta,
        pub touches: BTreeMap<PlanKey, TouchKind>,
        pub finalizes: BTreeSet<PlanKey>,
        pub has_code_changes: bool,
        pub plan_attribution: Option<PlanKey>,
        pub reviews: CommitReviews,
    }

    impl CommitNode {
        pub fn sha(&self) -> &CommitSha {
            &self.meta.sha
        }
        pub fn subject(&self) -> &str {
            &self.meta.subject
        }

        /// Plans this commit "belongs to" for timeline /
        /// projection purposes — union of touches, finalizes,
        /// and the attribution.
        pub fn associated_plans(&self) -> BTreeSet<&PlanKey> {
            let mut out: BTreeSet<&PlanKey> = self.touches.keys().collect();
            out.extend(self.finalizes.iter());
            if let Some(p) = self.plan_attribution.as_ref() {
                out.insert(p);
            }
            out
        }

        /// Iff exactly one entry in `touches` and no other
        /// facts. The touched plan is the inferred attribution
        /// for following code-only commits walking back
        /// through history.
        pub fn plan_only_touch(&self) -> Option<&PlanKey> {
            if self.touches.len() == 1 && !self.has_code_changes && self.finalizes.is_empty() {
                self.touches.keys().next()
            } else {
                None
            }
        }

        /// Iff no touches, no code, finalizes non-empty.
        /// Distinguishes lifecycle-only commits from ad-hoc
        /// reviewable ones in projection code.
        pub fn is_pure_lifecycle(&self) -> bool {
            self.touches.is_empty() && !self.has_code_changes && !self.finalizes.is_empty()
        }

        /// Iff this commit's facts make it ad-hoc reviewable:
        /// no plan touches, no plan attribution, code changes
        /// present. The actual policy still depends on config
        /// (`enable_misc_review`) and is computed by the gate
        /// projection — this method only answers the
        /// structural eligibility question.
        pub fn is_ad_hoc_eligible(&self) -> bool {
            self.touches.is_empty() && self.plan_attribution.is_none() && self.has_code_changes
        }
    }

    /// Folded per-plan workflow state — the product of the
    /// model. SHAs point into `RepoState.commits`; nothing
    /// here duplicates `CommitNode` facts.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[cfg_attr(
        feature = "cache-encoding",
        derive(wincode::SchemaWrite, wincode::SchemaRead)
    )]
    pub struct PlanState {
        pub id: PlanKey,
        pub plan_path: String,
        pub body: String,
        pub stage: PlanStage,
        pub timeline: Vec<CommitSha>,
        pub intro: CommitSha,
        pub latest_revision: Option<CommitSha>,
        pub latest_implementation: Option<CommitSha>,
        pub finalized_at: Option<CommitSha>,
        pub participants_cumulative: BTreeSet<AgentLabel>,
        pub last_activity_ts: i64,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[cfg_attr(
        feature = "cache-encoding",
        derive(wincode::SchemaWrite, wincode::SchemaRead)
    )]
    #[serde(rename_all = "snake_case")]
    pub enum PlanStage {
        /// Intro landed; no Revise, no implementation yet.
        Drafting,
        /// At least one implementation commit landed.
        Implementing,
        /// `finalized_at.is_some()`.
        Frozen,
    }

    /// Parallel aggregate for ad-hoc-eligible commits — they
    /// have no plan, but they share a review-participant set.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[cfg_attr(
        feature = "cache-encoding",
        derive(wincode::SchemaWrite, wincode::SchemaRead)
    )]
    pub struct AdHocState {
        pub commits: Vec<CommitSha>,
        pub participants_discovered: BTreeSet<AgentLabel>,
    }

    /// Title-prefix vocabulary the classifier recognizes.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TitlePrefix {
        /// `[misc]` — explicit ad-hoc opt-out.
        Misc,
        /// `[plan-a]` or `[plan-a,plan-b]` — explicit plan
        /// list. Whether the names are valid plan keys is
        /// checked in `classify`, not in the parser.
        Plans(Vec<String>),
    }

    /// Pure prefix parser. Returns `None` for "no recognized
    /// prefix"; the classifier then falls back to touch /
    /// walk-back inference.
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

    /// Inputs to the pure classifier. The daemon's diff walker
    /// computes `touches` / `finalizes` / `has_code_changes`
    /// from the commit's tree; the fold supplies
    /// `current_attribution` / `known_plans`. The classifier
    /// returns the facts (passing through the input facts and
    /// computing `plan_attribution`), warnings, and the
    /// updated walk-back carry.
    #[derive(Debug, Clone)]
    pub struct ClassifierInputs<'a> {
        pub subject: &'a str,
        pub touches: &'a BTreeMap<PlanKey, TouchKind>,
        pub finalizes: &'a BTreeSet<PlanKey>,
        pub has_code_changes: bool,
        /// Walk-back carry from the parent commit's
        /// classification.
        pub current_attribution: Option<&'a PlanKey>,
        /// Plans known to exist at this commit's parent state
        /// — used to validate `[plan-x]` prefix names.
        pub known_plans: &'a BTreeSet<PlanKey>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ClassifierOutput {
        pub facts: CommitFacts,
        pub warnings: Vec<AttributionWarning>,
        /// What the walk-back chain becomes after this
        /// commit. The fold assigns this to its carry
        /// verbatim.
        pub next_effective_attribution: Option<PlanKey>,
    }

    /// Pure classifier. Computes `plan_attribution` from the
    /// title prefix and walk-back; passes through
    /// `touches` / `finalizes` / `has_code_changes` unchanged;
    /// emits warnings for ambiguity / mismatch / missing
    /// prefix; computes the next walk-back carry.
    ///
    /// Attribution priority:
    /// 1. `[misc]` → `plan_attribution = None`, carry
    ///    preserved (descendants without a prefix still
    ///    inherit the prior attribution).
    /// 2. `[plan-x]` known → `Some(plan-x)`, carry becomes
    ///    `Some(plan-x)`. If `touches` names a different
    ///    plan, `AttributionMismatch` is emitted but the
    ///    prefix still wins for attribution.
    /// 3. `[plan-x]` unknown → `None`, carry preserved,
    ///    `UnknownPlanPrefix` warning.
    /// 4. `[a,b,...]` multi-plan prefix (≥2 known names) →
    ///    `None`, carry preserved (multi-plan commits are
    ///    transparent to walk-back). Each touched plan
    ///    independently reviews via touches; attribution
    ///    isn't a single-plan answer.
    /// 5. No prefix, exactly one entry in `touches` →
    ///    `Some(touched_plan)`, carry set to it,
    ///    `MissingPrefix` suggestion.
    /// 6. No prefix, multiple entries in `touches` →
    ///    `None`, carry preserved, `MissingPrefix`
    ///    suggestion naming all touched plans.
    /// 7. No prefix, no touches, code changes,
    ///    carry has attribution → inherit carry,
    ///    `MissingPrefix` suggestion.
    /// 8. Otherwise → `None`, carry preserved.
    pub fn classify(inputs: ClassifierInputs<'_>) -> ClassifierOutput {
        let prefix = parse_title_prefix(inputs.subject);
        let mut warnings: Vec<AttributionWarning> = Vec::new();

        let touched_plans: BTreeSet<&PlanKey> = inputs.touches.keys().collect();
        let touched_owned: Vec<PlanKey> = touched_plans.iter().map(|k| (*k).clone()).collect();

        let (plan_attribution, next_attribution) = match prefix {
            Some(TitlePrefix::Misc) => {
                // [misc]: this commit is ad-hoc, carry preserved.
                (None, inputs.current_attribution.cloned())
            }
            Some(TitlePrefix::Plans(names)) => {
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
                    (None, inputs.current_attribution.cloned())
                } else {
                    let valid: BTreeSet<PlanKey> = parsed.into_iter().flatten().collect();
                    if valid.len() == 1 {
                        let plan = valid.into_iter().next().expect("len==1");
                        // AttributionMismatch fires when the touched
                        // set disagrees with the prefix — either the
                        // prefix's plan isn't in touches, or touches
                        // names extra plans the prefix doesn't list.
                        let touched_set: BTreeSet<PlanKey> =
                            touched_owned.iter().cloned().collect();
                        let expected: BTreeSet<PlanKey> = std::iter::once(plan.clone()).collect();
                        if !touched_set.is_empty() && touched_set != expected {
                            warnings.push(AttributionWarning::AttributionMismatch {
                                attributed: vec![plan.clone()],
                                touched: touched_owned.clone(),
                            });
                        }
                        let next = Some(plan.clone());
                        (Some(plan), next)
                    } else {
                        // Multi-plan prefix: also check for mismatch.
                        let attributed: Vec<PlanKey> = valid.iter().cloned().collect();
                        let touched_set: BTreeSet<&PlanKey> = touched_plans.clone();
                        let any_missing = attributed.iter().any(|p| !touched_set.contains(p));
                        let extra_touched = touched_plans.iter().any(|p| !valid.contains(*p));
                        if any_missing || extra_touched {
                            warnings.push(AttributionWarning::AttributionMismatch {
                                attributed: attributed.clone(),
                                touched: touched_owned.clone(),
                            });
                        }
                        // Multi-plan is transparent to walk-back;
                        // there's no single attribution to carry.
                        (None, inputs.current_attribution.cloned())
                    }
                }
            }
            None => {
                // No prefix: infer from touches or walk-back. Suppress
                // MissingPrefix when the inferred attribution equals
                // the carry — the chain explains it; no operator
                // amendment is needed. Warn only when the chain
                // DISAGREES with what inference produces.
                if touched_plans.len() == 1 {
                    let plan = touched_plans.iter().next().expect("len==1");
                    let owned = (*plan).clone();
                    if inputs.current_attribution != Some(&owned) {
                        warnings.push(AttributionWarning::MissingPrefix {
                            suggested_prefix: format!("[{}]", plan.as_str()),
                        });
                    }
                    (Some(owned.clone()), Some(owned))
                } else if touched_plans.len() >= 2 {
                    let suggested = format!(
                        "[{}]",
                        touched_plans
                            .iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(",")
                    );
                    warnings.push(AttributionWarning::MissingPrefix {
                        suggested_prefix: suggested,
                    });
                    (None, inputs.current_attribution.cloned())
                } else if inputs.has_code_changes
                    && let Some(carry) = inputs.current_attribution
                {
                    // Pure carry inheritance — no warning (the
                    // implementation chain is the expected shape).
                    let owned = carry.clone();
                    (Some(owned.clone()), Some(owned))
                } else {
                    (None, inputs.current_attribution.cloned())
                }
            }
        };

        let facts = CommitFacts {
            touches: inputs.touches.clone(),
            finalizes: inputs.finalizes.clone(),
            has_code_changes: inputs.has_code_changes,
            plan_attribution,
        };

        ClassifierOutput {
            facts,
            warnings,
            next_effective_attribution: next_attribution,
        }
    }
}

#[cfg(test)]
mod facts_tests {
    use super::facts::*;
    use super::{AttributionWarning, CommitReviews, ReviewScope};
    use crate::ids::{AgentLabel, CommitSha, PlanKey};
    use std::collections::{BTreeMap, BTreeSet};

    fn plan(s: &str) -> PlanKey {
        PlanKey::parse(s).unwrap()
    }
    fn known(names: &[&str]) -> BTreeSet<PlanKey> {
        names.iter().map(|n| plan(n)).collect()
    }
    fn no_touches() -> BTreeMap<PlanKey, TouchKind> {
        BTreeMap::new()
    }
    fn no_finalizes() -> BTreeSet<PlanKey> {
        BTreeSet::new()
    }
    fn touches(items: &[(&str, TouchKind)]) -> BTreeMap<PlanKey, TouchKind> {
        items.iter().map(|(p, k)| (plan(p), *k)).collect()
    }

    fn classify_with<'a>(
        subject: &'a str,
        touches: &'a BTreeMap<PlanKey, TouchKind>,
        finalizes: &'a BTreeSet<PlanKey>,
        has_code: bool,
        carry: Option<&'a PlanKey>,
        known: &'a BTreeSet<PlanKey>,
    ) -> ClassifierOutput {
        classify(ClassifierInputs {
            subject,
            touches,
            finalizes,
            has_code_changes: has_code,
            current_attribution: carry,
            known_plans: known,
        })
    }

    // ============ Attribution from explicit prefix ============

    #[test]
    fn explicit_known_prefix_sets_attribution() {
        let k = known(&["foo"]);
        let t = touches(&[("foo", TouchKind::Revise)]);
        let out = classify_with("[foo] revise", &t, &no_finalizes(), false, None, &k);
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        assert_eq!(out.next_effective_attribution, Some(plan("foo")));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn unknown_prefix_warns_and_clears_attribution() {
        let k = known(&["other"]);
        let out = classify_with(
            "[ghost] random",
            &no_touches(),
            &no_finalizes(),
            true,
            None,
            &k,
        );
        assert_eq!(out.facts.plan_attribution, None);
        assert!(matches!(
            out.warnings.as_slice(),
            [AttributionWarning::UnknownPlanPrefix { unknown_names }]
                if unknown_names == &vec!["ghost".to_string()]
        ));
    }

    #[test]
    fn misc_prefix_ad_hoc_preserves_carry() {
        let k = known(&["foo"]);
        let parent = plan("foo");
        let out = classify_with(
            "[misc] one-off",
            &no_touches(),
            &no_finalizes(),
            true,
            Some(&parent),
            &k,
        );
        assert_eq!(out.facts.plan_attribution, None);
        // Carry preserved — descendants without a prefix still
        // inherit "foo".
        assert_eq!(out.next_effective_attribution, Some(plan("foo")));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn attribution_mismatch_prefix_wins_warning_emitted() {
        let k = known(&["foo", "bar"]);
        let t = touches(&[("bar", TouchKind::Revise)]);
        let out = classify_with(
            "[foo] but touched bar",
            &t,
            &no_finalizes(),
            false,
            None,
            &k,
        );
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        assert!(matches!(
            out.warnings.as_slice(),
            [AttributionWarning::AttributionMismatch {
                attributed, touched
            }] if attributed == &vec![plan("foo")] && touched == &vec![plan("bar")]
        ));
    }

    // ============ Multi-plan prefix and touches ============

    #[test]
    fn multi_plan_prefix_clears_attribution_carry_preserved() {
        let k = known(&["foo", "bar"]);
        let parent = plan("baz");
        let t = touches(&[("foo", TouchKind::Revise), ("bar", TouchKind::Revise)]);
        let out = classify_with(
            "[foo,bar] cross-cut",
            &t,
            &no_finalizes(),
            false,
            Some(&parent),
            &k,
        );
        assert_eq!(out.facts.plan_attribution, None);
        assert_eq!(out.next_effective_attribution, Some(plan("baz")));
        assert!(out.warnings.is_empty());
        assert_eq!(out.facts.touches.len(), 2);
    }

    #[test]
    fn no_prefix_multi_plan_touches_missing_prefix_warning() {
        let k = known(&["foo", "bar"]);
        let t = touches(&[("foo", TouchKind::Revise), ("bar", TouchKind::Revise)]);
        let out = classify_with("cross-cut", &t, &no_finalizes(), false, None, &k);
        assert_eq!(out.facts.plan_attribution, None);
        assert!(matches!(
            out.warnings.as_slice(),
            [AttributionWarning::MissingPrefix { suggested_prefix }]
                if suggested_prefix == "[bar,foo]"
        ));
    }

    // ============ Walk-back attribution ============

    #[test]
    fn no_prefix_single_touch_infers_attribution() {
        let k = known(&["foo"]);
        let t = touches(&[("foo", TouchKind::Intro)]);
        let out = classify_with("introduce foo", &t, &no_finalizes(), false, None, &k);
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        assert_eq!(out.next_effective_attribution, Some(plan("foo")));
        assert!(matches!(
            out.warnings.as_slice(),
            [AttributionWarning::MissingPrefix { suggested_prefix }]
                if suggested_prefix == "[foo]"
        ));
    }

    #[test]
    fn no_prefix_no_touches_inherits_carry() {
        let k = known(&["foo"]);
        let parent = plan("foo");
        let out = classify_with(
            "implement",
            &no_touches(),
            &no_finalizes(),
            true,
            Some(&parent),
            &k,
        );
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        assert_eq!(out.next_effective_attribution, Some(plan("foo")));
        // Pure carry inheritance is the expected implementation
        // shape; no MissingPrefix noise.
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn no_prefix_single_touch_matching_carry_suppresses_warning() {
        let k = known(&["foo"]);
        let parent = plan("foo");
        let t = touches(&[("foo", TouchKind::Revise)]);
        let out = classify_with("revise foo", &t, &no_finalizes(), false, Some(&parent), &k);
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        // Carry already explained the attribution — no MissingPrefix.
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn no_prefix_single_touch_differing_from_carry_still_warns() {
        let k = known(&["foo", "bar"]);
        let parent = plan("bar");
        let t = touches(&[("foo", TouchKind::Revise)]);
        let out = classify_with("revise foo", &t, &no_finalizes(), false, Some(&parent), &k);
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        assert!(matches!(
            out.warnings.as_slice(),
            [AttributionWarning::MissingPrefix { suggested_prefix }]
                if suggested_prefix == "[foo]"
        ));
    }

    #[test]
    fn explicit_single_plan_prefix_with_extra_touch_emits_mismatch() {
        let k = known(&["foo", "bar"]);
        let t = touches(&[("foo", TouchKind::Revise), ("bar", TouchKind::Revise)]);
        let out = classify_with(
            "[foo] touched bar too",
            &t,
            &no_finalizes(),
            false,
            None,
            &k,
        );
        // Prefix still wins for attribution.
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        // Mismatch because touches names plan-b too.
        assert!(matches!(
            out.warnings.as_slice(),
            [AttributionWarning::AttributionMismatch { attributed, touched }]
                if attributed == &vec![plan("foo")]
                    && touched.contains(&plan("bar"))
                    && touched.contains(&plan("foo"))
        ));
    }

    #[test]
    fn explicit_single_plan_prefix_matching_exact_touch_no_warning() {
        let k = known(&["foo"]);
        let t = touches(&[("foo", TouchKind::Revise)]);
        let out = classify_with("[foo] revise", &t, &no_finalizes(), false, None, &k);
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn explicit_single_plan_prefix_no_touches_no_warning() {
        // [plan-a] with no plan touch (e.g. code-only commit). The
        // prefix is the attribution; nothing to mismatch.
        let k = known(&["foo"]);
        let out = classify_with(
            "[foo] implement",
            &no_touches(),
            &no_finalizes(),
            true,
            None,
            &k,
        );
        assert_eq!(out.facts.plan_attribution, Some(plan("foo")));
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn no_prefix_no_touches_no_code_no_attribution() {
        let k = known(&["foo"]);
        let out = classify_with("noop", &no_touches(), &no_finalizes(), false, None, &k);
        assert_eq!(out.facts.plan_attribution, None);
        assert_eq!(out.next_effective_attribution, None);
    }

    #[test]
    fn misc_preserves_carry_through_descendants() {
        // a (intro foo) → b ([misc]) → c (code)
        // c should still inherit foo via the carry.
        let k = known(&["foo"]);
        let a = classify_with(
            "introduce foo",
            &touches(&[("foo", TouchKind::Intro)]),
            &no_finalizes(),
            false,
            None,
            &k,
        );
        assert_eq!(a.next_effective_attribution, Some(plan("foo")));

        let b_carry = a.next_effective_attribution.clone();
        let b = classify_with(
            "[misc] tweak",
            &no_touches(),
            &no_finalizes(),
            true,
            b_carry.as_ref(),
            &k,
        );
        assert_eq!(b.facts.plan_attribution, None);
        assert_eq!(b.next_effective_attribution, Some(plan("foo")));

        let c_carry = b.next_effective_attribution.clone();
        let c = classify_with(
            "implement bit",
            &no_touches(),
            &no_finalizes(),
            true,
            c_carry.as_ref(),
            &k,
        );
        assert_eq!(c.facts.plan_attribution, Some(plan("foo")));
    }

    // ============ Touches pass through ============

    #[test]
    fn touch_kind_intro_revise_delete_preserved() {
        let k = known(&["a", "b", "c"]);
        let t = touches(&[
            ("a", TouchKind::Intro),
            ("b", TouchKind::Revise),
            ("c", TouchKind::Delete),
        ]);
        let out = classify_with("[a,b,c]", &t, &no_finalizes(), false, None, &k);
        assert_eq!(out.facts.touches.get(&plan("a")), Some(&TouchKind::Intro));
        assert_eq!(out.facts.touches.get(&plan("b")), Some(&TouchKind::Revise));
        assert_eq!(out.facts.touches.get(&plan("c")), Some(&TouchKind::Delete));
    }

    // ============ Finalize + touch coexist ============

    #[test]
    fn finalize_plus_touch_both_facts_survive() {
        let k = known(&["a", "b"]);
        let t = touches(&[("b", TouchKind::Revise)]);
        let mut f = BTreeSet::new();
        f.insert(plan("a"));
        let out = classify_with("[b] finalize a too", &t, &f, false, None, &k);
        assert!(out.facts.finalizes.contains(&plan("a")));
        assert!(out.facts.touches.contains_key(&plan("b")));
        assert_eq!(out.facts.plan_attribution, Some(plan("b")));
    }

    #[test]
    fn multiple_finalizes_all_preserved() {
        let k = known(&["a", "b", "c"]);
        let mut f = BTreeSet::new();
        f.insert(plan("a"));
        f.insert(plan("b"));
        f.insert(plan("c"));
        let out = classify_with("freeze trio", &no_touches(), &f, false, None, &k);
        assert_eq!(out.facts.finalizes.len(), 3);
    }

    // ============ CommitNode helpers ============

    fn make_node(
        touches: BTreeMap<PlanKey, TouchKind>,
        finalizes: BTreeSet<PlanKey>,
        has_code: bool,
        attribution: Option<PlanKey>,
    ) -> CommitNode {
        CommitNode {
            meta: super::CommitMeta {
                sha: CommitSha::parse("deadbeef").unwrap(),
                author_ts: 0,
                subject: "subj".into(),
                warnings: vec![],
            },
            touches,
            finalizes,
            has_code_changes: has_code,
            plan_attribution: attribution,
            reviews: CommitReviews::default(),
        }
    }

    #[test]
    fn associated_plans_union_of_touches_finalizes_attribution() {
        let node = make_node(
            touches(&[("a", TouchKind::Revise)]),
            {
                let mut s = BTreeSet::new();
                s.insert(plan("b"));
                s
            },
            true,
            Some(plan("c")),
        );
        let assoc = node.associated_plans();
        assert_eq!(assoc.len(), 3);
        assert!(assoc.contains(&plan("a")));
        assert!(assoc.contains(&plan("b")));
        assert!(assoc.contains(&plan("c")));
    }

    #[test]
    fn plan_only_touch_some_iff_single_pure_touch() {
        let yes = make_node(
            touches(&[("a", TouchKind::Revise)]),
            no_finalizes(),
            false,
            None,
        );
        assert_eq!(yes.plan_only_touch(), Some(&plan("a")));

        let two = make_node(
            touches(&[("a", TouchKind::Revise), ("b", TouchKind::Revise)]),
            no_finalizes(),
            false,
            None,
        );
        assert_eq!(two.plan_only_touch(), None);

        let with_code = make_node(
            touches(&[("a", TouchKind::Revise)]),
            no_finalizes(),
            true,
            None,
        );
        assert_eq!(with_code.plan_only_touch(), None);

        let with_finalize = make_node(
            touches(&[("a", TouchKind::Revise)]),
            {
                let mut s = BTreeSet::new();
                s.insert(plan("a"));
                s
            },
            false,
            None,
        );
        assert_eq!(with_finalize.plan_only_touch(), None);
    }

    #[test]
    fn is_pure_lifecycle_only_when_finalizes_alone() {
        let yes = make_node(
            no_touches(),
            {
                let mut s = BTreeSet::new();
                s.insert(plan("a"));
                s
            },
            false,
            None,
        );
        assert!(yes.is_pure_lifecycle());

        let with_code = make_node(
            no_touches(),
            {
                let mut s = BTreeSet::new();
                s.insert(plan("a"));
                s
            },
            true,
            None,
        );
        assert!(!with_code.is_pure_lifecycle());
    }

    #[test]
    fn is_ad_hoc_eligible_code_only_no_attribution() {
        let yes = make_node(no_touches(), no_finalizes(), true, None);
        assert!(yes.is_ad_hoc_eligible());

        let with_attribution = make_node(no_touches(), no_finalizes(), true, Some(plan("a")));
        assert!(!with_attribution.is_ad_hoc_eligible());

        let no_code = make_node(no_touches(), no_finalizes(), false, None);
        assert!(!no_code.is_ad_hoc_eligible());
    }

    // ============ Scope-aware review storage ============

    #[test]
    fn commit_reviews_scope_aware_same_author_distinct_scopes() {
        use super::FeedbackBody;
        use crate::vocab::Verdict;
        let mut reviews = CommitReviews::default();
        let author = AgentLabel::parse("codex").unwrap();
        let scope_a = ReviewScope::Plan(plan("a"));
        let scope_b = ReviewScope::Plan(plan("b"));

        reviews.feedback.entry(scope_a.clone()).or_default().insert(
            author.clone(),
            FeedbackBody {
                verdict: Verdict::Approve,
                body: "ok for a".into(),
                created_at: 1,
            },
        );
        reviews.feedback.entry(scope_b.clone()).or_default().insert(
            author.clone(),
            FeedbackBody {
                verdict: Verdict::RequestChanges,
                body: "not for b".into(),
                created_at: 2,
            },
        );

        // Same author wrote different verdicts for two scopes —
        // neither overwrites the other.
        let a = reviews
            .feedback
            .get(&scope_a)
            .unwrap()
            .get(&author)
            .unwrap();
        let b = reviews
            .feedback
            .get(&scope_b)
            .unwrap()
            .get(&author)
            .unwrap();
        assert_eq!(a.verdict, Verdict::Approve);
        assert_eq!(b.verdict, Verdict::RequestChanges);
    }
}
