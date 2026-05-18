//! Daemon-side fold-state types. The shapes the daemon stores
//! in memory live here; the daemon also publishes them on the
//! wire — newtypes serialize transparently, so daemon-side
//! validation survives round-trip.
//!
//! Wire-only fields (today: `body_html` on `dto::Feedback`) are
//! NOT on these types. Phase 4 of `trinity-core-unification.md`
//! formalises the model/api split — until then `dto::Feedback`
//! and `dto::CommitGate` continue to carry rendered HTML, and
//! the daemon's response projection builds those from the
//! storage shapes here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, ContentHash, PlanKey};
use crate::vocab::{CommitGateState, CommitKind, PlanLifecycle, PlanWorktreeStatus};

/// One reviewer's verdict file as the daemon stores it. `body` is
/// raw markdown (matches the wire's `body_raw`; the wire renames
/// to make space for `body_html`). Rendered HTML lives on
/// `dto::Feedback` and is computed at projection time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feedback {
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

/// One commit in a plan's life as observed by the fold. The `kind`
/// is the per-plan classification; `gate` is `Some` for reviewable
/// kinds (`PlanOnly | CodeOnly | Mixed`) and `None` for
/// non-reviewable kinds (`MultiPlan`, `Finalize`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanTimelineEvent {
    pub sha: CommitSha,
    pub kind: CommitKind,
    pub author_ts: i64,
    pub subject: String,
    pub gate: Option<CommitGate>,
}

/// Per-cycle summary surfaced in the plan-detail wire under
/// `archived_cycles`. One entry per freeze event (today: 0 or 1).
/// Lives in `model` because the fold-state stores it directly on
/// `Plan.archived_cycles`; api response shapes re-export it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedCycle {
    /// The commit at which the cycle was closed (the freeze event).
    pub closer: CommitSha,
    /// Number of approving-reviewer files in
    /// `.trinity/finished/<stem>/` at that freeze commit's tree.
    pub approver_count: u32,
}

/// One plan's full fold state. The daemon stores this directly and
/// the wire response shape is built by projecting selected fields
/// (plus `body_html` rendering) at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Chronological per-commit log of this plan's life — the
    /// single source of truth for plan revisions, implementation
    /// commits, reviewable commits, per-commit gates, and per-
    /// commit metadata. All projection queries are filters or
    /// reverse-scans over this list — no parallel buckets.
    pub timeline: Vec<PlanTimelineEvent>,
    /// Per-cycle summaries derived from the fold's freeze events
    /// (one entry per freeze). Today the monotone rule means this
    /// has length 0 or 1.
    pub archived_cycles: Vec<ArchivedCycle>,
}

impl Plan {
    /// Find the timeline event for a SHA, if this plan has one.
    pub fn event_for(&self, sha: &CommitSha) -> Option<&PlanTimelineEvent> {
        self.timeline.iter().find(|e| &e.sha == sha)
    }

    pub fn event_for_mut(&mut self, sha: &CommitSha) -> Option<&mut PlanTimelineEvent> {
        self.timeline.iter_mut().find(|e| &e.sha == sha)
    }

    /// The latest reviewable commit event, if any. Reverse scan.
    pub fn latest_reviewable_event(&self) -> Option<&PlanTimelineEvent> {
        self.timeline.iter().rev().find(|e| e.kind.is_reviewable())
    }

    /// The freeze commit's SHA, if this plan has frozen. Derived
    /// from the timeline — the last `CommitKind::Finalize` event is
    /// the freeze.
    pub fn frozen_at(&self) -> Option<&CommitSha> {
        self.timeline
            .iter()
            .rev()
            .find_map(|e| matches!(e.kind, CommitKind::Finalize).then_some(&e.sha))
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
