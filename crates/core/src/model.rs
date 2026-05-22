//! Wire-only shared types for review-feedback shapes.
//!
//! The sans-io fold lives in [`crate::repo_state`]. This module
//! defines wire-shape DTOs (Feedback, CommitGate) used by response
//! builders; they are NOT folded into state.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::AgentLabel;
use crate::vocab::CommitGateState;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct Feedback {
    pub author: AgentLabel,
    pub verdict: crate::vocab::Verdict,
    pub body: String,
    pub path: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "cache-encoding",
    derive(wincode::SchemaWrite, wincode::SchemaRead)
)]
pub struct CommitGate {
    pub state: CommitGateState,
    pub participants: Vec<AgentLabel>,
    pub approvers: Vec<AgentLabel>,
    pub requesters: Vec<AgentLabel>,
    pub ambiguous: Vec<AgentLabel>,
    pub missing: Vec<AgentLabel>,
    pub feedback: BTreeMap<AgentLabel, Feedback>,
}
