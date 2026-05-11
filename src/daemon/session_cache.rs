//! Per-session active-plan cache and the single chokepoint
//! `SessionLifecycle::observe` that funnels every lifecycle mutation through
//! decide → apply → commit → cache update under a per-session async mutex.

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::lifecycle::{
    ActivePlan, Decision, FeedbackTarget, LifecycleError, Observation, PlanFilePath, SessionId,
    decide,
};
use crate::storage::{implementation_revisions as impl_revs, plan_revisions, plans, sessions};

use super::apply::{self, ApplyError, ApplyOutcome};

#[derive(Debug, thiserror::Error)]
pub enum LifecycleServiceError {
    #[error("reducer rejected observation: {0}")]
    Reducer(#[from] LifecycleError),
    #[error("apply layer failed: {0}")]
    Apply(#[from] ApplyError),
    #[error("session `{0}` not found")]
    NoSession(String),
    #[error(
        "feedback target {target} does not belong to the active plan for session `{session_id}`"
    )]
    TargetNotInActivePlan { session_id: String, target: String },
    #[error("session `{0}` has no active plan; feedback rejected")]
    NoActivePlanForFeedback(String),
    #[error("active plan invariant violated: {0}")]
    ActivePlan(#[from] plans::ActivePlanInconsistency),
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct ObservationOutcome {
    pub decision: Decision,
    pub apply: ApplyOutcome,
}

pub struct SessionLifecycle {
    pool: SqlitePool,
    watcher: Arc<super::watcher::PlanWatcher>,
    /// Outer mutex guards lookup/insertion of the per-session inner mutex.
    /// Inner mutex holds the cached `Option<ActivePlan>` and is held across
    /// decide+apply+commit+cache for that session.
    locks: Mutex<HashMap<SessionId, Arc<Mutex<Option<ActivePlan>>>>>,
}

impl SessionLifecycle {
    pub fn new(pool: SqlitePool, watcher: Arc<super::watcher::PlanWatcher>) -> Self {
        Self {
            pool,
            watcher,
            locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn watcher(&self) -> &super::watcher::PlanWatcher {
        &self.watcher
    }

    /// Get or create the per-session lock, populating its initial value from
    /// SQL the first time. Returns an `Arc<Mutex<Option<ActivePlan>>>` ready
    /// to lock.
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

    /// Force a fresh load from SQL (used at startup to seed all sessions
    /// without going through `lock_for` for each).
    pub async fn seed(&self, session_id: &SessionId) -> Result<(), LifecycleServiceError> {
        let initial = self.load_initial(session_id).await?;
        let cell = Arc::new(Mutex::new(initial));
        self.locks.lock().await.insert(session_id.clone(), cell);
        Ok(())
    }

    /// The chokepoint. Every lifecycle mutation flows through here so the
    /// reducer + apply + DB + cache + watcher all stay consistent.
    ///
    /// For `Observation::FeedbackPosted`, validates *under the per-session
    /// lock* that the target belongs to the session's current active plan.
    /// This closes the race where a plan-file edit between the caller's
    /// pre-validation and the lifecycle apply could archive the plan the
    /// caller validated against.
    pub async fn observe(
        &self,
        session_id: &SessionId,
        actor: &str,
        obs: Observation,
    ) -> Result<ObservationOutcome, LifecycleServiceError> {
        let cell = self.lock_for(session_id).await?;
        let mut state = cell.lock().await;
        let current = state.clone();

        // PlanRegistered also updates sessions.plan_file_path in the same tx.
        let path_for_update: Option<PlanFilePath> = match &obs {
            Observation::PlanRegistered { path, .. } => Some(path.clone()),
            _ => None,
        };

        // For feedback, the target must belong to the *currently active* plan.
        // Validating under the lock means a concurrent observe() cannot archive
        // the plan between validation and apply.
        if let Observation::FeedbackPosted { target, .. } = &obs {
            self.validate_feedback_target(session_id, target).await?;
        }

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
            // Re-point the watcher only after commit. PlanWatcher::switch is idempotent.
            self.watcher.switch(session_id, path);
        }

        *state = decision.new_active.clone();
        Ok(ObservationOutcome {
            decision,
            apply: apply_outcome,
        })
    }

    /// Verify that a `FeedbackTarget` belongs to `sessions.active_plan_id`.
    /// Called inside `observe` while holding the per-session lock, so the
    /// active plan can't be archived out from under us between validation
    /// and apply.
    async fn validate_feedback_target(
        &self,
        session_id: &SessionId,
        target: &FeedbackTarget,
    ) -> Result<(), LifecycleServiceError> {
        let active_plan_id: Option<i64> =
            sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = ?")
                .bind(session_id.as_str())
                .fetch_one(&self.pool)
                .await?;
        let plan_id = active_plan_id.ok_or_else(|| {
            LifecycleServiceError::NoActivePlanForFeedback(session_id.as_str().into())
        })?;
        match target {
            FeedbackTarget::PlanRevision { revision_id } => {
                let rev = plan_revisions::fetch(&self.pool, *revision_id).await?;
                match rev {
                    Some(r) if r.plan_id == plan_id => Ok(()),
                    _ => Err(LifecycleServiceError::TargetNotInActivePlan {
                        session_id: session_id.as_str().into(),
                        target: format!("plan_revision {revision_id}"),
                    }),
                }
            }
            FeedbackTarget::ImplementationCommit { commit_sha } => {
                let row = impl_revs::fetch_by_sha(&self.pool, plan_id, commit_sha).await?;
                if row.is_some() {
                    Ok(())
                } else {
                    Err(LifecycleServiceError::TargetNotInActivePlan {
                        session_id: session_id.as_str().into(),
                        target: format!("commit {}", commit_sha.as_str()),
                    })
                }
            }
        }
    }

    /// Read the current cached active plan (used by handlers that need to
    /// answer "is there an active plan, and what is it?" without holding the
    /// observe lock).
    pub async fn peek(&self, session_id: &SessionId) -> Option<ActivePlan> {
        let cell = match self.lock_for(session_id).await {
            Ok(c) => c,
            Err(_) => return None,
        };
        cell.lock().await.clone()
    }
}
