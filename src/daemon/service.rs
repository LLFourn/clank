//! Per-session active-plan cache + structural feedback operations.
//!
//! `SessionService` (renamed from `SessionLifecycle`) is the single
//! chokepoint for:
//! - lifecycle observations (`observe`) — feeds the sans-IO reducer +
//!   apply layer + commit + cache update;
//! - structural feedback writes (`put_feedback`) — upsert keyed on
//!   `(plan_id, target, author)` with no-op-on-identical-body semantics;
//! - master-agent reads (`current_feedback`) — snapshot-consistent
//!   listing + digest.
//!
//! Lifecycle and feedback share the same per-session async mutex.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::json;
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::domain::{EventKind, FeedbackTargetRef, TargetKind};
use crate::lifecycle::{
    ActivePlan, AgentLabel, Decision, LifecycleError, Observation, PlanFilePath, SessionId, decide,
};
use crate::storage::{
    agents, events as ev_store, feedback as feedback_store, feedback::FeedbackRecord,
    implementation_revisions as impl_revs, plan_revisions, plans, sessions,
};

use super::apply::{self, ApplyError, ApplyOutcome};

#[derive(Debug, thiserror::Error)]
pub enum LifecycleServiceError {
    #[error("reducer rejected observation: {0}")]
    Reducer(#[from] LifecycleError),
    #[error("apply layer failed: {0}")]
    Apply(#[from] ApplyError),
    #[error("session `{0}` not found")]
    NoSession(String),
    #[error("active plan invariant violated: {0}")]
    ActivePlan(#[from] plans::ActivePlanInconsistency),
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("session `{0}` has no active plan; feedback rejected")]
    NoActivePlanForFeedback(String),
    #[error("feedback target {target} does not belong to active plan {active_plan_id}")]
    FeedbackTargetNotInActivePlan { target: String, active_plan_id: i64 },
    #[error("feedback body is empty")]
    EmptyFeedbackBody,
    #[error("session `{0}` not found")]
    NoSession(String),
    #[error("active plan invariant violated: {0}")]
    ActivePlan(#[from] plans::ActivePlanInconsistency),
    #[error("feedback row decode failed: {0}")]
    Decode(#[from] feedback_store::DecodeError),
    #[error("feedback storage: {0}")]
    Feedback(#[from] feedback_store::Error),
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct ObservationOutcome {
    pub decision: Decision,
    pub apply: ApplyOutcome,
}

/// HEAD-derived current review target. `id` is the plan_revision id
/// (as a string) for `PlanRevision`, or the full commit SHA for
/// `ImplementationCommit`.
#[derive(Debug, Clone)]
pub struct ActiveTarget {
    pub plan_id: i64,
    pub kind: TargetKind,
    pub id: String,
}

/// Outcome of a `put_feedback` call.
#[derive(Debug, Clone)]
pub struct FeedbackUpsertOutcome {
    pub record: FeedbackRecord,
    /// `true` on first call for `(plan_id, target, author)`; emits
    /// `feedback_added`.
    pub was_insert: bool,
    /// `true` if the call carried byte-identical body to the existing row.
    /// No audit event was appended; `updated_at` was not bumped.
    pub was_no_op: bool,
}

/// Master read shape for `get_current_feedback`. `feedback_digest` is over
/// the returned rows + `plan_id` + filter; recomputed on every call,
/// never persisted.
#[derive(Debug, Clone)]
pub enum CurrentFeedbackView {
    NoActivePlan {
        session_id: SessionId,
    },
    Active {
        session_id: SessionId,
        plan_id: i64,
        filter: Option<TargetKind>,
        feedback_digest: String,
        feedback: Vec<FeedbackRecord>,
    },
}

pub struct SessionService {
    pool: SqlitePool,
    watcher: Arc<super::watcher::Watcher>,
    locks: Mutex<HashMap<SessionId, Arc<Mutex<Option<ActivePlan>>>>>,
}

impl SessionService {
    pub fn new(pool: SqlitePool, watcher: Arc<super::watcher::Watcher>) -> Self {
        Self {
            pool,
            watcher,
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn watcher(&self) -> &super::watcher::Watcher {
        &self.watcher
    }

    /// Compute the session's current active review target. For
    /// planning plans this is the latest plan_revision; for implementing
    /// plans it HEAD-derives — preferring the row that matches current
    /// `git HEAD`, falling back to the latest impl row when HEAD isn't
    /// in the table (mid-amend / unobserved reset). Returns `None`
    /// when the session has no active plan.
    ///
    /// Used by `get_context`, the feedback ingest dispatcher, and the
    /// web UI so a reset-to-older-SHA doesn't leave the three surfaces
    /// disagreeing about what's currently under review.
    pub async fn resolve_active_target(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ActiveTarget>, LifecycleServiceError> {
        let session = match sessions::fetch(&self.pool, session_id).await? {
            Some(s) => s,
            None => return Err(LifecycleServiceError::NoSession(session_id.as_str().into())),
        };
        let Some(active_plan_id) = session.active_plan_id else {
            return Ok(None);
        };
        let plan = match plans::fetch(&self.pool, active_plan_id).await? {
            Some(p) => p,
            None => return Ok(None),
        };
        if plan.state == "implementing" {
            let head_sha = super::git::rev_parse_head(std::path::Path::new(&session.repo_root))
                .await
                .ok();
            let head_match = match head_sha.as_deref() {
                Some(h) => impl_revs::fetch_by_sha(
                    &self.pool,
                    active_plan_id,
                    &crate::lifecycle::CommitSha::from(h.to_string()),
                )
                .await?
                .map(|r| r.commit_sha),
                None => None,
            };
            let chosen = match head_match {
                Some(sha) => Some(sha),
                None => impl_revs::latest_for_plan(&self.pool, active_plan_id)
                    .await?
                    .map(|r| r.commit_sha),
            };
            Ok(chosen.map(|sha| ActiveTarget {
                plan_id: active_plan_id,
                kind: TargetKind::ImplementationCommit,
                id: sha,
            }))
        } else if plan.state == "planning" {
            Ok(plan_revisions::latest_for_plan(&self.pool, active_plan_id)
                .await?
                .map(|rev| ActiveTarget {
                    plan_id: active_plan_id,
                    kind: TargetKind::PlanRevision,
                    id: rev.id.to_string(),
                }))
        } else {
            Ok(None)
        }
    }

    async fn lock_for(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<Mutex<Option<ActivePlan>>>, LifecycleServiceError> {
        let mut map = self.locks.lock().await;
        if let Some(existing) = map.get(session_id) {
            return Ok(Arc::clone(existing));
        }
        let initial = self.load_initial(session_id).await?;
        let cell = Arc::new(Mutex::new(initial));
        map.insert(session_id.clone(), Arc::clone(&cell));
        Ok(cell)
    }

    async fn load_initial(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<ActivePlan>, LifecycleServiceError> {
        let session = sessions::fetch(&self.pool, session_id)
            .await?
            .ok_or_else(|| LifecycleServiceError::NoSession(session_id.as_str().into()))?;
        if session.active_plan_id.is_none() {
            return Ok(None);
        }
        match plans::load_active_plan(&self.pool, session_id).await {
            Ok(Some(loaded)) => Ok(Some(loaded.to_active_plan())),
            Ok(None) => Ok(None),
            Err(plans::LoadActivePlanError::Sql(e)) => Err(e.into()),
            Err(plans::LoadActivePlanError::Inconsistent(inc)) => Err(inc.into()),
        }
    }

    pub async fn seed(&self, session_id: &SessionId) -> Result<(), LifecycleServiceError> {
        let initial = self.load_initial(session_id).await?;
        let cell = Arc::new(Mutex::new(initial));
        self.locks.lock().await.insert(session_id.clone(), cell);
        Ok(())
    }

    /// Lifecycle chokepoint. Reducer + apply + commit + cache update all
    /// under the per-session lock.
    pub async fn observe(
        &self,
        session_id: &SessionId,
        actor: &str,
        obs: Observation,
    ) -> Result<ObservationOutcome, LifecycleServiceError> {
        let cell = self.lock_for(session_id).await?;
        let mut state = cell.lock().await;
        let current = state.clone();

        let path_for_update: Option<PlanFilePath> = match &obs {
            Observation::PlanRegistered { path, .. } => Some(path.clone()),
            _ => None,
        };

        let decision = decide(current, obs)?;

        let now = chrono::Utc::now().timestamp();
        let mut tx = self.pool.begin().await?;
        if let Some(path) = &path_for_update {
            sessions::set_plan_file_path(&mut *tx, session_id, path, now).await?;
        }
        let apply_outcome =
            apply::apply_decision(&mut tx, session_id, &decision, actor, now).await?;
        tx.commit().await?;

        if let Some(path) = &path_for_update {
            self.watcher.switch_plan_file(session_id, path);
        }

        *state = decision.new_active.clone();
        Ok(ObservationOutcome {
            decision,
            apply: apply_outcome,
        })
    }

    /// Upsert reviewer feedback for `(active plan, target, author)`.
    ///
    /// Steps under the per-session lock:
    /// 1. resolve `active_plan_id`;
    /// 2. validate `target` belongs to the active plan;
    /// 3. SELECT existing row by natural key;
    /// 4. INSERT / UPDATE / no-op + append audit event;
    /// 5. commit (skip commit on no-op).
    pub async fn put_feedback(
        &self,
        session_id: &SessionId,
        author: &AgentLabel,
        target: FeedbackTargetRef,
        body: String,
    ) -> Result<FeedbackUpsertOutcome, ServiceError> {
        if body.trim().is_empty() {
            return Err(ServiceError::EmptyFeedbackBody);
        }

        let cell = match self.lock_for(session_id).await {
            Ok(c) => c,
            Err(LifecycleServiceError::NoSession(s)) => return Err(ServiceError::NoSession(s)),
            Err(LifecycleServiceError::ActivePlan(inc)) => return Err(inc.into()),
            Err(LifecycleServiceError::Sql(e)) => return Err(e.into()),
            Err(LifecycleServiceError::Reducer(_)) | Err(LifecycleServiceError::Apply(_)) => {
                unreachable!("lock_for never returns these")
            }
        };
        let _guard = cell.lock().await;

        // Resolve the active plan through the invariant helper, under the
        // per-session lock. `load_active_plan` rejects archived /
        // cross-session / dangling pointers.
        let active = match plans::load_active_plan(&self.pool, session_id).await {
            Ok(Some(p)) => p,
            Ok(None) => {
                return Err(ServiceError::NoActivePlanForFeedback(
                    session_id.as_str().into(),
                ));
            }
            Err(plans::LoadActivePlanError::Sql(e)) => return Err(e.into()),
            Err(plans::LoadActivePlanError::Inconsistent(inc)) => return Err(inc.into()),
        };
        let active_plan_id = active.id;

        // Validate target belongs to the active plan.
        match &target {
            FeedbackTargetRef::PlanRevision(revision_id) => {
                let rev = plan_revisions::fetch(&self.pool, *revision_id).await?;
                if rev.map(|r| r.plan_id) != Some(active_plan_id) {
                    return Err(ServiceError::FeedbackTargetNotInActivePlan {
                        target: target.to_string(),
                        active_plan_id,
                    });
                }
            }
            FeedbackTargetRef::ImplementationCommit(sha) => {
                let row = impl_revs::fetch_by_sha(&self.pool, active_plan_id, sha).await?;
                if row.is_none() {
                    return Err(ServiceError::FeedbackTargetNotInActivePlan {
                        target: target.to_string(),
                        active_plan_id,
                    });
                }
            }
        }

        let kind = target.kind();
        let target_id = target.target_id_string();
        let author_label = author.as_str().to_string();

        let now = chrono::Utc::now().timestamp();
        let mut tx = self.pool.begin().await?;

        // Upsert agents.last_seen + emit one agent_joined on first-sight.
        // Atomic with the feedback write: on a no-op or validation failure
        // upstream we've already returned; on a transaction roll-back the
        // agent row stays consistent with the feedback row.
        let seen = agents::upsert_seen_tx(&mut tx, session_id, author, now).await?;
        if matches!(seen, agents::SeenOutcome::Inserted) {
            ev_store::append(
                &mut *tx,
                session_id,
                None,
                None,
                None,
                EventKind::AgentJoined.as_str(),
                &format!("agent:{}", author.as_str()),
                &json!({ "label": author.as_str() }),
                None,
                now,
            )
            .await?;
        }

        let existing = feedback_store::find_by_natural_key(
            &mut *tx,
            active_plan_id,
            kind,
            &target_id,
            &author_label,
        )
        .await?;

        let (outcome, committed) = match existing {
            None => {
                let row = feedback_store::insert(
                    &mut *tx,
                    session_id,
                    active_plan_id,
                    kind,
                    &target_id,
                    &author_label,
                    &body,
                    now,
                )
                .await?;
                let feedback_id = row.id;
                let record = row.into_record()?;
                ev_store::append(
                    &mut *tx,
                    session_id,
                    Some(active_plan_id),
                    Some(kind.as_str()),
                    Some(&target_id),
                    EventKind::FeedbackAdded.as_str(),
                    &format!("agent:{}", author.as_str()),
                    &json!({ "feedback_id": feedback_id }),
                    None,
                    now,
                )
                .await?;
                sessions::touch_updated_at(&mut *tx, session_id, now).await?;
                (
                    FeedbackUpsertOutcome {
                        record,
                        was_insert: true,
                        was_no_op: false,
                    },
                    true,
                )
            }
            Some(prior) if prior.body == body => (
                FeedbackUpsertOutcome {
                    record: prior.into_record()?,
                    was_insert: false,
                    was_no_op: true,
                },
                false,
            ),
            Some(prior) => {
                let feedback_id = prior.id;
                let prior_body = prior.body.clone();
                let row = feedback_store::update_body(&mut *tx, feedback_id, &body, now).await?;
                let record = row.into_record()?;
                ev_store::append(
                    &mut *tx,
                    session_id,
                    Some(active_plan_id),
                    Some(kind.as_str()),
                    Some(&target_id),
                    EventKind::FeedbackUpdated.as_str(),
                    &format!("agent:{}", author.as_str()),
                    &json!({ "feedback_id": feedback_id, "prior_body": prior_body }),
                    None,
                    now,
                )
                .await?;
                sessions::touch_updated_at(&mut *tx, session_id, now).await?;
                (
                    FeedbackUpsertOutcome {
                        record,
                        was_insert: false,
                        was_no_op: false,
                    },
                    true,
                )
            }
        };

        // Always commit so the agents upsert above lands even on a
        // body-no-op. (The `committed` flag predates the agents upsert;
        // before it, a no-op had nothing to write, so we'd drop the tx.)
        let _ = committed;
        tx.commit().await?;

        Ok(outcome)
    }

    /// Master read. Snapshot-consistent: resolves the active plan and
    /// fetches its feedback inside a deferred read transaction so a
    /// concurrent archive cannot stitch the two reads together. Also
    /// enforces the active-plan invariant (archived / cross-session /
    /// dangling pointers fail loudly).
    pub async fn current_feedback(
        &self,
        session_id: &SessionId,
        filter: Option<TargetKind>,
    ) -> Result<CurrentFeedbackView, ServiceError> {
        let mut tx = self.pool.begin().await?;

        // Confirm the session exists vs. distinguishing "no active plan".
        let row: Option<Option<i64>> =
            sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = ?")
                .bind(session_id.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        let active_plan_id = match row {
            None => return Err(ServiceError::NoSession(session_id.as_str().into())),
            Some(None) => {
                return Ok(CurrentFeedbackView::NoActivePlan {
                    session_id: session_id.clone(),
                });
            }
            Some(Some(id)) => id,
        };

        // Active-plan invariant check (must run in the same tx so the
        // snapshot is consistent with the feedback list below).
        let plan_row: Option<(String, String)> =
            sqlx::query_as("SELECT state, session_id FROM plans WHERE id = ?")
                .bind(active_plan_id)
                .fetch_optional(&mut *tx)
                .await?;
        match plan_row {
            None => {
                return Err(plans::ActivePlanInconsistency::Dangling {
                    session_id: session_id.as_str().into(),
                    active_plan_id,
                }
                .into());
            }
            Some((state, _)) if state == "archived" => {
                return Err(plans::ActivePlanInconsistency::Archived {
                    session_id: session_id.as_str().into(),
                    active_plan_id,
                }
                .into());
            }
            Some((_, plan_session)) if plan_session != session_id.as_str() => {
                return Err(plans::ActivePlanInconsistency::CrossSession {
                    session_id: session_id.as_str().into(),
                    active_plan_id,
                    plan_session_id: plan_session,
                }
                .into());
            }
            Some(_) => { /* Active plan invariant holds; nothing more to assert. */ }
        }

        let feedback = feedback_store::list_for_plan(&mut *tx, active_plan_id, filter).await?;
        // Read-only tx; explicit rollback for clarity.
        let _ = tx.rollback().await;
        let digest = compute_digest(active_plan_id, filter, &feedback);

        Ok(CurrentFeedbackView::Active {
            session_id: session_id.clone(),
            plan_id: active_plan_id,
            filter,
            feedback_digest: digest,
            feedback,
        })
    }

    pub async fn peek(&self, session_id: &SessionId) -> Option<ActivePlan> {
        let cell = match self.lock_for(session_id).await {
            Ok(c) => c,
            Err(_) => return None,
        };
        cell.lock().await.clone()
    }
}

/// Canonical, deterministic digest preimage. See plan §"Read model".
fn compute_digest(plan_id: i64, filter: Option<TargetKind>, feedback: &[FeedbackRecord]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"trinity-current-feedback-v1");
    hasher.update(&plan_id.to_le_bytes());
    let filter_byte: u8 = match filter {
        None => 0,
        Some(TargetKind::PlanRevision) => 1,
        Some(TargetKind::ImplementationCommit) => 2,
    };
    hasher.update(&[filter_byte]);
    let row_count = u32::try_from(feedback.len()).unwrap_or(u32::MAX);
    hasher.update(&row_count.to_le_bytes());
    for row in feedback {
        hasher.update(&row.id.to_le_bytes());
        hasher.update(&[row.target.kind().digest_byte()]);
        let tid = row.target.target_id_string();
        let tid_bytes = tid.as_bytes();
        hasher.update(&(tid_bytes.len() as u32).to_le_bytes());
        hasher.update(tid_bytes);
        let author = row.author_label.as_str().as_bytes();
        hasher.update(&(author.len() as u32).to_le_bytes());
        hasher.update(author);
        let body_hash = blake3::hash(row.body.as_bytes());
        hasher.update(body_hash.as_bytes());
        hasher.update(&row.updated_at.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}
