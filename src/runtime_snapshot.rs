//! Owned runtime snapshots for request handlers.
//!
//! These types are cloned while holding the runtime mutex, then used
//! after the mutex is released. They intentionally mirror the in-memory
//! `RepoState` shape closely so projection code can stay pure.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, PlanKey, PlanPath};
use crate::repo_state::{
    AttributionResult, Feedback, HeldFeedback, Plan, PlanTouchKind, RepoState,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSnapshot {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub plans: Vec<PlanSnapshot>,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
    pub plan_conflicts: BTreeMap<PlanKey, Vec<PlanPath>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSnapshot {
    pub id: PlanKey,
    pub plan_path: PlanPath,
    pub body: String,
    pub body_hash: ContentHash,
    pub plan_intro: CommitSha,
    pub plan_intro_parent: Option<CommitSha>,
    pub plan_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub impl_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub held_plan_feedback: Vec<HeldFeedback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSnapshotBundle {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub plan: PlanSnapshot,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(PlanKey, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
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
        }
    }
}

impl PlanSnapshot {
    pub fn from_plan(plan: &Plan) -> Self {
        Self {
            id: plan.id.clone(),
            plan_path: plan.plan_path.clone(),
            body: plan.body.clone(),
            body_hash: plan.body_hash.clone(),
            plan_intro: plan.plan_intro.clone(),
            plan_intro_parent: plan.plan_intro_parent.clone(),
            plan_feedback: plan.plan_feedback.clone(),
            impl_feedback: plan.impl_feedback.clone(),
            held_plan_feedback: plan.held_plan_feedback.clone(),
        }
    }

    pub fn to_plan(&self) -> Plan {
        Plan {
            id: self.id.clone(),
            plan_path: self.plan_path.clone(),
            body: self.body.clone(),
            body_hash: self.body_hash.clone(),
            plan_intro: self.plan_intro.clone(),
            plan_intro_parent: self.plan_intro_parent.clone(),
            plan_feedback: self.plan_feedback.clone(),
            impl_feedback: self.impl_feedback.clone(),
            held_plan_feedback: self.held_plan_feedback.clone(),
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
        })
    }

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
        }
    }
}
