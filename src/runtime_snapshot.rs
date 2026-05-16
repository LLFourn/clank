//! Owned runtime snapshots for request handlers.
//!
//! These types are cloned while holding the runtime mutex, then used
//! after the mutex is released. They intentionally mirror the in-memory
//! `RepoState` shape closely so projection code can stay pure.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::disk_snapshot::CommitMetaEntry;
use crate::lifecycle::{CommitSha, ContentHash, PlanKey};
use crate::repo_state::{AttributionResult, Plan, PlanTouchKind, RepoState};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSnapshot {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub plans: Vec<PlanSnapshot>,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
    pub plan_conflicts: BTreeMap<PlanKey, Vec<PathBuf>>,
    pub commit_meta: BTreeMap<CommitSha, CommitMetaEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSnapshot {
    pub id: PlanKey,
    pub plan_path: PathBuf,
    pub state: crate::repo_state::PlanState,
    pub body: String,
    pub body_hash: ContentHash,
    pub plan_intro: CommitSha,
    pub plan_intro_parent: Option<CommitSha>,
    pub commits: BTreeMap<CommitSha, crate::review_state::CommitGate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSnapshotBundle {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub plan: PlanSnapshot,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
    pub commit_meta: BTreeMap<CommitSha, CommitMetaEntry>,
}

impl RepoSnapshot {
    pub fn from_state(state: &RepoState) -> Self {
        Self {
            root: state.root.clone(),
            head: state.head.clone(),
            plans: state.plans.values().map(PlanSnapshot::from_plan).collect(),
            attribution: state.attribution.clone(),
            plan_touches: state.plan_touches.clone(),
            commit_order: state.commit_order.clone(),
            plan_conflicts: state.plan_conflicts.clone(),
            commit_meta: state.commit_meta.clone(),
        }
    }

    pub fn to_repo_state(&self) -> RepoState {
        RepoState {
            root: self.root.clone(),
            plans: self
                .plans
                .iter()
                .map(|plan| (plan.id.clone(), plan.to_plan()))
                .collect(),
            head: self.head.clone(),
            attribution: self.attribution.clone(),
            plan_touches: self.plan_touches.clone(),
            commit_order: self.commit_order.clone(),
            plan_conflicts: self.plan_conflicts.clone(),
            commit_meta: self.commit_meta.clone(),
        }
    }
}

impl PlanSnapshot {
    pub fn from_plan(plan: &Plan) -> Self {
        Self {
            id: plan.id.clone(),
            plan_path: plan.plan_path.clone(),
            state: plan.state,
            body: plan.body.clone(),
            body_hash: plan.body_hash.clone(),
            plan_intro: plan.plan_intro.clone(),
            plan_intro_parent: plan.plan_intro_parent.clone(),
            commits: plan.commits.clone(),
        }
    }

    pub fn to_plan(&self) -> Plan {
        Plan {
            id: self.id.clone(),
            plan_path: self.plan_path.clone(),
            state: self.state,
            body: self.body.clone(),
            body_hash: self.body_hash.clone(),
            plan_intro: self.plan_intro.clone(),
            plan_intro_parent: self.plan_intro_parent.clone(),
            commits: self.commits.clone(),
        }
    }
}

impl PlanSnapshotBundle {
    pub fn from_state_for(state: &RepoState, plan_key: &PlanKey) -> Option<Self> {
        let plan = state.plans.get(plan_key)?;
        Some(Self {
            root: state.root.clone(),
            head: state.head.clone(),
            plan: PlanSnapshot::from_plan(plan),
            attribution: state.attribution.clone(),
            plan_touches: state.plan_touches.clone(),
            commit_order: state.commit_order.clone(),
            commit_meta: state.commit_meta.clone(),
        })
    }

    /// Synthesize a `RepoState` containing just this bundle's plan.
    /// Callers should only invoke this when they need projections that
    /// take `&RepoState` and they have already established the plan is
    /// non-conflicting (e.g. `mcp_response`'s `get_context_response`).
    ///
    /// `plan_conflicts` is deliberately empty — conflict detection lives
    /// upstream of single-plan bundles, so any consumer of `to_repo_state`
    /// that reads `plan_conflicts` would mistake "we only carry one plan"
    /// for "no plan is in conflict." If you need conflict-aware logic,
    /// call it against `RepoSnapshot` (the full repo) instead.
    pub fn to_repo_state(&self) -> RepoState {
        RepoState {
            root: self.root.clone(),
            plans: [(self.plan.id.clone(), self.plan.to_plan())]
                .into_iter()
                .collect(),
            head: self.head.clone(),
            attribution: self.attribution.clone(),
            plan_touches: self.plan_touches.clone(),
            commit_order: self.commit_order.clone(),
            plan_conflicts: BTreeMap::new(),
            commit_meta: self.commit_meta.clone(),
        }
    }
}
