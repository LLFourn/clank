//! Daemon-side in-memory repo state. Wraps the sans-io fold
//! ([`clank_core::repo_state::RepoState`]) with daemon-only
//! runtime identity (`root`, `head`) and the multi-repo `Clank`
//! container.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use crate::lifecycle::{AgentLabel, CommitSha, PlanKey, RepoBasename};

pub use clank_core::PlanLifecycle;
pub use clank_core::Verdict;
pub use clank_core::api::WaitingOn;
pub use clank_core::{PlanWorktreeStatus, Posture};
pub use clank_core::{WaitingReason, WaitingRole};

pub type RepoRoot = PathBuf;

#[derive(Debug, Default)]
pub struct Clank {
    pub repos: BTreeMap<RepoRoot, RepoState>,
    pub repo_basenames: BTreeMap<RepoBasename, RepoRoot>,
    pub live_events: VecDeque<LiveEvent>,
    pub active_selections: BTreeMap<(RepoBasename, AgentLabel), PlanKey>,
    pub opportunistic_bodies:
        BTreeMap<(RepoRoot, AgentLabel, String), crate::lifecycle::ContentHash>,
    pub seen_stale_reviews: std::collections::BTreeSet<(RepoRoot, AgentLabel, String)>,
}

/// Daemon-side repo state. `fold` is the canonical sans-io fold
/// output; `root` and `head` are runtime identity injected at load
/// time (cache files don't carry root; the header carries head).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoState {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub fold: clank_core::repo_state::RepoState,
}

impl RepoState {
    pub fn empty(root: PathBuf) -> Self {
        Self {
            root,
            head: None,
            fold: clank_core::repo_state::RepoState::default(),
        }
    }

    /// Stable digest over every meaningful field.
    pub fn digest(&self) -> StateDigest {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"clank-state-v3\n");
        hasher.update(self.root.to_string_lossy().as_bytes());
        hasher.update(b"\nhead=");
        hasher.update(
            self.head
                .as_ref()
                .map(|h| h.as_str())
                .unwrap_or("")
                .as_bytes(),
        );
        hasher.update(b"\nfold.hint=");
        hasher.update(
            self.fold
                .active_plan_hint
                .as_ref()
                .map(|k| k.as_str())
                .unwrap_or("")
                .as_bytes(),
        );
        hasher.update(b"\nfold.plans[");
        for (key, ps) in &self.fold.plans {
            hasher.update(key.as_str().as_bytes());
            hasher.update(b":");
            for ev in &ps.commits {
                hasher.update(ev.sha.as_str().as_bytes());
                hasher.update(b",");
            }
            hasher.update(b";");
        }
        hasher.update(b"]\nfold.finished[");
        for fp in &self.fold.finished_plans {
            hasher.update(fp.plan.as_str().as_bytes());
            hasher.update(b":");
            hasher.update(fp.finalized_at.as_str().as_bytes());
            hasher.update(b";");
        }
        hasher.update(b"]\nfold.ad_hoc[");
        for ev in &self.fold.ad_hoc {
            hasher.update(ev.sha.as_str().as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"]\nfold.warnings[");
        for w in &self.fold.warnings {
            hasher.update(w.sha.as_str().as_bytes());
            hasher.update(b":");
            hasher.update(w.plan.as_ref().map(|k| k.as_str()).unwrap_or("").as_bytes());
            hasher.update(b";");
        }
        hasher.update(b"]");
        StateDigest(hasher.finalize().to_hex().to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StateDigest(pub String);

impl StateDigest {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
    pub payload: clank_core::api::RepoEventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEvent {
    pub ts: i64,
    pub repo: RepoRoot,
    pub plan_id: crate::lifecycle::PlanId,
    pub lifecycle: PlanLifecycle,
    pub payload: clank_core::api::PlanEventPayload,
}
