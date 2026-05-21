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
    /// Repo-wide commit nodes indexed by SHA. The authoritative
    /// per-commit data for the commit-first model — each
    /// `CommitNode` carries kind, attribution, plan set, and the
    /// `Option<CommitGate>` that drives review state. Iteration
    /// order is SHA-keyed and must NOT be relied on for chronology;
    /// use [`commit_order`](Self::commit_order) for that.
    pub commits: BTreeMap<CommitSha, CommitNode>,
    /// The fold's first-parent commit order, oldest-first. The
    /// commit-first matcher walks this directly instead of
    /// reconstructing chronology from `CommitNode.author_ts`,
    /// which is unreliable (timestamps can be edited, ties exist).
    /// Always populated by `disk_snapshot::apply_commit` and
    /// survives the cache.
    pub commit_order: Vec<CommitSha>,
    pub head: Option<CommitSha>,
    /// New sans-io fold state per `core-state-rewrite.md` Phase 1.
    /// Populated alongside the legacy fold by
    /// `disk_snapshot::apply_commit`. Subsequent commits migrate
    /// consumers off the legacy fields above onto this one; eventually
    /// the legacy fields are deleted and this becomes the only state.
    pub fold: trinity_core::repo_state::RepoState,
}

impl RepoState {
    /// Clone this state with `plans` filtered to a single plan.
    /// Returns `None` if the plan key is absent. Used by
    /// `Runtime::snapshot_session` to hand response builders a
    /// `RepoState` for one specific plan without paying for every
    /// other plan's clone.
    pub fn single_plan(&self, key: &PlanKey) -> Option<RepoState> {
        let plan = self.plans.get(key)?;
        let commits: BTreeMap<CommitSha, CommitNode> = self
            .commits
            .iter()
            .filter(|(_, node)| node.plans.contains(key))
            .map(|(sha, node)| (sha.clone(), node.clone()))
            .collect();
        let commit_order = self
            .commit_order
            .iter()
            .filter(|sha| commits.contains_key(*sha))
            .cloned()
            .collect();
        // Project the new fold to just this plan's active state.
        let mut fold = trinity_core::repo_state::RepoState::default();
        if let Some(ps) = self.fold.plans.get(key) {
            fold.plans.insert(key.clone(), ps.clone());
        }
        for fp in &self.fold.finished_plans {
            if &fp.plan == key {
                fold.finished_plans.push(fp.clone());
            }
        }
        for w in &self.fold.warnings {
            if w.plan.as_ref() == Some(key)
                || trinity_core::repo_state::warning_mentions_plan(&w.warning, key)
            {
                fold.warnings.push(w.clone());
            }
        }
        if self.fold.active_plan_hint.as_ref() == Some(key) {
            fold.active_plan_hint = Some(key.clone());
        }
        Some(RepoState {
            root: self.root.clone(),
            plans: [(key.clone(), plan.clone())].into_iter().collect(),
            commits,
            commit_order,
            head: self.head.clone(),
            fold,
        })
    }

    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            plans: BTreeMap::new(),
            commits: BTreeMap::new(),
            commit_order: Vec::new(),
            head: None,
            fold: trinity_core::repo_state::RepoState::default(),
        }
    }

    /// Borrow the per-commit gate for `sha`, if the commit exists in
    /// this state's commit map and is a reviewable variant whose gate
    /// has been computed. Phase 2 of `commit-first-review-model`
    /// shifts the gate's home from `PlanTimelineEvent` to
    /// `CommitNode`; callers use this helper instead of the now-
    /// removed `event.gate()` accessor.
    pub fn gate_for(&self, sha: &CommitSha) -> Option<&crate::review_state::CommitGate> {
        self.commits.get(sha).and_then(|n| n.gate.as_ref())
    }

    /// Mutable counterpart to [`gate_for`]. Used by the live-feedback
    /// overlay and the watcher's incremental upsert/remove path.
    pub fn gate_for_mut(
        &mut self,
        sha: &CommitSha,
    ) -> Option<&mut crate::review_state::CommitGate> {
        self.commits.get_mut(sha).and_then(|n| n.gate.as_mut())
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
                if let Some(gate) = self.gate_for(event.sha()) {
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
        hasher.update(b"]\ncommit_order[");
        for sha in &self.commit_order {
            hasher.update(sha.as_str().as_bytes());
            hasher.update(b",");
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
pub use trinity_core::model::{CommitAttribution, CommitNode, Feedback, Plan, PlanTimelineEvent};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::CommitSha;

    /// Codex on 62cd0e5: `commit_order` affects observable behavior
    /// (repo-scope WFW iterates it), so the state digest must see
    /// it. Two states with identical plans + commits but different
    /// commit_order MUST hash differently — otherwise the runtime
    /// can skip a broadcast that should fire.
    #[test]
    fn digest_distinguishes_commit_order() {
        let sha_a = CommitSha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let sha_b = CommitSha::parse("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let mut s1 = RepoState::empty(PathBuf::from("/r"));
        s1.commit_order = vec![sha_a.clone(), sha_b.clone()];
        let mut s2 = RepoState::empty(PathBuf::from("/r"));
        s2.commit_order = vec![sha_b, sha_a];
        assert_ne!(
            s1.digest(),
            s2.digest(),
            "commit_order reorder must produce a different digest"
        );
    }
}
