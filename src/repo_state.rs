//! In-memory state for the filesystem-truth model. All data here is a
//! derived cache from git + working tree; nothing is persisted.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` for the architecture.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanKey, RepoBasename};
use crate::review_state::CommitGate;

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

    /// Chronological timeline rows for one plan's activity. The renderer
    /// (web UI, MCP responses) walks the returned rows to display events
    /// in order. Each `PlanTimelineEvent` in `plan.timeline` becomes one
    /// `Commit` row followed by `Review` rows for its gate feedback.
    ///
    /// Returns an empty vec if the plan is unknown.
    pub fn timeline_for(&self, plan_key: &PlanKey) -> Vec<TimelineEvent> {
        let Some(plan) = self.plans.get(plan_key) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(plan.timeline.len() * 2);
        for event in &plan.timeline {
            let plan_touch = match event.kind {
                CommitKind::PlanOnly | CommitKind::Mixed | CommitKind::MultiPlan => {
                    if event.sha == plan.plan_intro {
                        Some(PlanTouchKind::Intro)
                    } else {
                        Some(PlanTouchKind::Revision)
                    }
                }
                CommitKind::CodeOnly | CommitKind::Finalize | CommitKind::Unattributed => None,
            };
            out.push(TimelineEvent::Commit {
                sha: event.sha.clone(),
                kind: event.kind,
                plan_touch,
                subject: event.subject.clone(),
            });
            if let Some(gate) = &event.gate {
                for (author, fb) in &gate.feedback {
                    out.push(TimelineEvent::Review {
                        target: event.sha.clone(),
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
            hasher.update(b"|frozen=");
            hasher.update(
                plan.frozen_at()
                    .map(|s| s.as_str())
                    .unwrap_or("")
                    .as_bytes(),
            );
            hasher.update(b"|timeline=[");
            for event in &plan.timeline {
                hasher.update(event.sha.as_str().as_bytes());
                hasher.update(b":");
                hasher.update(event.kind.as_str().as_bytes());
                hasher.update(b":");
                if let Some(gate) = &event.gate {
                    hasher.update(gate.state.as_str().as_bytes());
                    hasher.update(b":");
                    for (author, fb) in &gate.feedback {
                        hasher.update(author.as_str().as_bytes());
                        hasher.update(b"=");
                        hasher.update(fb.verdict.as_str().as_bytes());
                        hasher.update(b",");
                    }
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
    /// A commit on this plan's timeline. `kind` is the per-plan
    /// classification copied from the underlying `PlanTimelineEvent`;
    /// renderers exhaustive-match on it to emit the wire kind string
    /// (`commit_plan` / `commit_impl` / `commit_mixed` /
    /// `commit_finalize`). `plan_touch` distinguishes intro vs
    /// revision; `subject` is the commit's first line.
    Commit {
        sha: CommitSha,
        kind: CommitKind,
        plan_touch: Option<PlanTouchKind>,
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
    /// `max(author_ts of attributed commits, mtime of feedback
    /// files)`. Powers the /api/plans sort order. Updated
    /// incrementally — never a max-walk.
    pub last_activity_ts: i64,
    /// Chronological per-commit log of this plan's life — appended
    /// by `disk_snapshot::apply_commit` as the fold sees each
    /// relevant commit. The single source of truth for plan
    /// revisions, implementation commits, reviewable commits,
    /// per-commit gates, and per-commit metadata. All projection
    /// queries (`projection::*`) are filters or reverse-scans over
    /// this list — no parallel buckets to keep in sync.
    pub timeline: Vec<PlanTimelineEvent>,
    /// Per-cycle summaries derived from the fold's freeze events (one
    /// entry per freeze). Today the monotone rule means this has
    /// length 0 or 1. Surfaced in the plan-detail wire as
    /// `archived_cycles`; non-freeze touches of the snapshot path
    /// (deletions, phantom re-finalizes, hand edits) are intentionally
    /// excluded — see plan §Archived cycle.
    pub archived_cycles: Vec<ArchivedCycleSummary>,
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

    /// The freeze commit's SHA, if this plan has frozen. Derived from
    /// the timeline — the last `CommitKind::Finalize` event is the
    /// freeze. `Some(_)` ⇒ plan is finished. Once set the fold never
    /// appends further events on this plan (step 5 short-circuits on
    /// frozen plans), so this scan is at-most-once-per-plan-life.
    pub fn frozen_at(&self) -> Option<&CommitSha> {
        self.timeline
            .iter()
            .rev()
            .find_map(|e| matches!(e.kind, CommitKind::Finalize).then_some(&e.sha))
    }

    /// True iff the plan has frozen. Sugar for `frozen_at().is_some()`.
    pub fn is_frozen(&self) -> bool {
        self.frozen_at().is_some()
    }

    /// Lifecycle derived from `is_frozen()`. Replaces the legacy
    /// `PlanLifecycle::from_plan(&Plan)` constructor — now that
    /// `PlanLifecycle` lives in `trinity_core`, the daemon owns
    /// this projection as a method on the daemon's own struct
    /// (Rust's orphan rule forbids inherent impl blocks on
    /// externally-defined enums).
    pub fn lifecycle(&self) -> PlanLifecycle {
        if self.is_frozen() {
            PlanLifecycle::Finished
        } else {
            PlanLifecycle::Active
        }
    }

    /// True iff this plan should surface across Trinity's response
    /// shapes given the current `worktree_status`. A plan is HIDDEN
    /// (returns false) when its file is missing from the working
    /// tree AND it has not frozen: the operator has uncommitted-
    /// deleted it, so Trinity respects that decision until they
    /// either restore the file or commit the deletion. Frozen plans
    /// stay visible regardless — their body is captured at freeze
    /// and `.trinity/finished/` is sealed.
    pub fn is_visible(&self, worktree_status: PlanWorktreeStatus) -> bool {
        self.is_frozen() || !matches!(worktree_status, PlanWorktreeStatus::PlanFileMissing)
    }
}

/// One commit in a plan's life as observed by the fold. The `kind` is
/// the per-plan classification (`PlanOnly | CodeOnly | Mixed | MultiPlan
/// | Finalize` — `Unattributed` is never appended because such commits
/// are not part of this plan's timeline). `gate` is `Some` for
/// reviewable kinds (`PlanOnly | CodeOnly | Mixed`) and `None` for
/// non-reviewable kinds (`MultiPlan`, `Finalize`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTimelineEvent {
    pub sha: CommitSha,
    pub kind: CommitKind,
    pub author_ts: i64,
    pub subject: String,
    pub gate: Option<CommitGate>,
}

/// Plan lifecycle: re-exported from `trinity_core` so the daemon
/// and the frontend branch on one definition. Derived from
/// `Plan::is_frozen()`; see [`Plan::lifecycle`] for the daemon-side
/// constructor.
pub use trinity_core::PlanLifecycle;

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

pub use trinity_core::Verdict;

pub use trinity_core::{PlanWorktreeStatus, Posture};

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

pub use trinity_core::{CommitKind, PlanTouchKind};

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
            LiveEvent::Repo(e) => e.payload.kind_str(),
            LiveEvent::Plan(e) => e.payload.kind_str(),
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
    pub payload: trinity_core::dto::RepoEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    pub plan_id: crate::lifecycle::PlanId,
    pub lifecycle: PlanLifecycle,
    pub payload: trinity_core::dto::PlanEventPayload,
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

pub use trinity_core::{WaitingReason, WaitingRole};
