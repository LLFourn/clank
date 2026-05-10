use axum::Form;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::domain::EventKind;
use crate::storage::{agents, batches, events as ev_store, plan_revisions, plans};
use crate::tools::master as master_tools;

/// Inputs for any per-feedback action (stage/unstage/edit/delete).
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

fn default_curator_name() -> String {
    "human".to_string()
}

/// Stage a pending feedback event → curator wants this delivered.
pub async fn stage_feedback(
    State(state): State<AppState>,
    Path((plan_id, event_id)): Path<(String, i64)>,
) -> Result<Redirect, ApiError> {
    update_feedback_status(&state, &plan_id, event_id, "pending", "staged").await?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

pub async fn unstage_feedback(
    State(state): State<AppState>,
    Path((plan_id, event_id)): Path<(String, i64)>,
) -> Result<Redirect, ApiError> {
    update_feedback_status(&state, &plan_id, event_id, "staged", "pending").await?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

pub async fn edit_feedback(
    State(state): State<AppState>,
    Path((plan_id, event_id)): Path<(String, i64)>,
    Form(form): Form<EditFeedbackForm>,
) -> Result<Redirect, ApiError> {
    if form.text.trim().is_empty() {
        return Err(ApiError::bad("text must be non-empty"));
    }
    let event = require_pending_or_staged_feedback(&state, &plan_id, event_id).await?;
    let payload = json!({"text": form.text, "edited_at": chrono::Utc::now().timestamp(), "edited_from": event.payload});
    sqlx::query("UPDATE events SET payload = ? WHERE id = ?")
        .bind(payload.to_string())
        .bind(event_id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

pub async fn delete_feedback(
    State(state): State<AppState>,
    Path((plan_id, event_id)): Path<(String, i64)>,
) -> Result<Redirect, ApiError> {
    update_feedback_status(
        &state,
        &plan_id,
        event_id,
        "_any_pre_delivery_",
        "withdrawn",
    )
    .await?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

/// Curator clicks Deliver — bundle currently-staged feedback for `target_kind`
/// into a new directive_batch and flip those events to status='delivered'.
pub async fn deliver(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
    Form(form): Form<DeliverForm>,
) -> Result<Redirect, ApiError> {
    let target_kind = match form.target_kind.as_str() {
        "plan_revision" | "implementation_commit" => form.target_kind.as_str(),
        other => return Err(ApiError::bad(format!("invalid target_kind: {other}"))),
    };
    let now = chrono::Utc::now().timestamp();

    let mut tx = state.pool.begin().await.map_err(ApiError::sqlx)?;

    // Find currently-staged feedback events for this plan + target_kind.
    let staged: Vec<(i64,)> = sqlx::query_as(
        "SELECT id FROM events WHERE plan_id = ? AND kind = 'feedback_added' \
         AND target_kind = ? AND status = 'staged' ORDER BY id ASC",
    )
    .bind(&plan_id)
    .bind(target_kind)
    .fetch_all(&mut *tx)
    .await
    .map_err(ApiError::sqlx)?;

    if staged.is_empty() {
        return Err(ApiError::bad("nothing staged to deliver"));
    }

    let batch_id = batches::create(
        &mut *tx,
        &plan_id,
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
    plans::touch_updated_at(&mut *tx, &plan_id, now)
        .await
        .map_err(ApiError::sqlx)?;
    tx.commit().await.map_err(ApiError::sqlx)?;

    Ok(Redirect::to(&format!("/plans/{plan_id}")))
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

pub async fn comment(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
    Form(form): Form<CommentForm>,
) -> Result<Redirect, ApiError> {
    let _ = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    if form.text.trim().is_empty() {
        return Err(ApiError::bad("comment text required"));
    }
    let now = chrono::Utc::now().timestamp();
    let actor = format!("human:{}", form.author);
    let payload = json!({"text": form.text});
    ev_store::append(
        &state.pool,
        &ev_store::NewEvent::note(&plan_id, EventKind::HumanComment, &actor, &payload, now),
    )
    .await
    .map_err(ApiError::sqlx)?;
    plans::touch_updated_at(&state.pool, &plan_id, now)
        .await
        .map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

pub async fn archive(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let plan = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "UPDATE plans SET state = 'archived', archived_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(now)
    .bind(now)
    .bind(&plan.id)
    .execute(&state.pool)
    .await
    .map_err(ApiError::sqlx)?;
    let payload = json!({});
    ev_store::append(
        &state.pool,
        &ev_store::NewEvent::note(
            &plan.id,
            EventKind::Archived,
            "human:curator",
            &payload,
            now,
        ),
    )
    .await
    .map_err(ApiError::sqlx)?;
    if let Some(path) = plan.plan_path() {
        let _ = state.watcher.unwatch(&path);
    }
    Ok(Redirect::to("/"))
}

pub async fn rename(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
    Form(form): Form<RenameForm>,
) -> Result<Redirect, ApiError> {
    if form.display_title.trim().is_empty() {
        return Err(ApiError::bad("display_title required"));
    }
    let now = chrono::Utc::now().timestamp();
    sqlx::query("UPDATE plans SET display_title = ?, updated_at = ? WHERE id = ?")
        .bind(&form.display_title)
        .bind(now)
        .bind(&plan_id)
        .execute(&state.pool)
        .await
        .map_err(ApiError::sqlx)?;
    let payload = json!({"display_title": form.display_title});
    ev_store::append(
        &state.pool,
        &ev_store::NewEvent::note(&plan_id, EventKind::Renamed, "human:curator", &payload, now),
    )
    .await
    .map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

pub async fn register_head_as_impl(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let plan = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    match master_tools::register_commit_inner(
        &state,
        &plan,
        "HEAD",
        false,
        "human:ui_fallback",
        "ui_from_head",
    )
    .await
    {
        Ok(_) => Ok(Redirect::to(&format!("/plans/{plan_id}"))),
        Err(crate::tools::ToolError::Invalid(msg)) => Err(ApiError::bad(msg)),
        Err(crate::tools::ToolError::NotFound(msg)) => Err(ApiError::not_found(msg)),
        Err(crate::tools::ToolError::Forbidden(msg)) => Err(ApiError::bad(msg)),
        Err(crate::tools::ToolError::Internal(err)) => {
            tracing::error!(error = ?err, "register_head_as_impl");
            Err(ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: format!("{err}"),
            })
        }
    }
}

pub async fn mark_done(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let plan = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    if plan.state != "implementation_review" && plan.state != "plan_approved" {
        return Err(ApiError::bad(format!(
            "plan is in state `{}`, can only mark done from `implementation_review` or `plan_approved`",
            plan.state
        )));
    }
    let now = chrono::Utc::now().timestamp();
    let mut tx = state.pool.begin().await.map_err(ApiError::sqlx)?;
    sqlx::query("UPDATE plans SET state = 'done', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(&plan.id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::sqlx)?;
    let done_payload = json!({});
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::note(
            &plan.id,
            EventKind::MarkedDone,
            "human:curator",
            &done_payload,
            now,
        ),
    )
    .await
    .map_err(ApiError::sqlx)?;
    let st_payload = json!({"from": plan.state, "to": "done"});
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::note(
            &plan.id,
            EventKind::StateTransition,
            "human:curator",
            &st_payload,
            now,
        ),
    )
    .await
    .map_err(ApiError::sqlx)?;
    tx.commit().await.map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

pub async fn approve_plan(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let plan = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    if plan.state != "planning" {
        return Err(ApiError::bad(format!(
            "plan is in state `{}`, can only approve from `planning`",
            plan.state
        )));
    }
    let now = chrono::Utc::now().timestamp();
    let mut tx = state.pool.begin().await.map_err(ApiError::sqlx)?;
    sqlx::query("UPDATE plans SET state = 'plan_approved', updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(&plan.id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::sqlx)?;
    let approve_payload = json!({});
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::note(
            &plan.id,
            EventKind::PlanApproved,
            "human:curator",
            &approve_payload,
            now,
        ),
    )
    .await
    .map_err(ApiError::sqlx)?;
    let st_payload = json!({"from": "planning", "to": "plan_approved"});
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::note(
            &plan.id,
            EventKind::StateTransition,
            "human:curator",
            &st_payload,
            now,
        ),
    )
    .await
    .map_err(ApiError::sqlx)?;
    tx.commit().await.map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

pub async fn evict_master(
    State(state): State<AppState>,
    Path(plan_id): Path<String>,
) -> Result<Redirect, ApiError> {
    let _ = plans::fetch(&state.pool, &plan_id)
        .await
        .map_err(ApiError::sqlx)?
        .ok_or_else(|| ApiError::not_found("plan not found"))?;
    let now = chrono::Utc::now().timestamp();
    let master = agents::fetch_master(&state.pool, &plan_id)
        .await
        .map_err(ApiError::sqlx)?;
    let mut tx = state.pool.begin().await.map_err(ApiError::sqlx)?;
    sqlx::query("UPDATE plans SET master_agent_id = NULL, updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(&plan_id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::sqlx)?;
    if let Some(master) = &master {
        sqlx::query("DELETE FROM agents WHERE id = ?")
            .bind(master.id)
            .execute(&mut *tx)
            .await
            .map_err(ApiError::sqlx)?;
    }
    let payload = json!({"prior_label": master.as_ref().map(|m| m.label.clone())});
    ev_store::append(
        &mut *tx,
        &ev_store::NewEvent::note(
            &plan_id,
            EventKind::MasterEvicted,
            "human:curator",
            &payload,
            now,
        ),
    )
    .await
    .map_err(ApiError::sqlx)?;
    tx.commit().await.map_err(ApiError::sqlx)?;
    Ok(Redirect::to(&format!("/plans/{plan_id}")))
}

async fn update_feedback_status(
    state: &AppState,
    plan_id: &str,
    event_id: i64,
    expected_from: &str,
    new_status: &str,
) -> Result<(), ApiError> {
    let event = require_pending_or_staged_feedback(state, plan_id, event_id).await?;
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
    plan_id: &str,
    event_id: i64,
) -> Result<ev_store::Event, ApiError> {
    let row = sqlx::query_as::<_, ev_store::Event>(
        "SELECT * FROM events WHERE id = ? AND plan_id = ? AND kind = 'feedback_added'",
    )
    .bind(event_id)
    .bind(plan_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::sqlx)?
    .ok_or_else(|| ApiError::not_found("feedback event not found on this plan"))?;
    let status = row.status.as_deref().unwrap_or("");
    if !matches!(status, "pending" | "staged") {
        return Err(ApiError::bad(format!(
            "feedback is `{}` and cannot be modified",
            status
        )));
    }
    Ok(row)
}

#[allow(dead_code)]
fn _ensure_unused(_: plan_revisions::PlanRevision) {}

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
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}
