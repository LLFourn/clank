//! Human-curator HTTP routes (session-scoped admin). The non-lifecycle
//! routes here (comment, rename) write directly to the DB; the archive
//! route funnels through `SessionService::observe` like an agent
//! observation would. Feedback ingest lives entirely on the
//! filesystem-watcher path — none of it is here.

use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::domain::EventKind;
use crate::lifecycle::{Observation, SessionId};
use crate::storage::{events as ev_store, sessions};

#[derive(Debug, Deserialize)]
pub struct CommentForm {
    pub text: String,
    #[serde(default = "default_curator_name")]
    pub author: String,
}

#[derive(Debug, Deserialize)]
pub struct RenameForm {
    pub display_title: String,
}

fn default_curator_name() -> String {
    "human".to_string()
}

// ---- comment ----

pub async fn comment(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Form(form): Form<CommentForm>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    let _ = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found(format!("session `{session_id}` not found")))?;
    if form.text.trim().is_empty() {
        return Err(ApiError::bad("comment text required"));
    }
    let now = chrono::Utc::now().timestamp();
    let actor = format!("human:{}", form.author);
    let payload = json!({ "comment": form.text });
    ev_store::append(
        &state.pool,
        &sid,
        None,
        None,
        None,
        EventKind::HumanComment.as_str(),
        &actor,
        &payload,
        None,
        now,
    )
    .await
    .map_err(ApiError::sqlx)?;
    sessions::touch_updated_at(&state.pool, &sid, now)
        .await
        .map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

// ---- archive (lifecycle observation) ----

pub async fn archive(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    state
        .lifecycle
        .observe(&sid, "human:curator", Observation::ArchiveRequested)
        .await
        .map_err(ApiError::lifecycle)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

// ---- rename ----

pub async fn rename(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Form(form): Form<RenameForm>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    if form.display_title.trim().is_empty() {
        return Err(ApiError::bad("display_title required"));
    }
    let now = chrono::Utc::now().timestamp();
    sessions::set_display_title(&state.pool, &sid, &form.display_title, now)
        .await
        .map_err(ApiError::sqlx)?;
    let payload = json!({ "display_title": form.display_title });
    ev_store::append(
        &state.pool,
        &sid,
        None,
        None,
        None,
        EventKind::Renamed.as_str(),
        "human:curator",
        &payload,
        None,
        now,
    )
    .await
    .map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }
    fn sqlx(err: sqlx::Error) -> Self {
        tracing::error!(error = ?err, "sqlx error");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("database error: {err}"),
        }
    }
    fn lifecycle(err: super::LifecycleServiceError) -> Self {
        match err {
            super::LifecycleServiceError::Reducer(r) => Self::bad(r.to_string()),
            super::LifecycleServiceError::NoSession(s) => {
                Self::not_found(format!("session `{s}` not found"))
            }
            super::LifecycleServiceError::Apply(a) => {
                tracing::error!(error = ?a, "apply error");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("apply: {a}"),
                }
            }
            super::LifecycleServiceError::ActivePlan(inc) => {
                tracing::error!(error = ?inc, "active plan invariant");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("invariant: {inc}"),
                }
            }
            super::LifecycleServiceError::Feedback(e) => {
                tracing::error!(error = ?e, "feedback storage error");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("feedback storage: {e}"),
                }
            }
            super::LifecycleServiceError::Sql(e) => Self::sqlx(e),
            super::LifecycleServiceError::InvalidArgs(msg) => Self::bad(msg),
            super::LifecycleServiceError::Watcher(err) => {
                tracing::error!(error = ?err, "watcher attach failed");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("watcher: {err}"),
                }
            }
            super::LifecycleServiceError::RepoMismatch {
                session_id,
                expected,
                actual,
            } => Self::bad(format!(
                "session `{session_id}` is recorded under repo `{expected}`, not `{actual}`"
            )),
            super::LifecycleServiceError::SessionArchived(s) => {
                Self::bad(format!("session `{s}` is archived"))
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
