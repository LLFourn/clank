//! Typed fetch wrappers around the daemon's `/api/*` surface.
//!
//! Shapes deliberately mirror what `src/ui_response.rs` produces on the
//! daemon side. When changing one, update the other. Fields the SPA
//! does not consume are deleted, not retained behind `#[allow(dead_code)]`;
//! the daemon may continue emitting them (serde ignores extras) but the
//! frontend type only carries what the UI reads.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WaitingOn {
    pub role: String,
    #[serde(default)]
    pub agents: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanRow {
    pub plan_id: String,
    pub state: String,
    pub phase: String,
    pub worktree_status: String,
    pub waiting_on: WaitingOn,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanConflictRow {
    pub slug: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlansIndex {
    #[serde(default)]
    pub plans: Vec<PlanRow>,
    #[serde(default)]
    pub conflicts: Vec<PlanConflictRow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReviewGate {
    pub state: String,
    #[serde(default)]
    pub approvals: Vec<String>,
    #[serde(default)]
    pub request_changes: Vec<String>,
    #[serde(default)]
    pub missing_approvals: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitFeedback {
    pub author: String,
    pub verdict: String,
    pub body_html: String,
    #[serde(default)]
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitEntry {
    pub sha: String,
    pub kind: String,
    #[serde(default)]
    pub feedback: Vec<CommitFeedback>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitRef {
    pub commit_sha: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
pub enum TimelineEvent {
    #[serde(rename = "commit_plan")]
    CommitPlan {
        sha: String,
        #[serde(default)]
        subject: String,
    },
    #[serde(rename = "commit_impl")]
    CommitImpl {
        sha: String,
        #[serde(default)]
        subject: String,
    },
    #[serde(rename = "commit_mixed")]
    CommitMixed {
        sha: String,
        #[serde(default)]
        subject: String,
    },
    #[serde(rename = "commit_finalize")]
    CommitFinalize {
        sha: String,
        #[serde(default)]
        subject: String,
    },
    #[serde(rename = "review")]
    Review {
        phase: String,
        target: String,
        author: String,
        verdict: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrHintOption {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrHint {
    #[serde(default)]
    pub options: Vec<PrHintOption>,
    pub suggested_message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanDetail {
    pub plan_id: String,
    pub state: String,
    pub current_path: String,
    pub phase: String,
    pub plan_worktree_status: String,
    pub waiting_on: WaitingOn,
    pub review_gate: Option<ReviewGate>,
    pub latest_plan_revision: Option<CommitRef>,
    pub latest_implementation_revision: Option<CommitRef>,
    #[serde(default)]
    pub commits: Vec<CommitEntry>,
    #[serde(default)]
    pub latest_relevant_commit: Option<String>,
    #[serde(default)]
    pub plan_body_html: String,
    #[serde(default)]
    pub plan_body_truncated: bool,
    #[serde(default)]
    pub timeline: Vec<TimelineEvent>,
    pub pr_hint: Option<PrHint>,
}

#[derive(Debug, Clone)]
pub enum FetchError {
    Network(String),
    Status(u16),
    Decode(String),
}

impl core::fmt::Display for FetchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FetchError::Network(s) => write!(f, "network: {s}"),
            FetchError::Status(c) => write!(f, "HTTP {c}"),
            FetchError::Decode(s) => write!(f, "decode: {s}"),
        }
    }
}

pub async fn fetch_plans() -> Result<PlansIndex, FetchError> {
    let resp = gloo_net::http::Request::get("/api/plans")
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<PlansIndex>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn fetch_plan(plan_id: String) -> Result<PlanDetail, FetchError> {
    let url = format!("/api/plan/{plan_id}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<PlanDetail>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanRevisionPage {
    pub plan_id: String,
    pub commit_sha: String,
    pub body_html: String,
    pub previous_sha: Option<String>,
    pub next_sha: Option<String>,
    #[serde(default)]
    pub feedback: Vec<CommitFeedback>,
}

pub async fn fetch_plan_revision(
    plan_id: String,
    sha: String,
) -> Result<PlanRevisionPage, FetchError> {
    let url = format!("/api/plan/{plan_id}/revision/{sha}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<PlanRevisionPage>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiffLine {
    pub kind: String,
    pub old_lineno: Option<u64>,
    pub new_lineno: Option<u64>,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub old_path: Option<String>,
    pub additions: u64,
    pub deletions: u64,
    pub mode: String,
    pub binary: bool,
    #[serde(default)]
    pub always_folded: bool,
    #[serde(default)]
    pub hunks: Vec<DiffHunk>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CommitDiffPage {
    pub plan_id: String,
    pub commit_sha: String,
    /// Daemon-side `CommitKind::as_str()` — `plan_only` / `code_only`
    /// / `mixed` / `multi_plan` / `finalize`. The UI keys finalize
    /// rendering off `"finalize"`. Required: every commit-detail
    /// response carries it. No `#[serde(default)]` — wire drift
    /// surfaces as a decode error rather than silently rendering
    /// as a non-finalize commit.
    pub kind: String,
    #[serde(default)]
    pub message_body: String,
    #[serde(default)]
    pub diff_files: Vec<FileDiff>,
    /// Live reviewer feedback on this commit. Always present on the
    /// wire as a (possibly empty) array; the daemon emits `[]` for
    /// finalize commits.
    pub feedback: Vec<CommitFeedback>,
    /// Approval snapshot for finalize commits: the `.trinity/finished/
    /// <stem>/` directory contents at the freeze commit's tree.
    /// Always present on the wire as a (possibly empty) array; the
    /// daemon emits `[]` for non-finalize commits.
    pub finalize_snapshot: Vec<FinalizeApproval>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FinalizeApproval {
    pub author: String,
    pub filename: String,
    pub body_html: String,
}

pub async fn fetch_commit_diff(plan_id: String, sha: String) -> Result<CommitDiffPage, FetchError> {
    let url = format!("/api/plan/{plan_id}/commit/{sha}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<CommitDiffPage>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct DiffPage {
    pub from: String,
    pub to: String,
    pub from_path: String,
    pub to_path: String,
    #[serde(default)]
    pub diff_files: Vec<FileDiff>,
}

pub async fn fetch_diff(plan_id: String, from: String, to: String) -> Result<DiffPage, FetchError> {
    let url = format!("/api/plan/{plan_id}/diff/{from}/{to}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<DiffPage>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoRow {
    pub basename: String,
    pub root: String,
    pub plan_count: u32,
    #[serde(default)]
    pub last_activity_ts: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReposIndex {
    #[serde(default)]
    pub repos: Vec<RepoRow>,
}

pub async fn fetch_repos() -> Result<ReposIndex, FetchError> {
    let resp = gloo_net::http::Request::get("/api/repos")
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<ReposIndex>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeleteRepoOutcome {
    /// `Some(msg)` when the in-memory deregistration succeeded but the
    /// registry file rewrite failed. The repo will reappear on daemon
    /// restart until the operator fixes the file; the frontend should
    /// surface this so it isn't silently lost.
    #[serde(default)]
    pub registry_write_error: Option<String>,
}

pub async fn delete_repo(basename: String) -> Result<DeleteRepoOutcome, FetchError> {
    let url = format!("/api/repos/{basename}");
    let resp = gloo_net::http::Request::delete(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<DeleteRepoOutcome>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[cfg(test)]
mod dto_roundtrip_tests {
    //! DTO contract tests for `/api/plan/.../commit/{sha}`. The
    //! daemon's response shape is hand-maintained in
    //! `src/server/http.rs::api_commit_diff`; the frontend DTO
    //! (`CommitDiffPage`) must round-trip every shape the daemon
    //! emits. Without these tests, a backend that emits `null` for
    //! an array-typed field silently breaks every commit page on
    //! the live daemon — the kind of regression ruthless caught in
    //! 055d387.
    use super::*;

    #[test]
    fn reviewable_commit_response_decodes() {
        // Daemon emits empty `finalize_snapshot: []` for a
        // non-finalize commit; the DTO's `Vec<FinalizeApproval>`
        // must accept it.
        let wire = r#"{
            "plan_id": "trinity/foo.md",
            "commit_sha": "abcdef",
            "kind": "plan_only",
            "subject": "Plan revision",
            "message_body": "",
            "diff_files": [],
            "feedback": [],
            "finalize_snapshot": []
        }"#;
        let page: CommitDiffPage = serde_json::from_str(wire).expect("decode reviewable commit");
        assert_eq!(page.kind, "plan_only");
        assert!(page.finalize_snapshot.is_empty());
        assert!(page.feedback.is_empty());
    }

    #[test]
    fn finalize_commit_response_decodes() {
        let wire = r#"{
            "plan_id": "trinity/foo.md",
            "commit_sha": "abcdef",
            "kind": "finalize",
            "subject": "Finalize foo",
            "message_body": "",
            "diff_files": [],
            "feedback": [],
            "finalize_snapshot": [
                {"author": "alice", "filename": "alice.md", "body_html": "<p>lgtm</p>"}
            ]
        }"#;
        let page: CommitDiffPage = serde_json::from_str(wire).expect("decode finalize commit");
        assert_eq!(page.kind, "finalize");
        assert_eq!(page.finalize_snapshot.len(), 1);
        assert_eq!(page.finalize_snapshot[0].author, "alice");
    }

    #[test]
    fn null_finalize_snapshot_fails_to_decode() {
        // Regression: prior to the fix, the daemon emitted `null`
        // for non-finalize commits. This test pins the failure
        // mode so a future "be lenient" change doesn't reintroduce
        // the silent-degrade bug.
        let wire = r#"{
            "plan_id": "trinity/foo.md",
            "commit_sha": "abcdef",
            "kind": "plan_only",
            "feedback": [],
            "finalize_snapshot": null
        }"#;
        let result: Result<CommitDiffPage, _> = serde_json::from_str(wire);
        assert!(
            result.is_err(),
            "null finalize_snapshot must fail to decode (daemon must emit `[]`); got: {result:?}"
        );
    }

    #[test]
    fn missing_kind_fails_to_decode() {
        // Without `#[serde(default)]` on `kind`, wire drift surfaces
        // as a decode error rather than silently rendering as a
        // non-finalize commit.
        let wire = r#"{
            "plan_id": "trinity/foo.md",
            "commit_sha": "abcdef",
            "feedback": [],
            "finalize_snapshot": []
        }"#;
        let result: Result<CommitDiffPage, _> = serde_json::from_str(wire);
        assert!(
            result.is_err(),
            "missing `kind` must fail to decode; got: {result:?}"
        );
    }
}
