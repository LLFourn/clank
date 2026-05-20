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
    /// Ephemeral per-agent active-plan selections. Populated by
    /// `set_active_work`, cleared by `clear_active_work`, dropped on
    /// daemon restart. Consulted by `resolve_plan_id` before raising
    /// `ambiguous_plan`, validated at use time against the current
    /// fold (selected plan must exist, be non-frozen, and have its
    /// worktree file present).
    pub active_selections: BTreeMap<(RepoBasename, AgentLabel), PlanKey>,
    /// Per-agent opportunistic-body cache. Records "agent X has been
    /// sent path Y at content hash Z" so subsequent `wait_for_work`
    /// polls can omit content the agent already has. Key is
    /// `(canonical repo root, agent, repo-relative path)`; value is
    /// the content hash sent last. Hash-keyed: if the file changes,
    /// the next poll re-sends.
    pub opportunistic_bodies:
        BTreeMap<(RepoRoot, AgentLabel, String), crate::lifecycle::ContentHash>,
    /// Per-agent stale-review one-shot delivery cache. Records "agent
    /// X has been delivered the stale review at path Y" so a stale
    /// review surfaces exactly once per agent. Key is `(canonical
    /// repo root, agent, repo-relative feedback path)`. Path-keyed
    /// (not hash-keyed): stale-review content drifting after delivery
    /// does NOT re-surface the entry — stale reviews are historical
    /// and informational.
    pub seen_stale_reviews: std::collections::BTreeSet<(RepoRoot, AgentLabel, String)>,
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
    /// Repo-wide commit stream indexed by SHA. Phase 1 of
    /// `commit-first-review-model`: populated alongside per-plan
    /// timelines so the commit-first matcher can walk one
    /// chronological stream and the per-plan view becomes a
    /// projection over it. Phase 2 makes `CommitNode.gate`
    /// authoritative; Phase 1 shadows it.
    pub commits: BTreeMap<CommitSha, CommitNode>,
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
        let commits = self
            .commits
            .iter()
            .filter(|(_, node)| node.plans.contains(key))
            .map(|(sha, node)| (sha.clone(), node.clone()))
            .collect();
        Some(RepoState {
            root: self.root.clone(),
            plans: [(key.clone(), plan.clone())].into_iter().collect(),
            commits,
            head: self.head.clone(),
            plan_conflicts: std::collections::BTreeMap::new(),
        })
    }

    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            plans: BTreeMap::new(),
            commits: BTreeMap::new(),
            head: None,
            plan_conflicts: BTreeMap::new(),
        }
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
        hasher.update(b"trinity-state-v2\n");
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
                hasher.update(event.sha().as_str().as_bytes());
                hasher.update(b":");
                hasher.update(event.kind().as_str().as_bytes());
                hasher.update(b":");
                if let Some(gate) = event.gate() {
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
        hasher.update(b"]\ncommits[");
        for (sha, node) in &self.commits {
            hasher.update(sha.as_str().as_bytes());
            hasher.update(b":");
            hasher.update(node.kind.as_str().as_bytes());
            hasher.update(b":");
            hasher.update(commit_attribution_tag(&node.attribution).as_bytes());
            hasher.update(b":plans=");
            for plan in &node.plans {
                hasher.update(plan.as_str().as_bytes());
                hasher.update(b",");
            }
            if let Some(gate) = &node.gate {
                hasher.update(b":gate=");
                hasher.update(gate.state.as_str().as_bytes());
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

/// `RepoState` derived purely from committed git history — no live
/// feedback applied yet. Produced by `disk_snapshot::derive_base_state`,
/// consumed by `disk_snapshot::attach_live_feedback`. Cacheable: the
/// content is a function of HEAD's commit DAG alone, so it can be
/// persisted under `.trinity/cache/repo-state/<head>.v<n>.bin` and
/// reloaded on the next rebuild at the same HEAD.
///
/// The newtype exists to police construction: only `derive_base_state`
/// (and the cache loader, which produces an equivalent shape) builds
/// one, and only `attach_live_feedback` consumes one. Read-only
/// projections still go through `&RepoState` via `Deref`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRepoState(RepoState);

impl BaseRepoState {
    pub fn new(state: RepoState) -> Self {
        Self(state)
    }
    pub fn into_inner(self) -> RepoState {
        self.0
    }
    pub fn as_ref_inner(&self) -> &RepoState {
        &self.0
    }
}

impl std::ops::Deref for BaseRepoState {
    type Target = RepoState;
    fn deref(&self) -> &RepoState {
        &self.0
    }
}

/// `RepoState` after `attach_live_feedback` has folded in
/// working-tree `.trinity/feedback/` files. This is what the rest of
/// the codebase actually projects from. The newtype prevents the
/// cache layer from accidentally serializing a live state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRepoState(RepoState);

impl LiveRepoState {
    pub fn new(state: RepoState) -> Self {
        Self(state)
    }
    pub fn into_inner(self) -> RepoState {
        self.0
    }
}

impl std::ops::Deref for LiveRepoState {
    type Target = RepoState;
    fn deref(&self) -> &RepoState {
        &self.0
    }
}

impl std::ops::DerefMut for LiveRepoState {
    fn deref_mut(&mut self) -> &mut RepoState {
        &mut self.0
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

fn commit_attribution_tag(attr: &CommitAttribution) -> &'static str {
    match attr {
        CommitAttribution::Plan { .. } => "plan",
        CommitAttribution::MultiPlan { .. } => "multi_plan",
        CommitAttribution::AdHoc => "ad_hoc",
        CommitAttribution::Finalize { .. } => "finalize",
    }
}

/// Plan, PlanTimelineEvent, Feedback are the daemon's fold-state
/// types — defined once in `trinity_core::model` and re-exported
/// here AND on `api::*` so daemon storage and wire response are
/// one struct each. Rendered HTML lives in the wasm frontend
/// (`frontend::markdown`), not on these types and not on the wire.
pub use trinity_core::model::{
    ArchivedCycle, CommitAttribution, CommitNode, Feedback, Plan, PlanTimelineEvent,
};

pub use trinity_core::PlanLifecycle;

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
