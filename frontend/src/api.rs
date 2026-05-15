//! Typed fetch wrappers around the daemon's `/api/*` surface.
//!
//! Shapes deliberately mirror what `src/ui_response.rs` produces on the
//! daemon side. When changing one, update the other.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // fields consumed by later-phase components
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
