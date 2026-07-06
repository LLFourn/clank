//! Wire-shape DTOs the CLI produces for its preview / event flows.
//!
//! These are typed projections — the CLI builds them from the
//! sans-io fold + filesystem reads and the consumer (`clank finish`,
//! `clank purge`, the runtime's event loop) branches on the typed
//! shape.
//!
//! Tagged enums use `#[serde(tag = "kind")]` so kind-dependent
//! shapes have ONE discriminator in the type system AND on the wire.

use serde::{Deserialize, Serialize};

use crate::vocab::PlanLifecycle;

pub use crate::model::CommitGate;
pub use crate::model::Feedback;

// ============================================================
// Diff types — wire format for structured diff payloads.
// ============================================================

/// One file's diff inside a structured-diff payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    #[serde(default)]
    pub old_path: Option<String>,
    pub additions: usize,
    pub deletions: usize,
    pub mode: FileDiffMode,
    pub binary: bool,
    pub always_folded: bool,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDiffMode {
    Added,
    Removed,
    Renamed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// One line inside a `DiffHunk`. Tagged by `kind` on the wire; each
/// variant carries exactly the lineno fields valid for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiffLine {
    Insert {
        content: String,
        new_lineno: usize,
    },
    Delete {
        content: String,
        old_lineno: usize,
    },
    Context {
        content: String,
        old_lineno: usize,
        new_lineno: usize,
    },
    /// Hunk-header / file-header line (`@@ ... @@`, `+++ ...`, etc.).
    Meta {
        content: String,
    },
}

// ============================================================
// `clank finish` preview
// ============================================================

/// Everything `clank finish` needs to validate before sealing the
/// approval snapshot and committing the finalize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishPreviewResponse {
    pub plan_id: String,
    pub plan_path: String,
    pub readiness: FinalizeReadiness,
    pub gate_state: crate::vocab::CommitGateState,
    pub latest_reviewable_sha: Option<crate::ids::CommitSha>,
    pub plan_worktree_status: crate::vocab::PlanWorktreeStatus,
    pub is_finished: bool,
    /// Approval files the CLI seals when `readiness == Ready`. Each
    /// entry's `body_hash` was computed from `feedback.body`; the
    /// CLI re-reads and re-hashes each `source_path` before copying
    /// — any drift aborts the seal.
    pub sealed_approvals: Vec<SealedApproval>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FinalizeReadiness {
    Ready,
    AlreadyFinished,
    Blocked { reasons: Vec<FinalizeBlockReason> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FinalizeBlockReason {
    NoReviewableCommit,
    /// Gate state on the latest reviewable commit is something
    /// other than `Finished`. Holds the actual state so the
    /// caller can render a helpful message ("continued — needs a
    /// FINISHED vote", "changes requested — address them",
    /// etc.).
    NotFinished {
        state: crate::vocab::CommitGateState,
    },
    /// An unanswered plan-scoped (or repo-wide) block is pending. Its
    /// own reason — NOT a `NotFinished` gate state — because a block
    /// is a question to the HUMAN, not a review verdict: it must
    /// refuse finalize even on a master-only repo (where gate states
    /// are waived entirely) and `finish --force` must not waive it
    /// (tui-plan-actions-page, codex 137bd3e).
    OpenBlock,
    PlanFileMissing,
    PlanFileDirty,
}

/// One approving feedback the CLI's preview projected as part of
/// the continued gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedApproval {
    pub author: crate::ids::AgentLabel,
    /// Repo-relative path:
    /// `.clank/agents/<author>/feedback/<stem>/<ref>.md`.
    pub source_path: String,
    pub body_hash: crate::ids::ContentHash,
}

// ============================================================
// `clank purge` / `clank finish --purge` rewrite preview
// ============================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewritePreviewResponse {
    pub plan_id: String,
    pub plan_stem: crate::ids::PlanKey,
    pub intro_sha: Option<crate::ids::CommitSha>,
    pub head_sha: crate::ids::CommitSha,
    /// True iff `[intro_sha, head_sha]` is first-parent linear.
    pub linear: bool,
    pub commits: Vec<RewriteCommit>,
    /// Where a squash's COLLAPSE ends: `Some(finalized_at)` for a finished
    /// plan (only the plan's own `[intro..finalized_at]` run collapses;
    /// commits after it are RESTACKED individually on top), `None` for an
    /// active plan (collapse to `head_sha`). Without this bound, squashing
    /// a BURIED plan would sweep every later commit into the collapse.
    #[serde(default)]
    pub squash_tip: Option<crate::ids::CommitSha>,
    /// Strippable paths at the squash tip's tree (`squash_tip` when set,
    /// else `head_sha`). Used by squash mode to compute the correct
    /// collapsed tree.
    #[serde(default)]
    pub head_strip_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewriteCommit {
    pub sha: crate::ids::CommitSha,
    pub subject: String,
    pub disposition: RewriteDisposition,
    /// True if the commit is NOT attributed to this plan.
    pub foreign: bool,
    pub strip_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RewriteDisposition {
    Drop,
    KeepVerbatim,
    Rewrite,
}

/// All-plans purge preview. Same `commits` shape as
/// `RewritePreviewResponse` so the rewrite engine consumes one type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurgeAllPreviewResponse {
    pub repo: String,
    pub head_sha: crate::ids::CommitSha,
    pub linear: bool,
    /// Earliest commit in the first-parent walk that touched ANY
    /// `.clank/` path. `None` when no commit ever touched `.clank/`.
    pub intro_sha: Option<crate::ids::CommitSha>,
    /// Distinct plan stems whose plan/finalize artifacts appear in
    /// the walk. Sorted, deduped.
    pub plans_touched: Vec<crate::ids::PlanKey>,
    pub commits: Vec<RewriteCommit>,
    #[serde(default)]
    pub head_strip_paths: Vec<String>,
}

// ============================================================
// Live events (consumed by `crates/cli/src/runtime.rs`)
// ============================================================

/// One SSE-style event. Tagged by `scope` so the `repo` / `plan`
/// distinction is structural, not conventional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum LiveEvent {
    Repo(RepoEvent),
    Plan(PlanEvent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEvent {
    pub ts: i64,
    #[serde(flatten)]
    pub payload: RepoEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepoEventPayload {
    RepoRebuilt {},
    RepoUnwatched { plan_count: usize },
}

impl std::fmt::Display for RepoEventPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RepoEventPayload::RepoRebuilt {} => "repo_rebuilt",
            RepoEventPayload::RepoUnwatched { .. } => "repo_unwatched",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEvent {
    pub ts: i64,
    pub plan_id: String,
    pub lifecycle: PlanLifecycle,
    #[serde(flatten)]
    pub payload: PlanEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanEventPayload {
    PlanWorktreeChanged { path: String },
    FeedbackChanged {},
    FeedbackRemoved {},
}

impl std::fmt::Display for PlanEventPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            PlanEventPayload::PlanWorktreeChanged { .. } => "plan_worktree_changed",
            PlanEventPayload::FeedbackChanged {} => "feedback_changed",
            PlanEventPayload::FeedbackRemoved {} => "feedback_removed",
        })
    }
}
