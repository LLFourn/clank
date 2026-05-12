//! `get_context` — the agent-facing read tool. Returns pointers and
//! freshness only; never artifact content. Agent reads bodies off disk
//! and runs `git` locally.
//!
//! This module is a thin serializer over `SessionService::build_feedback_context`.
//! All target resolution, status derivation, and file partitioning
//! happen inside the service. Author-label filtering (`write_feedback` /
//! `prior_feedback` split) is the only logic that lives here.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, to_value};

use crate::daemon::AppState;
use crate::daemon::internal_api::ToolCallRequest;
use crate::daemon::service::{ActiveTarget, FeedbackContext, FeedbackFileSnapshot};
use crate::domain::{FeedbackFileStatus, FeedbackKind, Phase};
use crate::feedback_path;
use crate::lifecycle::AgentLabel;
use crate::storage::sessions;

use super::ToolError;
use super::master::upsert_seen_with_event;

const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Deserialize)]
struct GetContextArgs {
    session_id: String,
    #[serde(default)]
    author_label: Option<String>,
}

#[derive(serde::Serialize)]
struct GetContextResponse {
    schema_version: u8,
    session_id: String,
    repo_root: PathBuf,
    plan_file_path: PathBuf,
    git_logs_head_path: Option<PathBuf>,
    phase: Phase,
    review_target: Option<ReviewTarget>,
    latest_plan_revision: Option<PlanRevisionView>,
    latest_implementation_revision: Option<ImplRevisionView>,
    write_feedback: Option<FeedbackFileView>,
    prior_feedback: Option<PriorFeedback>,
    other_feedback_files: GroupedFeedbackFiles,
}

#[derive(serde::Serialize)]
struct ReviewTarget {
    kind: String,
    id: String,
    created_at: Option<i64>,
    age_label: Option<String>,
}

#[derive(serde::Serialize)]
struct PlanRevisionView {
    id: i64,
    number: i64,
    content_hash: String,
    created_at: i64,
    age_label: String,
}

#[derive(serde::Serialize)]
struct ImplRevisionView {
    commit_sha: String,
    parent_sha: Option<String>,
    branch: Option<String>,
    is_head: bool,
    worktree_dirty: bool,
    worktree_status_hash: String,
    created_at: i64,
    age_label: String,
}

#[derive(serde::Serialize)]
struct FeedbackFileView {
    kind: FeedbackKind,
    path: String,
    exists: bool,
    status: FeedbackFileStatus,
    last_observed_hash: Option<String>,
    last_observed_at: Option<i64>,
    last_observed_age_label: Option<String>,
    last_ingested_hash: Option<String>,
    last_ingested_at: Option<i64>,
    last_ingested_age_label: Option<String>,
    last_ingested_target: Option<TargetRef>,
    parse_error: Option<String>,
}

#[derive(serde::Serialize)]
struct FeedbackFilePointer {
    author_label: String,
    kind: FeedbackKind,
    path: String,
    status: FeedbackFileStatus,
    last_ingested_at: Option<i64>,
    last_ingested_age_label: Option<String>,
    last_ingested_target: Option<TargetRef>,
}

#[derive(serde::Serialize)]
struct TargetRef {
    kind: String,
    id: String,
}

#[derive(serde::Serialize)]
struct PriorFeedback {
    #[serde(rename = "self")]
    self_: Option<FeedbackFileView>,
    others: Vec<FeedbackFilePointer>,
}

#[derive(serde::Serialize)]
struct GroupedFeedbackFiles {
    plan: Vec<FeedbackFilePointer>,
    #[serde(rename = "impl")]
    impl_: Vec<FeedbackFilePointer>,
}

pub async fn get_context(state: &AppState, req: &ToolCallRequest) -> Result<Value, ToolError> {
    let args: GetContextArgs = serde_json::from_value(req.arguments.clone())
        .map_err(|e| ToolError::Invalid(format!("invalid args: {e}")))?;
    let session_id = sessions::validate_slug(&args.session_id).map_err(ToolError::Invalid)?;

    // Existence check FIRST so unknown session is 404, not 500 from FK.
    let _ = sessions::fetch(&state.pool, &session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| ToolError::NotFound(format!("session `{session_id}` not found")))?;

    let author_label = args
        .author_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| AgentLabel::from(s.to_string()));

    if let Some(label) = author_label.as_ref() {
        let now = chrono::Utc::now().timestamp();
        upsert_seen_with_event(state, &session_id, label, now).await?;
    }

    let ctx = state
        .lifecycle
        .build_feedback_context(&session_id)
        .await
        .map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))?;

    let now = chrono::Utc::now().timestamp();
    let response = render(&ctx, author_label.as_ref(), now);
    to_value(&response).map_err(|e| ToolError::Internal(anyhow::anyhow!(e)))
}

fn render(
    ctx: &FeedbackContext,
    author_label: Option<&AgentLabel>,
    now: i64,
) -> GetContextResponse {
    // Status is already pre-derived per-row by build_feedback_context
    // against the kind's expected target; we don't recompute here.

    let review_target = ctx
        .review_target
        .as_ref()
        .map(|t| review_target_view(t, ctx, now));

    let latest_plan_revision = ctx.latest_plan_revision.as_ref().map(|r| PlanRevisionView {
        id: r.id,
        number: r.revision_number,
        content_hash: r.content_hash.clone(),
        created_at: r.created_at,
        age_label: age_label(r.created_at, now),
    });

    let latest_implementation_revision =
        ctx.latest_implementation_revision
            .as_ref()
            .map(|r| ImplRevisionView {
                commit_sha: r.commit_sha.clone(),
                parent_sha: r.parent_sha.clone(),
                branch: r.branch.clone(),
                is_head: ctx.head_sha.as_deref() == Some(r.commit_sha.as_str()),
                worktree_dirty: ctx.worktree_dirty.unwrap_or(false),
                worktree_status_hash: ctx.worktree_status_hash.clone().unwrap_or_default(),
                created_at: r.created_at,
                age_label: age_label(r.created_at, now),
            });

    let write_kind = match ctx.phase {
        Phase::Planning => Some(FeedbackKind::Plan),
        Phase::Implementing => Some(FeedbackKind::Impl),
        Phase::NoActivePlan => None,
    };

    let write_feedback = match (author_label, write_kind) {
        (Some(label), Some(kind)) => {
            let rows = match kind {
                FeedbackKind::Plan => &ctx.plan_files,
                FeedbackKind::Impl => &ctx.impl_files,
            };
            let snap = rows.iter().find(|s| s.row.author_label == label.as_str());
            let conv =
                feedback_path::feedback_file_path(&ctx.repo_root, &ctx.session_id, kind, label);
            Some(view_for(kind, conv, snap, now))
        }
        _ => None,
    };

    let prior_feedback = author_label.map(|label| {
        let (prior_kind, prior_rows): (FeedbackKind, &Vec<FeedbackFileSnapshot>) = match ctx.phase {
            Phase::Implementing => (FeedbackKind::Plan, &ctx.plan_files),
            _ => {
                return PriorFeedback {
                    self_: None,
                    others: vec![],
                };
            }
        };
        let self_ = prior_rows
            .iter()
            .find(|s| s.row.author_label == label.as_str())
            .map(|s| view_for_existing(prior_kind, s, now));
        let others = prior_rows
            .iter()
            .filter(|s| s.row.author_label != label.as_str())
            .map(|s| pointer_for(prior_kind, s, now))
            .collect();
        PriorFeedback { self_, others }
    });

    let author_str = author_label.map(|l| l.as_str().to_string());
    let plan_pointers = ctx
        .plan_files
        .iter()
        .filter(|s| author_str.as_deref() != Some(s.row.author_label.as_str()))
        .map(|s| pointer_for(FeedbackKind::Plan, s, now))
        .collect();
    let impl_pointers = ctx
        .impl_files
        .iter()
        .filter(|s| author_str.as_deref() != Some(s.row.author_label.as_str()))
        .map(|s| pointer_for(FeedbackKind::Impl, s, now))
        .collect();

    GetContextResponse {
        schema_version: SCHEMA_VERSION,
        session_id: ctx.session_id.as_str().to_string(),
        repo_root: ctx.repo_root.clone(),
        plan_file_path: ctx.plan_file_path.clone(),
        git_logs_head_path: ctx.git_logs_head_path.clone(),
        phase: ctx.phase,
        review_target,
        latest_plan_revision,
        latest_implementation_revision,
        write_feedback,
        prior_feedback,
        other_feedback_files: GroupedFeedbackFiles {
            plan: plan_pointers,
            impl_: impl_pointers,
        },
    }
}

fn review_target_view(t: &ActiveTarget, ctx: &FeedbackContext, now: i64) -> ReviewTarget {
    let created_at = match t.kind {
        crate::domain::TargetKind::PlanRevision => {
            ctx.latest_plan_revision.as_ref().map(|r| r.created_at)
        }
        crate::domain::TargetKind::ImplementationCommit => ctx
            .latest_implementation_revision
            .as_ref()
            .map(|r| r.created_at),
    };
    ReviewTarget {
        kind: t.kind.as_str().to_string(),
        id: t.id.clone(),
        created_at,
        age_label: created_at.map(|ts| age_label(ts, now)),
    }
}

fn view_for(
    kind: FeedbackKind,
    conv_path: PathBuf,
    snap: Option<&FeedbackFileSnapshot>,
    now: i64,
) -> FeedbackFileView {
    match snap {
        Some(s) => view_for_existing(kind, s, now),
        None => FeedbackFileView {
            kind,
            path: conv_path.display().to_string(),
            exists: conv_path.exists(),
            status: FeedbackFileStatus::NotYetIngested,
            last_observed_hash: None,
            last_observed_at: None,
            last_observed_age_label: None,
            last_ingested_hash: None,
            last_ingested_at: None,
            last_ingested_age_label: None,
            last_ingested_target: None,
            parse_error: None,
        },
    }
}

fn view_for_existing(kind: FeedbackKind, s: &FeedbackFileSnapshot, now: i64) -> FeedbackFileView {
    FeedbackFileView {
        kind,
        path: s.row.path.clone(),
        exists: s.exists_on_disk,
        status: s.status,
        last_observed_hash: s.row.last_observed_hash.clone(),
        last_observed_at: s.row.last_observed_at,
        last_observed_age_label: s.row.last_observed_at.map(|ts| age_label(ts, now)),
        last_ingested_hash: s.row.last_ingested_hash.clone(),
        last_ingested_at: s.row.last_ingested_at,
        last_ingested_age_label: s.row.last_ingested_at.map(|ts| age_label(ts, now)),
        last_ingested_target: target_ref(s),
        parse_error: s.row.parse_error.clone(),
    }
}

fn pointer_for(kind: FeedbackKind, s: &FeedbackFileSnapshot, now: i64) -> FeedbackFilePointer {
    FeedbackFilePointer {
        author_label: s.row.author_label.clone(),
        kind,
        path: s.row.path.clone(),
        status: s.status,
        last_ingested_at: s.row.last_ingested_at,
        last_ingested_age_label: s.row.last_ingested_at.map(|ts| age_label(ts, now)),
        last_ingested_target: target_ref(s),
    }
}

fn target_ref(s: &FeedbackFileSnapshot) -> Option<TargetRef> {
    match (
        s.row.last_ingested_target_kind.as_deref(),
        s.row.last_ingested_target_id.as_deref(),
    ) {
        (Some(k), Some(id)) => Some(TargetRef {
            kind: k.to_string(),
            id: id.to_string(),
        }),
        _ => None,
    }
}

fn age_label(ts: i64, now: i64) -> String {
    let secs = (now - ts).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
