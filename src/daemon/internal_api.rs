use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use crate::tools::{ToolDescriptor, ToolError, dispatch};

/// Forwarded by the stdio shim for every MCP tool call.
///
/// `cwd` is the shim's launch directory (used to derive `repo_root` for
/// `register_plan_file` and to scope `list_sessions`).
///
/// Agent identity (`label` / `author_label`) is carried inside `arguments`,
/// not on the envelope. The shim is responsible for filling those in from
/// its cache before forwarding.
#[derive(Debug, Deserialize)]
pub struct ToolCallRequest {
    pub cwd: PathBuf,
    pub tool: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct ToolCallResponse {
    pub result: serde_json::Value,
}

pub async fn list_tools(State(_state): State<AppState>) -> Json<Vec<ToolDescriptor>> {
    Json(crate::tools::catalog())
}

pub async fn call_tool(
    State(state): State<AppState>,
    Json(req): Json<ToolCallRequest>,
) -> Result<Json<ToolCallResponse>, (StatusCode, String)> {
    match dispatch(&state, &req).await {
        Ok(result) => Ok(Json(ToolCallResponse { result })),
        Err(ToolError::Invalid(msg)) => Err((StatusCode::BAD_REQUEST, msg)),
        Err(ToolError::NotFound(msg)) => Err((StatusCode::NOT_FOUND, msg)),
        Err(ToolError::Forbidden(msg)) => Err((StatusCode::FORBIDDEN, msg)),
        Err(ToolError::Internal(err)) => {
            tracing::error!(error = ?err, "tool call internal error");
            Err((StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")))
        }
    }
}
