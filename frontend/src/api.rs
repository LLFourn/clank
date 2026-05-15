//! Typed fetch wrappers around the daemon's `/api/*` surface.
//!
//! Shapes deliberately mirror what `src/ui_response.rs` produces on the
//! daemon side. When changing one, update the other.

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
pub struct SessionRow {
    pub repo: String,
    pub session_id: String,
    pub plan_path: String,
    pub phase: String,
    pub worktree_status: String,
    pub waiting_on: WaitingOn,
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
    },
    #[serde(rename = "commit_impl")]
    CommitImpl {
        sha: String,
        plan_touch: Option<String>,
        has_code_changes: bool,
    },
    #[serde(rename = "commit_mixed")]
    CommitMixed {
        sha: String,
        plan_touch: Option<String>,
        has_code_changes: bool,
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
pub struct SessionDetail {
    pub repo: String,
    pub session_id: String,
    pub phase: String,
    pub plan_path: String,
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
    pub timeline: Vec<TimelineEvent>,
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

pub async fn fetch_sessions() -> Result<Vec<SessionRow>, FetchError> {
    let resp = gloo_net::http::Request::get("/api/sessions")
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<Vec<SessionRow>>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

pub async fn fetch_session(session_id: String) -> Result<SessionDetail, FetchError> {
    let url = format!("/api/sessions/{session_id}");
    let resp = gloo_net::http::Request::get(&url)
        .send()
        .await
        .map_err(|e| FetchError::Network(e.to_string()))?;
    if !resp.ok() {
        return Err(FetchError::Status(resp.status()));
    }
    resp.json::<SessionDetail>()
        .await
        .map_err(|e| FetchError::Decode(e.to_string()))
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PlanRevisionPage {
    pub repo: String,
    pub session_id: String,
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
    session_id: String,
    sha: String,
) -> Result<PlanRevisionPage, FetchError> {
    let url = format!("/api/sessions/{session_id}/plan/{sha}");
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
    pub session_id: String,
    pub commit_sha: String,
    #[serde(default)]
    pub diff_files: Vec<FileDiff>,
    #[serde(default)]
    pub feedback: Vec<FeedbackEntry>,
}

pub async fn fetch_commit_diff(
    session_id: String,
    sha: String,
) -> Result<CommitDiffPage, FetchError> {
    let url = format!("/api/sessions/{session_id}/commit/{sha}");
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
    pub from: String,
    pub to: String,
    pub path: String,
    #[serde(default)]
    pub diff_files: Vec<FileDiff>,
}

pub async fn fetch_diff(
    from: String,
    to: String,
    path: String,
) -> Result<DiffPage, FetchError> {
    let mut params = String::new();
    params.push_str("from=");
    params.push_str(&urlencode(&from));
    params.push_str("&to=");
    params.push_str(&urlencode(&to));
    params.push_str("&path=");
    params.push_str(&urlencode(&path));
    let url = format!("/api/diff?{params}");
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

/// Minimal percent-encoder for query-string segments. Avoids pulling in
/// a whole url crate just for this.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}
