//! In-memory state for the filesystem-truth model. All data here is a
//! derived cache from git + working tree; nothing is persisted.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` for the architecture.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanKey, PlanPath, RepoBasename};

pub type RepoRoot = PathBuf;

#[derive(Debug, Default)]
pub struct Trinity {
    pub repos: BTreeMap<RepoRoot, RepoState>,
    /// Basename → canonical repo root index. Maintained alongside
    /// `repos`. First registration wins on basename collision; later
    /// registrations are dropped and the daemon logs WARN.
    pub repo_basenames: BTreeMap<RepoBasename, RepoRoot>,
    pub live_events: VecDeque<LiveEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub root: PathBuf,
    pub plans: BTreeMap<PlanKey, Plan>,
    pub head: Option<CommitSha>,
    /// (commit_sha → attribution). Lookup table; iteration order is
    /// SHA-lex, not chronological. Use `commit_order` to walk the
    /// history in chronological (first-parent oldest-first) order.
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    /// Per-commit plan touches, including multi-plan commits. Attribution
    /// remains single-plan for implementation ownership; this index lets
    /// each touched plan still see its own revision/done_move.
    pub plan_touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>>,
    /// Commits in chronological order (first-parent walk, oldest first).
    /// Parallels `attribution`'s keys but preserves history order, which
    /// `BTreeMap` does not.
    pub commit_order: Vec<CommitSha>,
    /// Plans whose disk state is contradictory at rebuild time — the same
    /// stem exists at both `.trinity/plans/<stem>.md` and
    /// `.trinity/plans/done/<stem>.md`. Conflicting keys are omitted from
    /// [`Self::plans`] so neither file silently wins; resolution helpers
    /// surface the conflict so callers can flag it instead of routing
    /// work.
    pub plan_conflicts: BTreeMap<PlanKey, Vec<PlanPath>>,
}

impl RepoState {
    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            plans: BTreeMap::new(),
            head: None,
            attribution: BTreeMap::new(),
            plan_touches: BTreeMap::new(),
            commit_order: Vec::new(),
            plan_conflicts: BTreeMap::new(),
        }
    }

    /// Chronological timeline of one plan's activity. The renderer (web
    /// UI, MCP responses, anything else) walks this list to display events
    /// in order without needing to recombine attribution + feedback maps
    /// itself.
    ///
    /// Order: commits touching the plan file or attributed to `plan_key`
    /// as implementation work, in first-parent walk order (oldest first).
    /// Each commit is followed by the reviews targeting it (plan reviews
    /// for plan_touch commits, impl reviews for has_code_changes commits)
    /// sorted by author. Held flat-drop feedback files come last with no
    /// target.
    ///
    /// Pure; sans-IO. Returns an empty vec if the plan is unknown.
    pub fn timeline_for(&self, plan_key: &PlanKey) -> Vec<TimelineEvent> {
        let Some(plan) = self.plans.get(plan_key) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for sha in &self.commit_order {
            let attr = self.attribution.get(sha);
            let plan_touch = self
                .plan_touches
                .get(sha)
                .and_then(|touches| touches.iter().find(|(key, _)| key == plan_key))
                .map(|(_, kind)| *kind);
            let has_code_changes = matches!(
                attr,
                Some(AttributionResult::Attributed {
                    session: sid,
                    has_code_changes: true,
                    ..
                }) if sid == plan_key
            );
            if plan_touch.is_none() && !has_code_changes {
                continue;
            }
            out.push(TimelineEvent::Commit {
                sha: sha.clone(),
                plan_touch,
                has_code_changes,
            });
            for ((target, author), fb) in &plan.plan_feedback {
                if target == sha {
                    out.push(TimelineEvent::Review {
                        phase: TimelinePhase::Plan,
                        target: target.clone(),
                        author: author.clone(),
                        verdict: fb.verdict,
                    });
                }
            }
            for ((target, author), fb) in &plan.impl_feedback {
                if target == sha {
                    out.push(TimelineEvent::Review {
                        phase: TimelinePhase::Impl,
                        target: target.clone(),
                        author: author.clone(),
                        verdict: fb.verdict,
                    });
                }
            }
        }
        for held in &plan.held_plan_feedback {
            out.push(TimelineEvent::HeldFeedback {
                author: held.author.clone(),
                reason: held.reason,
            });
        }
        out
    }

    /// Stable digest over every meaningful field in the state. Two states
    /// with the same digest are observably identical to callers; equal
    /// digests across rebuilds mean nothing changed, so the runtime can
    /// skip the broadcast (no spurious chime).
    ///
    /// The digest is intentionally coarse — it doesn't tell you *what*
    /// changed, only that something did. That matches the user's
    /// stated need: ping on change, no diff required.
    pub fn digest(&self) -> StateDigest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"trinity-state-v1\n");
        hasher.update(self.root.to_string_lossy().as_bytes());
        hasher.update(b"\nhead=");
        hasher.update(
            self.head
                .as_ref()
                .map(|h| h.as_str())
                .unwrap_or("")
                .as_bytes(),
        );

        // BTreeMap iterates by key (sorted), so this is deterministic.
        hasher.update(b"\nplans[");
        for (key, plan) in &self.plans {
            hasher.update(key.as_str().as_bytes());
            hasher.update(b"|path=");
            hasher.update(plan.plan_path.to_string_lossy().as_bytes());
            hasher.update(b"|body_hash=");
            hasher.update(plan.body_hash.as_str().as_bytes());
            hasher.update(b"|intro=");
            hasher.update(plan.plan_intro.as_str().as_bytes());
            hasher.update(b"|intro_parent=");
            hasher.update(
                plan.plan_intro_parent
                    .as_ref()
                    .map(|p| p.as_str())
                    .unwrap_or("")
                    .as_bytes(),
            );
            hasher.update(b"|plan_fb=[");
            for ((sha, author), fb) in &plan.plan_feedback {
                hasher.update(sha.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(author.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(fb.verdict.as_str().as_bytes());
                hasher.update(b";");
            }
            hasher.update(b"]|impl_fb=[");
            for ((sha, author), fb) in &plan.impl_feedback {
                hasher.update(sha.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(author.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(fb.verdict.as_str().as_bytes());
                hasher.update(b";");
            }
            hasher.update(b"]|held=[");
            for held in &plan.held_plan_feedback {
                hasher.update(held.author.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(held.reason.as_bytes());
                hasher.update(b";");
            }
            hasher.update(b"]\n");
        }
        hasher.update(b"]\nattribution[");
        for (sha, attr) in &self.attribution {
            hasher.update(sha.as_str().as_bytes());
            hasher.update(b"=");
            match attr {
                AttributionResult::Attributed {
                    session,
                    plan_touch,
                    has_code_changes,
                } => {
                    hasher.update(b"A:");
                    hasher.update(session.as_str().as_bytes());
                    hasher.update(b":");
                    hasher.update(
                        plan_touch
                            .as_ref()
                            .map(|k| k.as_str())
                            .unwrap_or("none")
                            .as_bytes(),
                    );
                    hasher.update(b":");
                    hasher.update(if *has_code_changes {
                        b"code"
                    } else {
                        b"nocode"
                    });
                }
                AttributionResult::Unattributed => {
                    hasher.update(b"U");
                }
            }
            hasher.update(b";");
        }
        hasher.update(b"]\nplan_touches[");
        for (sha, touches) in &self.plan_touches {
            hasher.update(sha.as_str().as_bytes());
            hasher.update(b"=");
            for (key, kind) in touches {
                hasher.update(key.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(kind.as_str().as_bytes());
                hasher.update(b",");
            }
            hasher.update(b";");
        }
        hasher.update(b"]\nconflicts[");
        for (key, paths) in &self.plan_conflicts {
            hasher.update(key.as_str().as_bytes());
            hasher.update(b"=");
            for path in paths {
                hasher.update(path.to_string_lossy().as_bytes());
                hasher.update(b",");
            }
            hasher.update(b";");
        }
        hasher.update(b"]");

        StateDigest(hasher.finalize().to_hex().to_string())
    }

    /// Resolve a caller-supplied plan path against the repo's current
    /// state. Phase 1 of the plan-path-identity migration: see
    /// `.trinity/plans/plan-path-identity.md` §2 for the rules. Internal
    /// callers go through this so all surfaces (MCP, HTTP, watcher)
    /// classify "same stem different path" identically.
    pub fn resolve_plan<'a>(&'a self, requested: &PlanPath) -> Result<&'a Plan, PlanLookupError> {
        let key = PlanKey::from_path(requested.as_path())
            .ok_or_else(|| PlanLookupError::InvalidPlanPath(requested.clone()))?;
        if let Some(paths) = self.plan_conflicts.get(&key) {
            return Err(PlanLookupError::PlanConflict {
                key,
                paths: paths.clone(),
            });
        }
        let plan = self
            .plans
            .get(&key)
            .ok_or_else(|| PlanLookupError::UnknownPlan(key.clone()))?;
        if &plan.plan_path == requested {
            return Ok(plan);
        }
        if let Some(counterpart) = requested.counterpart()
            && counterpart == plan.plan_path
        {
            return Ok(plan);
        }
        Err(PlanLookupError::PlanPathMismatch {
            current: plan.plan_path.clone(),
            requested: requested.clone(),
        })
    }
}

/// Why a caller-supplied `plan_path` could not be resolved against the
/// runtime's `plans` map. Surfaced verbatim by MCP / HTTP error responses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanLookupError {
    #[error("invalid plan path: {0}")]
    InvalidPlanPath(PlanPath),
    #[error("unknown plan: {0}")]
    UnknownPlan(PlanKey),
    #[error("plan path mismatch: requested {requested} but the current plan path is {current}")]
    PlanPathMismatch {
        current: PlanPath,
        requested: PlanPath,
    },
    #[error("conflict: stem `{key}` maps to multiple files on disk")]
    PlanConflict { key: PlanKey, paths: Vec<PlanPath> },
}

/// One row in the per-session timeline returned by
/// `RepoState::timeline_for`. The renderer translates these to UI rows
/// or MCP context entries; the core just emits them in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineEvent {
    /// A commit attributed to this session. The render layer decides
    /// whether to label it "Plan revision" / "Implementation" / "Mixed"
    /// based on `(plan_touch, has_code_changes)`.
    Commit {
        sha: CommitSha,
        plan_touch: Option<PlanTouchKind>,
        has_code_changes: bool,
    },
    /// A reviewer's verdict against a specific commit. Always follows
    /// the `Commit` it targets in the timeline.
    Review {
        phase: TimelinePhase,
        target: CommitSha,
        author: AgentLabel,
        verdict: Verdict,
    },
    /// A flat-drop feedback file that hasn't been canonicalized to a
    /// target SHA yet. Shown at the end of the timeline.
    HeldFeedback {
        author: AgentLabel,
        reason: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimelinePhase {
    Plan,
    Impl,
}

impl TimelinePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            TimelinePhase::Plan => "plan",
            TimelinePhase::Impl => "impl",
        }
    }
}

/// Stable hash of a `RepoState`. Used by the runtime to skip broadcasts
/// when a rebuild produced byte-identical state.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StateDigest(pub String);

impl StateDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub id: PlanKey,
    /// Repo-relative path: `.trinity/plans/<stem>.md` or
    /// `.trinity/plans/done/<stem>.md`. Never absolute. Internal: never
    /// crosses an API boundary — the boundary uses `PlanId` plus
    /// `state` and a derived `current_path`.
    pub plan_path: PlanPath,
    /// Active vs Done lifecycle state, derived from `plan_path` at
    /// rebuild time. Exposed on the wire as a `state` field; the
    /// `PlanId` itself doesn't change when this flips.
    pub state: PlanState,
    /// Body from HEAD's blob, not the working tree.
    pub body: String,
    pub body_hash: ContentHash,
    /// First commit that added the plan file. Lower bound for
    /// attribution walks.
    pub plan_intro: CommitSha,
    /// First-parent of `plan_intro`, or `None` for the root commit.
    /// Used by `pr_hint` to suggest squash bases.
    pub plan_intro_parent: Option<CommitSha>,
    pub plan_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub impl_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    /// Plan-phase feedback files dropped while `plan_worktree_status` was
    /// `BodyDirty`. Not auto-organized into `<target-sha>/<author>.md` until
    /// the plan revision lands.
    pub held_plan_feedback: Vec<HeldFeedback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanState {
    Active,
    Done,
}

impl PlanState {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanState::Active => "active",
            PlanState::Done => "done",
        }
    }

    pub fn from_plan_path(p: &PlanPath) -> Self {
        if p.is_done() {
            PlanState::Done
        } else {
            PlanState::Active
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    /// Absolute path on disk for the feedback file.
    pub path: PathBuf,
    pub body: String,
    pub verdict: Verdict,
    /// File mtime as unix seconds at the time the feedback was ingested.
    /// Used by the UI to sort feedback chronologically when SHA + author
    /// alone don't establish order.
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Approve,
    RequestChanges,
    Unmarked,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request_changes",
            Verdict::Unmarked => "unmarked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldFeedback {
    pub path: PathBuf,
    pub author: AgentLabel,
    pub body: String,
    /// Machine-readable key for the held reason. Today: `"plan_dirty"`.
    pub reason: &'static str,
    /// File mtime as unix seconds at the time the held entry was created.
    pub created_at: i64,
}

/// Working-tree state of a session's plan file relative to HEAD. **Never
/// stored on `Session`** — always recomputed at read time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWorktreeStatus {
    /// Working tree matches HEAD's plan blob.
    Clean,
    /// File exists at the active path with a different body.
    BodyDirty,
    /// Active path is missing from the working tree but the done counterpart
    /// is present — operator ran `mv` but hasn't committed.
    DoneMovePending,
    /// Active path missing and no done counterpart — operator deleted the
    /// file without doing the proper move.
    MissingActivePlanFile,
}

impl PlanWorktreeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanWorktreeStatus::Clean => "clean",
            PlanWorktreeStatus::BodyDirty => "body_dirty",
            PlanWorktreeStatus::DoneMovePending => "done_move_pending",
            PlanWorktreeStatus::MissingActivePlanFile => "missing_active_plan_file",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Planning,
    Implementing,
    Done,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Planning => "planning",
            Phase::Implementing => "implementing",
            Phase::Done => "done",
        }
    }
}

/// Per-commit attribution result. See `.trinity/plans/filesystem-truth-rewrite.md`
/// "Commit Attribution — pure git walk" for the four classification rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributionResult {
    Attributed {
        session: PlanKey,
        /// Set when this commit itself touched the plan file
        /// (single-plan-touch case). `None` when the commit inherited
        /// attribution from a parent via walk-back.
        plan_touch: Option<PlanTouchKind>,
        /// True if this commit modified any non-`.trinity/` file.
        has_code_changes: bool,
    },
    /// Multi-plan-touch commit, or walk reached root without finding a
    /// single-plan-touch ancestor. Descendants walk through this commit
    /// transparently.
    Unattributed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanTouchKind {
    Intro,
    Revision,
    DoneMove,
}

impl PlanTouchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanTouchKind::Intro => "intro",
            PlanTouchKind::Revision => "revision",
            PlanTouchKind::DoneMove => "done_move",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    /// `None` for repo-level events (`repo_rebuilt`). Plan-scoped events
    /// (`plan_worktree_changed`, `feedback_changed`, `feedback_removed`)
    /// carry the plan's current canonical path.
    pub plan_path: Option<PlanPath>,
    pub kind: &'static str,
    pub payload: serde_json::Value,
}

/// The `waiting_on` projection — the canonical per-session "who blocks
/// progress" signal surfaced in MCP context and the web UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingOn {
    pub role: WaitingRole,
    pub reason: WaitingReason,
    pub agents: Vec<AgentLabel>,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingRole {
    Master,
    Reviewers,
    None,
}

impl WaitingRole {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitingRole::Master => "master",
            WaitingRole::Reviewers => "reviewers",
            WaitingRole::None => "none",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingReason {
    SessionDone,
    CommitDoneMove,
    RestoreOrCommitDoneMove,
    CommitPlanRevision,
    AddressPlanRequestChanges,
    ReadyToImplement,
    PlanNeedsInitialReview,
    PlanNeedsRereview,
    AddressImplRequestChanges,
    ReadyToFinish,
    ImplNeedsInitialReview,
    ImplNeedsRereview,
}

impl WaitingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitingReason::SessionDone => "session_done",
            WaitingReason::CommitDoneMove => "commit_done_move",
            WaitingReason::RestoreOrCommitDoneMove => "restore_or_commit_done_move",
            WaitingReason::CommitPlanRevision => "commit_plan_revision",
            WaitingReason::AddressPlanRequestChanges => "address_plan_request_changes",
            WaitingReason::ReadyToImplement => "ready_to_implement",
            WaitingReason::PlanNeedsInitialReview => "plan_needs_initial_review",
            WaitingReason::PlanNeedsRereview => "plan_needs_rereview",
            WaitingReason::AddressImplRequestChanges => "address_impl_request_changes",
            WaitingReason::ReadyToFinish => "ready_to_finish",
            WaitingReason::ImplNeedsInitialReview => "impl_needs_initial_review",
            WaitingReason::ImplNeedsRereview => "impl_needs_rereview",
        }
    }
}
