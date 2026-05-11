//! Curator HTTP routes (the human's surface). All session-scoped. Mutating
//! lifecycle actions (archive) funnel through `SessionLifecycle::observe`;
//! purely "curatorial" mutations (stage, edit, deliver, comment, rename,
//! evict-master, register-from-HEAD) write directly to the DB without going
//! through the reducer.

use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::domain::EventKind;
use crate::lifecycle::{AgentLabel, CommitSha, CommitSnapshot, Observation, SessionId};
use crate::storage::{
    agents, batches, events as ev_store, implementation_revisions as impl_revs, plans, sessions,
};

#[derive(Debug, Deserialize)]
pub struct EditFeedbackForm {
    pub text: String,
}

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

#[derive(Debug, Deserialize)]
pub struct DeliverForm {
    pub target_kind: String,
    #[serde(default)]
    pub author: Option<String>,
}

impl DeliverForm {
    fn author(&self) -> String {
        self.author
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(default_curator_name)
    }
}

fn default_curator_name() -> String {
    "human".to_string()
}

// ---- per-feedback actions ----

pub async fn stage_feedback(
    State(state): State<AppState>,
    Path((session_id, event_id)): Path<(String, i64)>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    update_feedback_status(&state, &sid, event_id, "pending", "staged").await?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

pub async fn unstage_feedback(
    State(state): State<AppState>,
    Path((session_id, event_id)): Path<(String, i64)>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    update_feedback_status(&state, &sid, event_id, "staged", "pending").await?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

pub async fn edit_feedback(
    State(state): State<AppState>,
    Path((session_id, event_id)): Path<(String, i64)>,
    Form(form): Form<EditFeedbackForm>,
) -> Result<Redirect, ApiError> {
    if form.text.trim().is_empty() {
        return Err(ApiError::bad("text must be non-empty"));
    }
    let sid = SessionId::from(session_id.clone());
    let event = require_pending_or_staged_feedback(&state, &sid, event_id).await?;
    let payload = json!({"text": form.text, "edited_at": chrono::Utc::now().timestamp(), "edited_from": event.payload});
    sqlx::query("UPDATE events SET payload = ? WHERE id = ?")
        .bind(payload.to_string())
        .bind(event_id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

pub async fn delete_feedback(
    State(state): State<AppState>,
    Path((session_id, event_id)): Path<(String, i64)>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    update_feedback_status(&state, &sid, event_id, "_any_pre_delivery_", "withdrawn").await?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

// ---- delivery ----

pub async fn deliver(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    Form(form): Form<DeliverForm>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    let target_kind = match form.target_kind.as_str() {
        "plan_revision" | "implementation_commit" => form.target_kind.as_str(),
        other => return Err(ApiError::bad(format!("invalid target_kind: {other}"))),
    };
    let session = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found(format!("session `{session_id}` not found")))?;
    let active_plan_id = session.active_plan_id.ok_or_else(|| {
        ApiError::bad("session has no active plan; nothing to deliver".to_string())
    })?;
    let now = chrono::Utc::now().timestamp();

    let mut tx = state.pool.begin().await.map_err(ApiError::sqlx)?;
    // Scope staged-pickup to the active plan. Old-cycle staged items must
    // not be carried into the new lifecycle (the apply layer also withdraws
    // them on archive, but this filter is the load-bearing defense).
    let staged: Vec<(i64,)> = sqlx::query_as(
        "SELECT id FROM events WHERE session_id = ? AND plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = ? AND status = 'staged' ORDER BY id ASC",
    )
    .bind(sid.as_str())
    .bind(active_plan_id)
    .bind(target_kind)
    .fetch_all(&mut *tx)
    .await
    .map_err(ApiError::sqlx)?;
    if staged.is_empty() {
        return Err(ApiError::bad("nothing staged to deliver"));
    }
    let batch_id = batches::create(
        &mut *tx,
        &sid,
        session.active_plan_id,
        target_kind,
        &format!("human:{}", form.author()),
        now,
    )
    .await
    .map_err(ApiError::sqlx)?;
    for (event_id,) in &staged {
        batches::add_item(&mut *tx, batch_id, *event_id)
            .await
            .map_err(ApiError::sqlx)?;
        sqlx::query("UPDATE events SET status = 'delivered' WHERE id = ?")
            .bind(event_id)
            .execute(&mut *tx)
            .await
            .map_err(ApiError::sqlx)?;
    }
    sessions::touch_updated_at(&mut *tx, &sid, now)
        .await
        .map_err(ApiError::sqlx)?;
    tx.commit().await.map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
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
    let payload = json!({"text": form.text});
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
    // Lifecycle handles the archive. If there's no active plan it's a no-op.
    state
        .lifecycle
        .observe(&sid, "human:curator", Observation::ArchiveRequested)
        .await
        .map_err(ApiError::lifecycle)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

// ---- rename / evict / register-head ----

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
    let payload = json!({"display_title": form.display_title});
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

pub async fn evict_master(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    let _ = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found(format!("session `{session_id}` not found")))?;
    let now = chrono::Utc::now().timestamp();
    let master = agents::fetch_master(&state.pool, &sid)
        .await
        .map_err(ApiError::sqlx)?;
    let mut tx = state.pool.begin().await.map_err(ApiError::sqlx)?;
    // NULL the pointer before deleting the agent row, otherwise the FK from
    // sessions.master_agent_id → agents.id refuses the delete.
    sessions::set_master_agent(&mut *tx, &sid, None, now)
        .await
        .map_err(ApiError::sqlx)?;
    if let Some(master) = &master {
        agents::delete(&mut *tx, master.id)
            .await
            .map_err(ApiError::sqlx)?;
    }
    let payload = json!({"prior_label": master.as_ref().map(|m| m.label.clone())});
    ev_store::append(
        &mut *tx,
        &sid,
        None,
        None,
        None,
        EventKind::MasterEvicted.as_str(),
        "human:curator",
        &payload,
        None,
        now,
    )
    .await
    .map_err(ApiError::sqlx)?;
    tx.commit().await.map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

pub async fn register_head_as_impl(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let sid = SessionId::from(session_id.clone());
    let session = sessions::fetch(&state.pool, &sid)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found(format!("session `{session_id}` not found")))?;
    let repo = std::path::Path::new(&session.repo_root);
    let head_sha = super::git::rev_parse_head(repo)
        .await
        .map_err(|e| ApiError::bad(format!("git rev-parse HEAD: {e}")))?;
    let parent = super::git::parent_sha(repo, &head_sha)
        .await
        .map_err(|e| ApiError::bad(format!("git parent: {e}")))?;
    let branch = super::git::current_branch(repo)
        .await
        .map_err(|e| ApiError::bad(format!("git branch: {e}")))?;
    let message = super::git::commit_message(repo, &head_sha)
        .await
        .map_err(|e| ApiError::bad(format!("git log: {e}")))?;
    let stat = super::git::diff_stat(repo, parent.as_deref(), &head_sha)
        .await
        .map_err(|e| ApiError::bad(format!("git diff-stat: {e}")))?;
    let porcelain = super::git::worktree_porcelain(repo)
        .await
        .map_err(|e| ApiError::bad(format!("git status: {e}")))?;
    let dirty = !porcelain.trim().is_empty();
    let commit = CommitSnapshot {
        sha: CommitSha::from(head_sha),
        parent_sha: parent.map(CommitSha::from),
        branch,
        message,
        diff_stat: stat,
        worktree_status: Some(if dirty {
            porcelain
        } else {
            "clean".to_string()
        }),
        is_head: true,
    };
    state
        .lifecycle
        .observe(
            &sid,
            "human:ui_fallback",
            Observation::CommitObserved { commit },
        )
        .await
        .map_err(ApiError::lifecycle)?;
    Ok(Redirect::to(&format!("/sessions/{session_id}")))
}

// ---- helpers ----

async fn update_feedback_status(
    state: &AppState,
    session_id: &SessionId,
    event_id: i64,
    expected_from: &str,
    new_status: &str,
) -> Result<(), ApiError> {
    let event = require_pending_or_staged_feedback(state, session_id, event_id).await?;
    let current_status = event.status.as_deref().unwrap_or("");
    if expected_from != "_any_pre_delivery_" && current_status != expected_from {
        return Err(ApiError::bad(format!(
            "feedback is `{}`, cannot transition from `{}` → `{}`",
            current_status, expected_from, new_status
        )));
    }
    sqlx::query("UPDATE events SET status = ? WHERE id = ?")
        .bind(new_status)
        .bind(event_id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::sqlx)?;
    Ok(())
}

async fn require_pending_or_staged_feedback(
    state: &AppState,
    session_id: &SessionId,
    event_id: i64,
) -> Result<ev_store::Event, ApiError> {
    let row = sqlx::query_as::<_, ev_store::Event>(
        "SELECT * FROM events WHERE id = ? AND session_id = ? AND kind = 'feedback_added'",
    )
    .bind(event_id)
    .bind(session_id.as_str())
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::sqlx)?
    .ok_or_else(|| ApiError::not_found("feedback event not found on this session"))?;
    let status = row.status.as_deref().unwrap_or("");
    if !matches!(status, "pending" | "staged") {
        return Err(ApiError::bad(format!(
            "feedback is `{}` and cannot be modified",
            status
        )));
    }
    // Refuse to stage/edit/delete feedback that targets a non-active plan.
    // Mutating old-cycle feedback would leak it into the next cycle's deliver.
    let active_plan_id: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = ?")
            .bind(session_id.as_str())
            .fetch_one(&state.pool)
            .await
            .map_err(ApiError::sqlx)?;
    if row.plan_id != active_plan_id {
        return Err(ApiError::bad(
            "feedback belongs to a non-active plan and is read-only history",
        ));
    }
    Ok(row)
}

#[allow(dead_code)]
fn _ensure_used(_: &impl_revs::ImplementationRevision, _: &plans::Plan, _: AgentLabel) {}

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
            super::LifecycleServiceError::TargetNotInActivePlan { .. }
            | super::LifecycleServiceError::NoActivePlanForFeedback(_) => {
                Self::bad(err.to_string())
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
            super::LifecycleServiceError::Sql(e) => Self::sqlx(e),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
