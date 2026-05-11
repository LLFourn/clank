//! Translate `Decision` effects into SQL writes inside a single transaction.
//! Returns generated IDs the tool layer needs for response bodies.
//!
//! Feedback is **not** an effect; it is handled by
//! `SessionService::put_feedback` as structural state.

use serde_json::json;
use sqlx::{Sqlite, Transaction};

use crate::domain::{EventKind, TargetKind};
use crate::lifecycle::{CommitSha, Decision, Effect, SessionId};
use crate::storage::{
    events as ev_store, implementation_revisions as impl_revs, plan_revisions, plans, sessions,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedEffect {
    Started {
        plan_id: i64,
        plan_revision_id: i64,
    },
    PlanRevision {
        plan_id: i64,
        plan_revision_id: i64,
        revision_number: i64,
    },
    Implementation {
        plan_id: i64,
        implementation_revision_id: i64,
        commit_sha: CommitSha,
    },
    Archived {
        plan_id: i64,
    },
}

#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub items: Vec<AppliedEffect>,
}

impl ApplyOutcome {
    pub fn empty() -> Self {
        Self { items: Vec::new() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("active plan invariant violated: {0}")]
    ActivePlan(#[from] plans::ActivePlanInconsistency),
    #[error("apply_decision invariant: ArchiveActivePlan with no active plan in session")]
    ArchiveWithoutActive,
    #[error(
        "apply_decision invariant: RecordPlanRevision/RecordImplementation with no active plan in session"
    )]
    EffectWithoutActive,
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
}

/// Apply a `Decision` against the session row. `actor` is the
/// caller-supplied attribution string (e.g. `"agent:claude-main"` or
/// `"system:watcher"`) and rides on each emitted `events.actor`.
///
/// Effects are applied in order. Generated IDs flow into the returned
/// `ApplyOutcome` aligned by index with `decision.effects`.
pub async fn apply_decision(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    decision: &Decision,
    actor: &str,
    now: i64,
) -> Result<ApplyOutcome, ApplyError> {
    let mut items = Vec::with_capacity(decision.effects.len());

    let mut active_plan_id: Option<i64> =
        sqlx::query_scalar::<_, Option<i64>>("SELECT active_plan_id FROM sessions WHERE id = ?")
            .bind(session_id.as_str())
            .fetch_one(&mut **tx)
            .await?;

    for effect in &decision.effects {
        match effect {
            Effect::StartPlan {
                base_commit,
                initial_body,
            } => {
                let plan_id = plans::insert(&mut **tx, session_id, base_commit, now).await?;
                let hash = plan_revisions::compute_content_hash(initial_body);
                let (plan_revision_id, _revision_number) =
                    plan_revisions::append(tx, plan_id, hash.as_str(), initial_body, now).await?;
                sessions::set_active_plan_id(&mut **tx, session_id, Some(plan_id), now).await?;
                active_plan_id = Some(plan_id);
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    Some(TargetKind::PlanRevision.as_str()),
                    Some(&plan_revision_id.to_string()),
                    EventKind::PlanRevisionCreated.as_str(),
                    actor,
                    &json!({}),
                    None,
                    now,
                )
                .await?;
                items.push(AppliedEffect::Started {
                    plan_id,
                    plan_revision_id,
                });
            }
            Effect::RecordPlanRevision { body } => {
                let plan_id = active_plan_id.ok_or(ApplyError::EffectWithoutActive)?;
                let hash = plan_revisions::compute_content_hash(body);
                let (plan_revision_id, revision_number) =
                    plan_revisions::append(tx, plan_id, hash.as_str(), body, now).await?;
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    Some(TargetKind::PlanRevision.as_str()),
                    Some(&plan_revision_id.to_string()),
                    EventKind::PlanRevisionCreated.as_str(),
                    actor,
                    &json!({}),
                    None,
                    now,
                )
                .await?;
                sessions::touch_updated_at(&mut **tx, session_id, now).await?;
                items.push(AppliedEffect::PlanRevision {
                    plan_id,
                    plan_revision_id,
                    revision_number,
                });
            }
            Effect::RecordImplementation { commit } => {
                let plan_id = active_plan_id.ok_or(ApplyError::EffectWithoutActive)?;
                let implementation_revision_id =
                    impl_revs::append(&mut **tx, plan_id, commit, actor, now).await?;
                let state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
                    .bind(plan_id)
                    .fetch_one(&mut **tx)
                    .await?;
                if state == "planning" {
                    plans::set_state(&mut **tx, plan_id, "implementing").await?;
                    ev_store::append(
                        &mut **tx,
                        session_id,
                        Some(plan_id),
                        None,
                        None,
                        EventKind::StateTransition.as_str(),
                        actor,
                        &json!({"from": "planning", "to": "implementing"}),
                        None,
                        now,
                    )
                    .await?;
                }
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    Some(TargetKind::ImplementationCommit.as_str()),
                    Some(commit.sha.as_str()),
                    EventKind::ImplRevisionCreated.as_str(),
                    actor,
                    &json!({
                        "branch": commit.branch,
                        "is_head": commit.is_head,
                    }),
                    None,
                    now,
                )
                .await?;
                if let Some(porcelain) = commit
                    .worktree_status
                    .as_deref()
                    .filter(|s| !s.trim().is_empty() && *s != "clean")
                {
                    ev_store::append(
                        &mut **tx,
                        session_id,
                        Some(plan_id),
                        Some(TargetKind::ImplementationCommit.as_str()),
                        Some(commit.sha.as_str()),
                        EventKind::DirtyWorktreeWarning.as_str(),
                        actor,
                        &json!({"porcelain": porcelain}),
                        None,
                        now,
                    )
                    .await?;
                }
                sessions::touch_updated_at(&mut **tx, session_id, now).await?;
                items.push(AppliedEffect::Implementation {
                    plan_id,
                    implementation_revision_id,
                    commit_sha: commit.sha.clone(),
                });
            }
            Effect::ArchiveActivePlan => {
                let plan_id = active_plan_id.ok_or(ApplyError::ArchiveWithoutActive)?;
                plans::archive(&mut **tx, plan_id, now).await?;
                // Feedback rows for the archived plan are left in place
                // (audit history). `get_current_feedback` filters by
                // `plan_id = active_plan_id`, so archived feedback is
                // invisible to the read API without any cascade write.
                sessions::set_active_plan_id(&mut **tx, session_id, None, now).await?;
                active_plan_id = None;
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    None,
                    None,
                    EventKind::StateTransition.as_str(),
                    actor,
                    &json!({"to": "archived"}),
                    None,
                    now,
                )
                .await?;
                items.push(AppliedEffect::Archived { plan_id });
            }
        }
    }

    Ok(ApplyOutcome { items })
}
