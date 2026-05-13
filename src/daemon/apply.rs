//! Translate session lifecycle effects into SQL writes inside a single transaction.
//! Returns generated IDs the tool layer needs for response bodies.
//!
//! Feedback is **not** an effect; it is handled by
//! `SessionService::put_feedback` as structural state.

use serde_json::json;
use sqlx::{Sqlite, Transaction};

use crate::domain::{EventKind, TargetKind};
use crate::lifecycle::{CommitSha, Decision, Effect, SessionId};
use crate::review_state::ReviewPhase;
use crate::storage::{
    events as ev_store, implementation_revisions as impl_revs, plan_revisions, plans,
    repo_effective_sessions, review_gate_overrides, sessions,
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
    /// HEAD was observed at a SHA already known for this plan
    /// (e.g. `git reset --hard <earlier-sha>` or HEAD bouncing back
    /// to the latest impl). No new `implementation_revisions` row;
    /// a `head_reset_to_known_sha` audit event was appended.
    HeadResetToKnownSha {
        plan_id: i64,
        commit_sha: CommitSha,
    },
    Archived {
        plan_id: i64,
    },
    Finished {
        plan_id: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionSideEffect {
    AttachSessionWatchers,
    ScanFeedbackDirs { force_reingest: bool },
}

#[derive(Debug, Clone)]
pub struct ApplyOutcome {
    pub items: Vec<AppliedEffect>,
    pub side_effects: Vec<SessionSideEffect>,
}

impl ApplyOutcome {
    pub fn empty() -> Self {
        Self {
            items: Vec::new(),
            side_effects: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("active plan invariant violated: {0}")]
    ActivePlan(#[from] plans::ActivePlanInconsistency),
    #[error("apply_session_effects invariant: ArchiveActivePlan with no active plan in session")]
    ArchiveWithoutActive,
    #[error(
        "apply_session_effects invariant: RecordPlanRevision/RecordImplementation with no active plan in session"
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
pub async fn apply_session_effects(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    decision: &Decision,
    actor: &str,
    now: i64,
) -> Result<ApplyOutcome, ApplyError> {
    let mut items = Vec::with_capacity(decision.effects.len());
    let mut side_effects = Vec::new();

    let mut active_plan_id = sessions::active_plan_rowid(&mut **tx, session_id, true).await?;

    for effect in &decision.effects {
        match effect {
            Effect::StartPlan {
                base_commit,
                initial_body,
                source,
            } => {
                let prior_plan_count: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM plans WHERE session_id = ?")
                        .bind(session_id.as_str())
                        .fetch_one(&mut **tx)
                        .await?;
                let event_floor = if prior_plan_count > 0 {
                    sqlx::query_scalar(
                        "SELECT COALESCE(MAX(id), 0) FROM events WHERE session_id = ?",
                    )
                    .bind(session_id.as_str())
                    .fetch_one(&mut **tx)
                    .await?
                } else {
                    0
                };
                let plan_id = plans::insert(&mut **tx, session_id, base_commit, now).await?;
                if prior_plan_count > 0 {
                    sessions::set_current_event_floor(&mut **tx, session_id, event_floor).await?;
                }
                let hash = plan_revisions::compute_content_hash(initial_body);
                let (plan_revision_id, _revision_number) =
                    plan_revisions::append(tx, plan_id, hash.as_str(), initial_body, now).await?;
                review_gate_overrides::delete_phase(&mut **tx, session_id, ReviewPhase::Plan)
                    .await?;
                review_gate_overrides::delete_phase(&mut **tx, session_id, ReviewPhase::Impl)
                    .await?;
                active_plan_id = Some(plan_id);
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    Some(TargetKind::PlanRevision.as_str()),
                    Some(&plan_revision_id.to_string()),
                    EventKind::PlanRevisionCreated.as_str(),
                    actor,
                    &json!({
                        "home_row_projection": if prior_plan_count == 0
                            || matches!(source, crate::lifecycle::ActivationSource::AutoRegisterReactivation)
                        {
                            "insert"
                        } else {
                            "replace"
                        },
                        "activation_source": source.as_str(),
                    }),
                    None,
                    now,
                )
                .await?;
                items.push(AppliedEffect::Started {
                    plan_id,
                    plan_revision_id,
                });
                if repo_effective_sessions::claim_if_unset(tx, session_id, actor, now).await? {
                    ev_store::append(
                        &mut **tx,
                        session_id,
                        Some(plan_id),
                        None,
                        None,
                        EventKind::SessionClaimed.as_str(),
                        actor,
                        &json!({"mode": "auto_if_unset"}),
                        None,
                        now,
                    )
                    .await?;
                }
                side_effects.push(SessionSideEffect::AttachSessionWatchers);
                side_effects.push(SessionSideEffect::ScanFeedbackDirs {
                    force_reingest: matches!(
                        source,
                        crate::lifecycle::ActivationSource::AutoRegisterReactivation
                    ),
                });
            }
            Effect::RecordPlanRevision { body } => {
                let plan_id = active_plan_id.ok_or(ApplyError::EffectWithoutActive)?;
                let hash = plan_revisions::compute_content_hash(body);
                let (plan_revision_id, revision_number) =
                    plan_revisions::append(tx, plan_id, hash.as_str(), body, now).await?;
                review_gate_overrides::delete_phase(&mut **tx, session_id, ReviewPhase::Plan)
                    .await?;
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

                // SHA-known check (Codex round-2 P1 #1 + round-3 P1 #1):
                // if this SHA was already observed for this plan, treat as
                // a reset to a known SHA — audit only, no INSERT, no state
                // change. `get_context.active_target` resolves the
                // implementation target by matching git HEAD against the
                // table, so the read path will correctly point at the
                // reset target without us having to update any row here.
                let exists: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM implementation_revisions ir \
                     JOIN sessions s ON s.id = ir.session_id \
                     WHERE s.rowid = ? AND ir.commit_sha = ?",
                )
                .bind(plan_id)
                .bind(commit.sha.as_str())
                .fetch_optional(&mut **tx)
                .await?;
                if exists.is_some() {
                    ev_store::append(
                        &mut **tx,
                        session_id,
                        Some(plan_id),
                        Some(TargetKind::ImplementationCommit.as_str()),
                        Some(commit.sha.as_str()),
                        EventKind::HeadResetToKnownSha.as_str(),
                        actor,
                        &json!({
                            "branch": commit.branch,
                            "is_head": commit.is_head,
                        }),
                        None,
                        now,
                    )
                    .await?;
                    items.push(AppliedEffect::HeadResetToKnownSha {
                        plan_id,
                        commit_sha: commit.sha.clone(),
                    });
                    continue;
                }

                let implementation_revision_id =
                    impl_revs::append(&mut **tx, plan_id, commit, actor, now).await?;
                review_gate_overrides::delete_phase(&mut **tx, session_id, ReviewPhase::Impl)
                    .await?;
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
            Effect::IgnoreSealedPlanEdit => {}
            Effect::ArchiveActivePlan => {
                let plan_id = active_plan_id.ok_or(ApplyError::ArchiveWithoutActive)?;
                let from_state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
                    .bind(plan_id)
                    .fetch_one(&mut **tx)
                    .await?;
                plans::archive(&mut **tx, plan_id, now).await?;
                repo_effective_sessions::clear_session(tx, session_id).await?;
                // Feedback rows for the archived plan are left in place
                // (audit history). The read API resolves current phase
                // targets from session state, so archived feedback is
                // invisible without any cascade write.
                active_plan_id = None;
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    None,
                    None,
                    EventKind::StateTransition.as_str(),
                    actor,
                    &json!({"from": from_state, "to": "archived"}),
                    None,
                    now,
                )
                .await?;
                items.push(AppliedEffect::Archived { plan_id });
            }
            Effect::FinishActivePlan => {
                let plan_id = active_plan_id.ok_or(ApplyError::ArchiveWithoutActive)?;
                let from_state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
                    .bind(plan_id)
                    .fetch_one(&mut **tx)
                    .await?;
                plans::finish(&mut **tx, plan_id, now).await?;
                repo_effective_sessions::clear_session(tx, session_id).await?;
                active_plan_id = None;
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    None,
                    None,
                    EventKind::StateTransition.as_str(),
                    actor,
                    &json!({"from": from_state, "to": "finished"}),
                    None,
                    now,
                )
                .await?;
                ev_store::append(
                    &mut **tx,
                    session_id,
                    Some(plan_id),
                    None,
                    None,
                    EventKind::PlanFinished.as_str(),
                    actor,
                    &json!({"plan_id": plan_id, "from_state": from_state}),
                    None,
                    now,
                )
                .await?;
                items.push(AppliedEffect::Finished { plan_id });
            }
        }
    }

    Ok(ApplyOutcome {
        items,
        side_effects,
    })
}
