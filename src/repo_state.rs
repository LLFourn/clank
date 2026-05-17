//! In-memory state for the filesystem-truth model. All data here is a
//! derived cache from git + working tree; nothing is persisted.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` for the architecture.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use crate::lifecycle::{
    AgentLabel, CommitSha, ContentHash, PlanKey, RepoBasename,
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

/// Repo-level reduced state. Everything here is the *result* of
/// applying a series of commits in order — nothing in this struct is
/// a list of commits to be searched later. Per-commit / per-plan
/// information lives on [`Plan`].
///
/// The fold's only public entry point is `apply_commit(state, event)`
/// (see `disk_snapshot::apply_commit`); `derive_state` is just a
/// loop over `apply_commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub root: PathBuf,
    pub plans: BTreeMap<PlanKey, Plan>,
    pub head: Option<CommitSha>,
    /// Plans whose disk state is contradictory at rebuild time. Today
    /// effectively unused (path-parser rejects `.trinity/plans/done/`),
    /// but kept on the type for future stem-collision surfacing.
    pub plan_conflicts: BTreeMap<PlanKey, Vec<PathBuf>>,
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
            plan_conflicts: std::collections::BTreeMap::new(),
        })
    }

    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            plans: BTreeMap::new(),
            head: None,
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
        // Chronological merge of plan_revisions (touch commits) and
        // implementation_commits (code commits). Both lists are
        // sorted in the order the fold appended them, so a two-
        // pointer merge produces the chronological union with one
        // entry per commit.
        let mut out = Vec::new();
        let mut revs = plan.plan_revisions.iter().peekable();
        let mut impls = plan.implementation_commits.iter().peekable();
        loop {
            // Take the next chronological SHA. Mixed commits appear in
            // BOTH lists at the same fold step, so advance both.
            let sha = match (revs.peek().copied(), impls.peek().copied()) {
                (None, None) => break,
                (Some(s), None) => {
                    revs.next();
                    s
                }
                (None, Some(s)) => {
                    impls.next();
                    s
                }
                (Some(a), Some(b)) if a == b => {
                    revs.next();
                    impls.next();
                    a
                }
                (Some(a), Some(_)) => {
                    // Distinct SHAs at the heads: since both lists are
                    // append-only in fold order, the lower-index one
                    // came first. We don't store indices; instead use
                    // the fact that the next chronological commit is
                    // either at revs or impls. Probe via reviewable
                    // list (which has every reviewable commit). Fall
                    // back to revs.
                    revs.next();
                    a
                }
            };
            let plan_touch = plan
                .plan_revisions
                .iter()
                .position(|s| s == sha)
                .and(Some(PlanTouchKind::Revision));
            // `Intro` vs `Revision` distinction: plan_intro is the
            // intro commit; any other touch is a revision.
            let plan_touch = if sha == &plan.plan_intro {
                Some(PlanTouchKind::Intro)
            } else {
                plan_touch
            };
            let has_code_changes = plan.implementation_commits.iter().any(|s| s == sha);
            let subject = plan
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
            hasher.update(b"|revs=[");
            for sha in &plan.plan_revisions {
                hasher.update(sha.as_str().as_bytes());
                hasher.update(b",");
            }
            hasher.update(b"]|impls=[");
            for sha in &plan.implementation_commits {
                hasher.update(sha.as_str().as_bytes());
                hasher.update(b",");
            }
            hasher.update(b"]|frozen=");
            hasher.update(
                plan.frozen_at
                    .as_ref()
                    .map(|s| s.as_str())
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
    /// Repo-relative path: `.trinity/plans/<stem>.md`. Never absolute.
    /// Internal: never crosses an API boundary — the boundary uses
    /// `PlanId` plus a derived `current_path`.
    pub plan_path: PathBuf,
    /// Plan-file body. For unfrozen plans, this tracks HEAD. For
    /// frozen plans, this is the body at the freeze commit.
    pub body: String,
    pub body_hash: ContentHash,
    /// First commit that introduced the plan file (in this plan
    /// instance's life — if a same-stem plan was deleted and
    /// re-introduced, `plan_intro` is the re-introduction commit).
    pub plan_intro: CommitSha,
    /// First-parent of `plan_intro`, or `None` for the root commit.
    /// Used by `pr_hint` to suggest squash bases.
    pub plan_intro_parent: Option<CommitSha>,
    /// Chronological list of commits where this plan was touched
    /// (`Intro` or `Revision`), including MultiPlan commits where
    /// this plan was one of several touched. Accumulated by the
    /// fold; no scanning required for projection queries.
    pub plan_revisions: Vec<CommitSha>,
    /// Chronological list of attributed implementation commits
    /// (`has_code_changes && session == this plan`).
    pub implementation_commits: Vec<CommitSha>,
    /// Chronological list of reviewable commits (CommitKind
    /// `PlanOnly | CodeOnly | Mixed` for this plan). Subset of
    /// `plan_revisions ∪ implementation_commits`. `MultiPlan` /
    /// `Unattributed` are excluded.
    pub reviewable_commits: Vec<CommitSha>,
    /// Author timestamp + first-line subject for each commit this
    /// plan accumulated (`plan_revisions ∪ implementation_commits ∪
    /// {plan_intro}`). Lookups O(log n) for timeline rendering.
    pub commit_meta: BTreeMap<CommitSha, CommitMetaPlan>,
    /// `max(author_ts of attributed commits, mtime of feedback
    /// files)`. Powers the /api/plans sort order. Updated
    /// incrementally — never a max-walk.
    pub last_activity_ts: i64,
    /// Cached latest reviewable commit. Set whenever the fold builds
    /// a gate entry for a reviewable commit. None if the plan has
    /// no reviewable commits yet.
    pub latest_reviewable_commit: Option<CommitSha>,
    /// Per-commit gate state under the commit-centric model.
    /// Authoritative as of phase 2.3 — all feedback bodies live
    /// here under `commits[sha].feedback`. Keys are exactly the
    /// entries in `reviewable_commits`.
    pub commits: BTreeMap<CommitSha, crate::review_state::CommitGate>,
    /// First commit (chronological) whose tree satisfied the finalize
    /// rule (plan file present + `.trinity/finished/<stem>/` with ≥1
    /// file all starting with `APPROVE`). `Some(_)` ⇒ the plan is
    /// finished. Set monotonically by the sans-IO fold; never cleared.
    pub frozen_at: Option<CommitSha>,
    /// Commits at which this plan transitioned to frozen, in
    /// chronological order. Equivalent to "the chain of finalize
    /// events." The last entry == `frozen_at` (when set); earlier
    /// entries can only appear if a future implementation supports
    /// reopening a frozen plan via history rewrite mid-fold — which
    /// today never happens, so the list has length 0 or 1.
    pub freeze_events: Vec<CommitSha>,
    /// Per-cycle summaries derived from the fold's freeze events (one
    /// entry per freeze). Today the monotone rule means this has
    /// length 0 or 1. Surfaced in the plan-detail wire as
    /// `archived_cycles`; non-freeze touches of the snapshot path
    /// (deletions, phantom re-finalizes, hand edits) are intentionally
    /// excluded — see plan §Archived cycle.
    pub archived_cycles: Vec<ArchivedCycleSummary>,
}

/// Per-commit metadata accumulated on each `Plan`. Same shape as the
/// global `CommitMetaEntry` used to be, but scoped per-plan: only
/// the commits this plan cares about get stored, and entries
/// disappear with the plan when it's deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMetaPlan {
    pub author_ts: i64,
    pub subject: String,
}

/// Plan lifecycle as projected from the event-log fold. Replaces the
/// legacy `PlanState` (which mirrored the `done/` directory move).
/// Derived from `Plan.frozen_at` — never stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLifecycle {
    Active,
    Finished,
}

impl PlanLifecycle {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanLifecycle::Active => "active",
            PlanLifecycle::Finished => "finished",
        }
    }

    pub fn from_plan(plan: &Plan) -> Self {
        if plan.frozen_at.is_some() {
            PlanLifecycle::Finished
        } else {
            PlanLifecycle::Active
        }
    }
}

/// Per-cycle summary surfaced in the plan-detail UI's cycle-history
/// view. Sourced from the fold's `freeze_events` side-output; the
/// last entry of `freeze_events` is the current cycle, earlier
/// entries are archived. Today the rule is monotone so the list has
/// length 0 or 1; the shape scales to richer histories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivedCycleSummary {
    /// The commit at which the cycle was closed (the freeze event).
    pub closer: CommitSha,
    /// Number of approving-reviewer files in
    /// `.trinity/finished/<stem>/` at that freeze commit's tree.
    pub approver_count: u32,
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

/// Working-tree state of a plan's plan file relative to HEAD. **Never
/// stored on `Plan`** — always recomputed at read time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanWorktreeStatus {
    /// Working tree matches HEAD's plan blob.
    Clean,
    /// File exists at the active path with a different body.
    BodyDirty,
    /// Plan file is missing from the working tree but still present in
    /// HEAD — operator made an uncommitted deletion. Action: restore
    /// it from HEAD or commit the deletion.
    PlanFileMissing,
}

impl PlanWorktreeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanWorktreeStatus::Clean => "clean",
            PlanWorktreeStatus::BodyDirty => "body_dirty",
            PlanWorktreeStatus::PlanFileMissing => "plan_file_missing",
        }
    }
}

/// Current activity posture for a plan, derived from the latest
/// reviewable commit's `CommitKind`. `PlanOnly | Mixed → Planning`,
/// `CodeOnly → Implementing`. Surfaced on the wire as `phase` (legacy
/// name) for one release. Computed by `projection::current_posture`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posture {
    Planning,
    Implementing,
}

impl Posture {
    pub fn as_str(self) -> &'static str {
        match self {
            Posture::Planning => "planning",
            Posture::Implementing => "implementing",
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
}

impl PlanTouchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanTouchKind::Intro => "intro",
            PlanTouchKind::Revision => "revision",
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
            CommitKind::MultiPlan => "multi_plan",
            CommitKind::Unattributed => "unattributed",
        }
    }

    /// Reviewable commits get a `CommitGate` entry in `Plan.commits`.
    /// `MultiPlan` and `Unattributed` are intentionally excluded.
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
    pub lifecycle: PlanLifecycle,
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
    /// Plan is frozen (`Plan.frozen_at` is `Some`). Nothing to do for
    /// either role; the plan is sealed.
    SessionFinished,
    /// Plan file is missing from the working tree but still present in
    /// HEAD. The master should restore it from HEAD or commit the
    /// deletion.
    RestoreOrCommitPlanFile,
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
            WaitingReason::SessionFinished => "session_finished",
            WaitingReason::RestoreOrCommitPlanFile => "restore_or_commit_plan_file",
            WaitingReason::CommitPlanRevision => "commit_plan_revision",
            WaitingReason::AddressCommitChanges => "address_commit_changes",
            WaitingReason::ReadyToStartImplementation => "ready_to_start_implementation",
            WaitingReason::CommitNeedsReview => "commit_needs_review",
        }
    }
}
