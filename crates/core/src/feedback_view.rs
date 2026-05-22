//! Typed view of one plan's review feedback.
//!
//! Pure data. CLI scans `.clank/feedback/<plan>/<sha>/<author>.md`
//! files (see `clank::feedback_scan`) and hands the result here;
//! `plan_view::project` consumes it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ids::{AgentLabel, CommitSha, ContentHash};
use crate::vocab::Verdict;

/// All feedback the CLI scanned for one plan, indexed by commit.
///
/// `per_commit` is in chronological order (same order as the plan's
/// reviewable timeline). Older commits feed the cumulative
/// participant set; the latest one's entries decide the gate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackView {
    pub per_commit: Vec<CommitFeedback>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitFeedback {
    pub sha: CommitSha,
    pub entries: BTreeMap<AgentLabel, FeedbackEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackEntry {
    pub verdict: Verdict,
    pub body_hash: ContentHash,
    /// Path relative to the repo root. Round-trips for the CLI to
    /// re-read when sealing approvals.
    pub source_path: String,
}
