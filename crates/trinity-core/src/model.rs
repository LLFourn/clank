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
}
