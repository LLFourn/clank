//! Owned runtime snapshots for request handlers.
//!
//! These types are cloned while holding the runtime mutex, then used
//! after the mutex is released. They intentionally mirror the in-memory
//! `RepoState` shape closely so projection code can stay pure.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::lifecycle::{AgentLabel, CommitSha, ContentHash, SessionId};
use crate::repo_state::{
    AttributionResult, Feedback, HeldFeedback, PlanTouchKind, RepoState, Session,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoSnapshot {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub sessions: Vec<SessionSnapshot>,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(SessionId, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub plan_path: PathBuf,
    pub body: String,
    pub body_hash: ContentHash,
    pub plan_intro: CommitSha,
    pub plan_intro_parent: Option<CommitSha>,
    pub plan_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub impl_feedback: BTreeMap<(CommitSha, AgentLabel), Feedback>,
    pub held_plan_feedback: Vec<HeldFeedback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshotBundle {
    pub root: PathBuf,
    pub head: Option<CommitSha>,
    pub session: SessionSnapshot,
    pub attribution: BTreeMap<CommitSha, AttributionResult>,
    pub plan_touches: BTreeMap<CommitSha, Vec<(SessionId, PlanTouchKind)>>,
    pub commit_order: Vec<CommitSha>,
}

impl RepoSnapshot {
    pub fn from_state(state: &RepoState) -> Self {
        Self {
            root: state.root.clone(),
            head: state.head.clone(),
            sessions: state
                .sessions
                .values()
                .map(SessionSnapshot::from_session)
                .collect(),
            attribution: state.attribution.clone(),
            plan_touches: state.plan_touches.clone(),
            commit_order: state.commit_order.clone(),
        }
    }

    pub fn to_repo_state(&self) -> RepoState {
        RepoState {
            root: self.root.clone(),
            sessions: self
                .sessions
                .iter()
                .map(|session| (session.id.clone(), session.to_session()))
                .collect(),
            head: self.head.clone(),
            attribution: self.attribution.clone(),
            plan_touches: self.plan_touches.clone(),
            commit_order: self.commit_order.clone(),
        }
    }
}

impl SessionSnapshot {
    pub fn from_session(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            plan_path: session.plan_path.clone(),
            body: session.body.clone(),
            body_hash: session.body_hash.clone(),
            plan_intro: session.plan_intro.clone(),
            plan_intro_parent: session.plan_intro_parent.clone(),
            plan_feedback: session.plan_feedback.clone(),
            impl_feedback: session.impl_feedback.clone(),
            held_plan_feedback: session.held_plan_feedback.clone(),
        }
    }

    pub fn to_session(&self) -> Session {
        Session {
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

impl SessionSnapshotBundle {
    pub fn from_state_for(state: &RepoState, session_id: &SessionId) -> Option<Self> {
        let session = state.sessions.get(session_id)?;
        Some(Self {
            root: state.root.clone(),
            head: state.head.clone(),
            session: SessionSnapshot::from_session(session),
            attribution: state.attribution.clone(),
            plan_touches: state.plan_touches.clone(),
            commit_order: state.commit_order.clone(),
        })
    }

    pub fn to_repo_state(&self) -> RepoState {
        RepoState {
            root: self.root.clone(),
            sessions: [(self.session.id.clone(), self.session.to_session())]
                .into_iter()
                .collect(),
            head: self.head.clone(),
            attribution: self.attribution.clone(),
            plan_touches: self.plan_touches.clone(),
            commit_order: self.commit_order.clone(),
        }
    }
}
