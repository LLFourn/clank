//! Typed fetch wrappers around the daemon's `/api/*` surface.
//!
//! Shapes deliberately mirror what `src/ui_response.rs` produces on the
//! daemon side. When changing one, update the other.
//!
//! Several structs carry `#[allow(dead_code)]` because the SPA does not
//! consume every JSON field yet (e.g. `expected_action`,
//! `implementation_commits`, `plan_intro`). The allow is intentional and
//! per-struct: the type round-trips the full contract so a future
//! component is a UI-only change rather than a coordinated daemon+SPA
//! edit.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct WaitingOn {
    pub role: String,
    pub reason: String,
    #[serde(default)]
    pub agents: Vec<String>,
    pub description: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PlanRow {
    pub repo: String,
    pub plan_id: String,
    pub slug: String,
    pub state: String,
    pub current_path: String,
    pub phase: String,
    pub worktree_status: String,
    pub waiting_on: WaitingOn,
    #[serde(default)]
    pub last_activity_ts: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PlanConflictRow {
    pub plan_id: String,
    pub slug: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PlansIndex {
    #[serde(default)]
    pub plans: Vec<PlanRow>,
    #[serde(default)]
    pub conflicts: Vec<PlanConflictRow>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ReviewTarget {
    pub phase: String,
    pub commit_sha: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct ReviewGate {
    pub state: String,
    pub phase: String,
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub approvals: Vec<String>,
    #[serde(default)]
    pub request_changes: Vec<String>,
    #[serde(default)]
    pub missing_approvals: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct CommitRef {
    pub commit_sha: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct FeedbackEntry {
    pub target_sha: String,
    pub author: String,
    pub verdict: String,
    pub body_raw: String,
    pub body_html: String,
    pub path: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct HeldFeedbackEntry {
    pub author: String,
    pub verdict: String,
    pub body_raw: String,
    pub body_html: String,
    pub path: String,
    pub reason: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind")]
#[allow(dead_code)]
pub enum TimelineEvent {
    #[serde(rename = "commit_plan")]
    CommitPlan {
        sha: String,
        plan_touch: Option<String>,
        has_code_changes: bool,
        #[serde(default)]
        subject: String,
    },
    #[serde(rename = "commit_impl")]
    CommitImpl {
        sha: String,
        plan_touch: Option<String>,
        has_code_changes: bool,
        #[serde(default)]
        subject: String,
    },
    #[serde(rename = "commit_mixed")]
    CommitMixed {
        sha: String,
        plan_touch: Option<String>,
        has_code_changes: bool,
        #[serde(default)]
        subject: String,
    },
    #[serde(rename = "review")]
    Review {
        phase: String,
        target: String,
        author: String,
        verdict: String,
        #[serde(default)]
        created_at: i64,
    },
    #[serde(rename = "held_feedback")]
    HeldFeedback {
        author: String,
        reason: String,
        #[serde(default)]
        created_at: i64,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PrHintOption {
    pub name: String,
    pub base: String,
    pub command: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PrHint {
    pub plan_intro: String,
    pub plan_intro_parent: Option<String>,
    #[serde(default)]
    pub implementation_commits: Vec<String>,
    #[serde(default)]
    pub options: Vec<PrHintOption>,
    pub suggested_message: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PlanDetail {
    pub repo: String,
    pub plan_id: String,
    pub slug: String,
    pub state: String,
    pub current_path: String,
    pub phase: String,
    pub plan_worktree_status: String,
    pub waiting_on: WaitingOn,
    pub expected_action: String,
    pub review_target: Option<ReviewTarget>,
    pub review_gate: Option<ReviewGate>,
    pub latest_plan_revision: Option<CommitRef>,
    pub latest_implementation_revision: Option<CommitRef>,
    #[serde(default)]
    pub plan_revisions: Vec<String>,
    #[serde(default)]
    pub implementation_commits: Vec<String>,
    #[serde(default)]
    pub plan_feedback: Vec<FeedbackEntry>,
    #[serde(default)]
    pub impl_feedback: Vec<FeedbackEntry>,
    #[serde(default)]
    pub held_plan_feedback: Vec<HeldFeedbackEntry>,
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
#[allow(dead_code)]
pub struct PlanRevisionPage {
    pub repo: String,
    pub plan_id: String,
    pub slug: String,
    pub commit_sha: String,
    pub body_raw: String,
    pub body_html: String,
    pub plan_intro: String,
    pub plan_intro_parent: Option<String>,
    pub previous_sha: Option<String>,
    pub next_sha: Option<String>,
    #[serde(default)]
    pub feedback: Vec<FeedbackEntry>,
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
#[allow(dead_code)]
pub struct DiffLine {
    pub kind: String,
    pub old_lineno: Option<u64>,
    pub new_lineno: Option<u64>,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct CommitDiffPage {
    pub repo: String,
    pub plan_id: String,
    pub slug: String,
    pub commit_sha: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub message_body: String,
    #[serde(default)]
    pub diff_files: Vec<FileDiff>,
    #[serde(default)]
    pub feedback: Vec<FeedbackEntry>,
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
#[allow(dead_code)]
pub struct DiffPage {
    pub repo: String,
    pub plan_id: String,
    pub from: String,
    pub to: String,
    pub from_path: String,
    pub to_path: String,
    #[serde(default)]
    pub diff_files: Vec<FileDiff>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DoneResponse {
    pub ok: bool,
    pub new_plan_path: String,
}

/// `POST /api/plan/{plan_id}/done` — moves the active plan file under
/// `.trinity/plans/done/`. Returns the new repo-relative path on
/// success.
pub async fn post_move_to_done(plan_id: String) -> Result<DoneResponse, FetchError> {
    let url = format!("/api/plan/{plan_id}/done");
    let resp = gloo_net::http::Request::post(&url)
        .header("content-type", "application/json")
        .body("{}")
        .map_err(|e| FetchError::Network(e.to_string()))?
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<DoneResponse>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
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
#[allow(dead_code)]
pub struct RepoRow {
    pub basename: String,
    pub root: String,
    pub plan_count: u32,
    #[serde(default)]
    pub last_activity_ts: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
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

pub async fn delete_repo(basename: String) -> Result<(), FetchError> {
    let url = format!("/api/repos/{basename}");
    let resp = gloo_net::http::Request::delete(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    Ok(())
}
