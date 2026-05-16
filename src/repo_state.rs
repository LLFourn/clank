//! In-memory state for the filesystem-truth model. All data here is a
//! derived cache from git + working tree; nothing is persisted.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` for the architecture.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use crate::lifecycle::{
    AgentLabel, CommitSha, ContentHash, PlanKey, RepoBasename, is_done_plan_path,
};

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
    pub plan_conflicts: BTreeMap<PlanKey, Vec<PathBuf>>,
    /// Author timestamp + first-line subject for every commit in
    /// `commit_order`. Sourced from one batched `git log` per rebuild
    /// (`git_io::first_parent_commits`). Consumed by:
    ///
    /// - timeline rendering (subject column on commit rows).
    /// - `last_activity_ts` per plan (max `author_ts` over the plan's
    ///   attributed commits, vs feedback mtimes).
    ///
    /// Keyed on the same SHAs that appear in `commit_order` /
    /// `attribution`. Lookups are infallible for known commits.
    pub commit_meta: BTreeMap<CommitSha, crate::disk_snapshot::CommitMetaEntry>,
}

impl RepoState {
    /// Clone this state with `plans` filtered to a single plan and
    /// `plan_conflicts` cleared. Returns `None` if the plan key is
    /// absent. Used by `Runtime::snapshot_session` to hand response
    /// builders a `RepoState` for one specific plan without paying for
    /// every other plan's clone.
    pub fn single_plan(&self, key: &PlanKey) -> Option<RepoState> {
        let plan = self.plans.get(key)?;
        Some(RepoState {
            root: self.root.clone(),
            plans: [(key.clone(), plan.clone())].into_iter().collect(),
            head: self.head.clone(),
            attribution: self.attribution.clone(),
            plan_touches: self.plan_touches.clone(),
            commit_order: self.commit_order.clone(),
            plan_conflicts: std::collections::BTreeMap::new(),
            commit_meta: self.commit_meta.clone(),
        })
    }

    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            plans: BTreeMap::new(),
            head: None,
            attribution: BTreeMap::new(),
            plan_touches: BTreeMap::new(),
            commit_order: Vec::new(),
            plan_conflicts: BTreeMap::new(),
            commit_meta: BTreeMap::new(),
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
            let subject = self
                .commit_meta
                .get(sha)
                .map(|m| m.subject.clone())
                .unwrap_or_default();
            out.push(TimelineEvent::Commit {
                sha: sha.clone(),
                plan_touch,
                has_code_changes,
                subject,
            });
            if let Some(gate) = plan.commits.get(sha) {
                for (author, fb) in &gate.feedback {
                    out.push(TimelineEvent::Review {
                        target: sha.clone(),
                        author: author.clone(),
                        verdict: fb.verdict,
                    });
                }
            }
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
            hasher.update(b"|commits=[");
            for (sha, gate) in &plan.commits {
                hasher.update(sha.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(gate.state.as_str().as_bytes());
                hasher.update(b":");
                for (author, fb) in &gate.feedback {
                    hasher.update(author.as_str().as_bytes());
                    hasher.update(b"=");
                    hasher.update(fb.verdict.as_str().as_bytes());
                    hasher.update(b",");
                }
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
}

/// One row in the per-session timeline returned by
/// `RepoState::timeline_for`. The renderer translates these to UI rows
/// or MCP context entries; the core just emits them in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineEvent {
    /// A commit attributed to this session. The render layer decides
    /// whether to label it "Plan revision" / "Implementation" / "Mixed"
    /// based on `(plan_touch, has_code_changes)`. `subject` is the
    /// first line of the commit message — populated from
    /// `RepoState::commit_meta` at projection time.
    Commit {
        sha: CommitSha,
        plan_touch: Option<PlanTouchKind>,
        has_code_changes: bool,
        subject: String,
    },
    /// A reviewer's verdict against a specific commit. Always follows
    /// the `Commit` it targets in the timeline. Renderers that want a
    /// "plan review" vs "impl review" label derive it from the
    /// targeted commit's `CommitKind` via the wire `commits[]` field.
    Review {
        target: CommitSha,
        author: AgentLabel,
        verdict: Verdict,
    },
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
    pub plan_path: PathBuf,
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
    /// Per-commit gate state under the commit-centric model.
    /// Authoritative as of phase 2.3 — all feedback bodies live here
    /// under `commits[sha].feedback`.
    ///
    /// Keys are commits whose `CommitKind` is reviewable for this plan
    /// (`PlanOnly` | `CodeOnly` | `Mixed`). `DoneMove`, `MultiPlan`,
    /// and `Unattributed` commits never get an entry — see
    /// `CommitKind::is_reviewable`.
    pub commits: BTreeMap<CommitSha, crate::review_state::CommitGate>,
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

    pub fn from_plan_path(p: &std::path::Path) -> Self {
        if is_done_plan_path(p) {
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

/// Per-(plan, commit) classification under the commit-centric review
/// model. Derived from `plan_touches` + `attribution`; see
/// `projection::commit_kind_for`. Phase 1 uses this only for unit-test
/// coverage and the additive `Plan.commits` map; phase 2 wires it
/// into projections, MCP responses, and the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitKind {
    /// Touches exactly this plan file (single-plan-touch) and no code.
    PlanOnly,
    /// Touches code attributed to this plan; no plan-file touch on
    /// this commit (attribution inherited via walk-back).
    CodeOnly,
    /// Touches this plan file AND code (single-plan-touch + code).
    Mixed,
    /// Renames this plan file into / out of `.trinity/plans/done/`.
    /// Lifecycle-significant; never gated, never reviewable.
    DoneMove,
    /// Touches two or more distinct plan files on a single commit.
    /// Surfaced in the timeline but never gated.
    MultiPlan,
    /// Commit has no relevance to this plan (no touch, attribution
    /// belongs to another plan, or unattributed).
    Unattributed,
}

impl CommitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CommitKind::PlanOnly => "plan_only",
            CommitKind::CodeOnly => "code_only",
            CommitKind::Mixed => "mixed",
            CommitKind::DoneMove => "done_move",
            CommitKind::MultiPlan => "multi_plan",
            CommitKind::Unattributed => "unattributed",
        }
    }

    /// Reviewable commits get a `CommitGate` entry in `Plan.commits`.
    /// `DoneMove`, `MultiPlan`, and `Unattributed` are intentionally
    /// excluded — see plan §"single done_move/multi_plan invariant".
    pub fn is_reviewable(self) -> bool {
        matches!(
            self,
            CommitKind::PlanOnly | CommitKind::CodeOnly | CommitKind::Mixed
        )
    }
}

/// A single live activity tick from the watcher loop. Tagged enum so a
/// repo-level event (no plan context) is structurally distinct from a
/// plan-scoped event — no `Option<PlanId>` variant tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    Repo(RepoEvent),
    Plan(PlanEvent),
}

impl LiveEvent {
    pub fn kind_str(&self) -> &'static str {
        match self {
            LiveEvent::Repo(e) => e.kind.as_str(),
            LiveEvent::Plan(e) => e.kind.as_str(),
        }
    }

    pub fn plan_id(&self) -> Option<&crate::lifecycle::PlanId> {
        match self {
            LiveEvent::Plan(e) => Some(&e.plan_id),
            LiveEvent::Repo(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    pub kind: RepoEventKind,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoEventKind {
    RepoRebuilt,
    RepoUnwatched,
}

impl RepoEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            RepoEventKind::RepoRebuilt => "repo_rebuilt",
            RepoEventKind::RepoUnwatched => "repo_unwatched",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    pub plan_id: crate::lifecycle::PlanId,
    pub state: PlanState,
    pub kind: PlanEventKind,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanEventKind {
    PlanWorktreeChanged,
    FeedbackChanged,
    FeedbackRemoved,
}

impl PlanEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanEventKind::PlanWorktreeChanged => "plan_worktree_changed",
            PlanEventKind::FeedbackChanged => "feedback_changed",
            PlanEventKind::FeedbackRemoved => "feedback_removed",
        }
    }
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

/// Seven-variant collapsed reason set under the commit-centric model.
/// The pre-2.5 axis was twelve variants (six pairs of plan/impl
/// mirrors); the new shape uses one variant per *action* and lets
/// `description_for` disambiguate prose by `CommitKind` of the
/// latest relevant commit. See plan §"WaitingReason collapse".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitingReason {
    SessionDone,
    CommitDoneMove,
    RestoreOrCommitDoneMove,
    CommitPlanRevision,
    /// "REQUEST_CHANGES on the latest reviewable commit; address it."
    /// Replaces the old AddressPlanRequestChanges + AddressImplRequestChanges
    /// pair. Description prose disambiguates by commit kind.
    AddressCommitChanges,
    /// "Latest reviewable commit is approved; your move." Replaces
    /// ReadyToImplement + ReadyToFinish. After an approved plan_only
    /// the master can start coding, revise further, or move to done;
    /// after an approved code_only/mixed the master can continue,
    /// revise the plan, or move to done. The wire action is
    /// `start_implementation` — historically named `move_forward`
    /// until the verb was found ambiguous between "start the next
    /// commit" and "move the plan to done/".
    ReadyToStartImplementation,
    /// "Latest reviewable commit hasn't been reviewed yet." Replaces
    /// `PlanNeedsInitialReview` + `PlanNeedsRereview` +
    /// `ImplNeedsInitialReview` + `ImplNeedsRereview`. Initial review
    /// vs re-review is a description-prose distinction (do we have
    /// prior participants?), not a state distinction.
    CommitNeedsReview,
}

impl WaitingReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitingReason::SessionDone => "session_done",
            WaitingReason::CommitDoneMove => "commit_done_move",
            WaitingReason::RestoreOrCommitDoneMove => "restore_or_commit_done_move",
            WaitingReason::CommitPlanRevision => "commit_plan_revision",
            WaitingReason::AddressCommitChanges => "address_commit_changes",
            WaitingReason::ReadyToStartImplementation => "ready_to_start_implementation",
            WaitingReason::CommitNeedsReview => "commit_needs_review",
        }
    }
}
