//! In-memory state for the filesystem-truth model. All data here is a
//! derived cache from git + working tree; nothing is persisted.
//!
//! See `.trinity/plans/filesystem-truth-rewrite.md` for the architecture.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, RepoBasename};

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
            hasher.update(plan.plan_path.as_bytes());
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

/// Plan, PlanTimelineEvent, Feedback are the daemon's fold-state
/// types — defined once in `trinity_core::model` and shared with
/// the wire. `body_html`-bearing wire shapes (`dto::Feedback`,
/// `dto::CommitGate`) are still built by the response projection
/// step from these model types.
pub use trinity_core::model::{Feedback, Plan, PlanTimelineEvent};

pub use trinity_core::PlanLifecycle;

pub use trinity_core::api::ArchivedCycle;

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
    pub payload: trinity_core::api::RepoEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    pub plan_id: crate::lifecycle::PlanId,
    pub lifecycle: PlanLifecycle,
    pub payload: trinity_core::api::PlanEventPayload,
}

/// The `waiting_on` projection — the canonical per-session "who blocks
/// progress" signal surfaced in MCP context and the web UI. Now
/// shared with the wire crate so daemon-side projection state and
/// wire response shape are one definition.
pub use trinity_core::api::WaitingOn;
pub use trinity_core::{WaitingReason, WaitingRole};
