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
use tokio::sync::{Mutex, broadcast};

use crate::domain::{
    EventKind, FeedbackFileStatus, FeedbackKind, FeedbackTargetRef, Phase, TargetKind,
};
use crate::lifecycle::{
    ActivePlan, AgentLabel, Decision, LifecycleError, Observation, PlanFilePath, SessionId, decide,
};
use crate::storage::feedback_files::FeedbackFile;
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
    /// Plan-file watcher attachment failed after the lifecycle
    /// transaction committed. The DB now records the new plan path
    /// but the watcher isn't bound to it; the caller must surface
    /// this so it doesn't hand the agent a silently-broken session.
    #[error("plan-file watcher attach failed: {0}")]
    Watcher(String),
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

/// Outcome of `SessionService::resolve_target_for_kind`. Typed so the
/// dispatcher's parse_error sentinel mapping lives in one place
/// (`parse_error_for`) and no caller writes the strings by hand.
#[derive(Debug, Clone)]
pub enum FeedbackTargetResolution {
    Found(ActiveTarget),
    NoActivePlan,
    /// kind=plan asked but no plan_revision exists yet. In practice
    /// every plan is created with revision #1, so this is a rare
    /// degenerate state.
    NoActivePlanTarget,
    /// kind=impl asked but the session is in `planning` phase or has
    /// no observed impl revision.
    NoActiveImplTarget,
}

impl FeedbackTargetResolution {
    pub fn parse_error(&self) -> Option<&'static str> {
        match self {
            FeedbackTargetResolution::Found(_) => None,
            FeedbackTargetResolution::NoActivePlan => Some("no_active_plan"),
            FeedbackTargetResolution::NoActivePlanTarget => Some("no_active_plan_target_for_kind"),
            FeedbackTargetResolution::NoActiveImplTarget => Some("no_active_impl_target_for_kind"),
        }
    }
    pub fn into_option(self) -> Option<ActiveTarget> {
        match self {
            FeedbackTargetResolution::Found(t) => Some(t),
            _ => None,
        }
    }
}

/// Pre-derived snapshot of one `feedback_files` row plus its read-time
/// status. Carried inside `FeedbackContext` so consumers don't recompute.
#[derive(Debug, Clone)]
pub struct FeedbackFileSnapshot {
    pub row: FeedbackFile,
    pub exists_on_disk: bool,
    pub status: FeedbackFileStatus,
}

/// Materialised feedback-context snapshot. Built once from a single
/// read transaction; consumed by `get_context`, the watcher ingest
/// dispatcher, the web UI, and tests.
#[derive(Debug, Clone)]
pub struct FeedbackContext {
    pub session_id: SessionId,
    pub repo_root: std::path::PathBuf,
    pub plan_file_path: std::path::PathBuf,
    pub git_logs_head_path: Option<std::path::PathBuf>,
    pub phase: Phase,
    pub review_target: Option<ActiveTarget>,
    pub plan_target_resolution: FeedbackTargetResolution,
    pub impl_target_resolution: FeedbackTargetResolution,
    pub latest_plan_revision: Option<crate::storage::plan_revisions::PlanRevision>,
    pub latest_implementation_revision:
        Option<crate::storage::implementation_revisions::ImplementationRevision>,
    pub head_sha: Option<String>,
    pub worktree_dirty: Option<bool>,
    pub worktree_status_hash: Option<String>,
    pub plan_files: Vec<FeedbackFileSnapshot>,
    pub impl_files: Vec<FeedbackFileSnapshot>,
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
    /// Per-session event ping channel. Emits the `SessionId` after every
    /// `observe` or `put_feedback` call that produced at least one new row in
    /// `events`. SSE handlers subscribe here and tail the `events` table past
    /// the cursor on each ping.
    events_tx: broadcast::Sender<SessionId>,
}

impl SessionService {
    pub fn new(pool: SqlitePool, watcher: Arc<super::watcher::Watcher>) -> Self {
        let (events_tx, _) = broadcast::channel(256);
        Self {
            pool,
            watcher,
            locks: Mutex::new(HashMap::new()),
            events_tx,
        }
    }

    /// Subscribe to the per-session event ping channel. Pings carry the
    /// `SessionId` whose event stream advanced; subscribers filter and
    /// drive their own cursor against the `events` table.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionId> {
        self.events_tx.subscribe()
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

    /// Kind-aware target resolution. The dispatcher routes through this
    /// (not `resolve_active_target`) so plan/ writes always target a
    /// plan_revision and impl/ writes are accepted only in implementing
    /// phase. Returns a typed `FeedbackTargetResolution` so the
    /// parse_error sentinel mapping lives in `parse_error_for`.
    pub async fn resolve_target_for_kind(
        &self,
        session_id: &SessionId,
        kind: FeedbackKind,
    ) -> Result<FeedbackTargetResolution, LifecycleServiceError> {
        let session = match sessions::fetch(&self.pool, session_id).await? {
            Some(s) => s,
            None => return Err(LifecycleServiceError::NoSession(session_id.as_str().into())),
        };
        let Some(active_plan_id) = session.active_plan_id else {
            return Ok(FeedbackTargetResolution::NoActivePlan);
        };
        let plan = match plans::fetch(&self.pool, active_plan_id).await? {
            Some(p) => p,
            None => return Ok(FeedbackTargetResolution::NoActivePlan),
        };

        match kind {
            FeedbackKind::Plan => {
                // plan/ is accepted in any phase that has an active plan.
                let latest = plan_revisions::latest_for_plan(&self.pool, active_plan_id).await?;
                match latest {
                    Some(rev) => Ok(FeedbackTargetResolution::Found(ActiveTarget {
                        plan_id: active_plan_id,
                        kind: TargetKind::PlanRevision,
                        id: rev.id.to_string(),
                    })),
                    None => Ok(FeedbackTargetResolution::NoActivePlanTarget),
                }
            }
            FeedbackKind::Impl => {
                // impl/ requires implementing phase.
                if plan.state != "implementing" {
                    return Ok(FeedbackTargetResolution::NoActiveImplTarget);
                }
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
                match chosen {
                    Some(sha) => Ok(FeedbackTargetResolution::Found(ActiveTarget {
                        plan_id: active_plan_id,
                        kind: TargetKind::ImplementationCommit,
                        id: sha,
                    })),
                    None => Ok(FeedbackTargetResolution::NoActiveImplTarget),
                }
            }
        }
    }

    /// One read snapshot consumed by `get_context`, the web UI, and
    /// tests. Status is pre-derived per row using the row's kind's
    /// expected target.
    ///
    /// Single-snapshot invariant: session, plan, latest revisions, all
    /// feedback_files rows, and the HEAD SHA are read inside one
    /// `BEGIN DEFERRED` transaction + one `git rev-parse HEAD` so a
    /// concurrent watcher update can't yield a mixed view where phase
    /// comes from one snapshot and target resolution from another.
    pub async fn build_feedback_context(
        &self,
        session_id: &SessionId,
    ) -> Result<FeedbackContext, LifecycleServiceError> {
        // Read git state first so we can fold HEAD-match lookup into
        // the same DB transaction below. The two reads together form
        // one read snapshot — a concurrent watcher update can't produce
        // a mixed view where `phase` is from one moment and the
        // resolved target from another.
        let pre = sessions::fetch(&self.pool, session_id).await?;
        let repo_root_for_git = pre.as_ref().map(|s| std::path::PathBuf::from(&s.repo_root));
        let git_logs_head_path = match repo_root_for_git.as_ref() {
            Some(r) => super::git::resolve_git_logs_head(r).await.ok(),
            None => None,
        };
        let head_sha_from_git = match repo_root_for_git.as_ref() {
            Some(r) => super::git::rev_parse_head(r).await.ok(),
            None => None,
        };

        let mut tx = self.pool.begin().await?;

        let session: crate::storage::sessions::Session =
            match sqlx::query_as("SELECT * FROM sessions WHERE id = ?")
                .bind(session_id.as_str())
                .fetch_optional(&mut *tx)
                .await?
            {
                Some(s) => s,
                None => return Err(LifecycleServiceError::NoSession(session_id.as_str().into())),
            };
        let repo_root = std::path::PathBuf::from(&session.repo_root);
        let plan_file_path = std::path::PathBuf::from(&session.plan_file_path);

        let active_plan: Option<crate::storage::plans::Plan> = match session.active_plan_id {
            Some(id) => {
                sqlx::query_as("SELECT * FROM plans WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&mut *tx)
                    .await?
            }
            None => None,
        };

        let phase = match active_plan.as_ref().map(|p| p.state.as_str()) {
            Some("planning") => Phase::Planning,
            Some("implementing") => Phase::Implementing,
            _ => Phase::NoActivePlan,
        };

        let latest_plan_revision: Option<crate::storage::plan_revisions::PlanRevision> =
            match active_plan.as_ref() {
                Some(p) => {
                    sqlx::query_as(
                        "SELECT * FROM plan_revisions WHERE plan_id = ? ORDER BY id DESC LIMIT 1",
                    )
                    .bind(p.id)
                    .fetch_optional(&mut *tx)
                    .await?
                }
                None => None,
            };

        let latest_impl_revision_row: Option<
            crate::storage::implementation_revisions::ImplementationRevision,
        > = match active_plan.as_ref() {
            Some(p) => sqlx::query_as(
                "SELECT * FROM implementation_revisions WHERE plan_id = ? ORDER BY id DESC LIMIT 1",
            )
            .bind(p.id)
            .fetch_optional(&mut *tx)
            .await?,
            None => None,
        };

        // HEAD-match lookup inside the same tx as everything else so
        // a concurrent INSERT of a new impl_revision can't sneak between
        // the latest-row read and the HEAD-match read.
        let head_match_row: Option<
            crate::storage::implementation_revisions::ImplementationRevision,
        > =
            match (active_plan.as_ref(), head_sha_from_git.as_deref()) {
                (Some(p), Some(head)) => sqlx::query_as(
                    "SELECT * FROM implementation_revisions WHERE plan_id = ? AND commit_sha = ?",
                )
                .bind(p.id)
                .bind(head)
                .fetch_optional(&mut *tx)
                .await?,
                _ => None,
            };

        let all_files: Vec<FeedbackFile> = sqlx::query_as(
            "SELECT * FROM feedback_files WHERE session_id = ? ORDER BY feedback_kind, author_label",
        )
        .bind(session_id.as_str())
        .fetch_all(&mut *tx)
        .await?;

        tx.commit().await?;

        // resolve_target_for_kind, inlined and driven by the locals.
        let plan_target_resolution = match (active_plan.as_ref(), latest_plan_revision.as_ref()) {
            (None, _) => FeedbackTargetResolution::NoActivePlan,
            (Some(_), None) => FeedbackTargetResolution::NoActivePlanTarget,
            (Some(p), Some(rev)) => FeedbackTargetResolution::Found(ActiveTarget {
                plan_id: p.id,
                kind: TargetKind::PlanRevision,
                id: rev.id.to_string(),
            }),
        };

        let impl_target_resolution = match (active_plan.as_ref(), &latest_impl_revision_row) {
            (None, _) => FeedbackTargetResolution::NoActivePlan,
            (Some(p), _) if p.state != "implementing" => {
                FeedbackTargetResolution::NoActiveImplTarget
            }
            (Some(_), None) => FeedbackTargetResolution::NoActiveImplTarget,
            (Some(p), Some(latest)) => {
                let sha = match (head_match_row.as_ref(), head_sha_from_git.as_deref()) {
                    (Some(_), Some(head)) => head.to_string(),
                    _ => latest.commit_sha.clone(),
                };
                FeedbackTargetResolution::Found(ActiveTarget {
                    plan_id: p.id,
                    kind: TargetKind::ImplementationCommit,
                    id: sha,
                })
            }
        };

        // review_target follows the phase: for implementing, this is
        // the impl resolution; for planning, the plan resolution; for
        // no-active-plan, None.
        let review_target = match phase {
            Phase::Implementing => impl_target_resolution.clone().into_option(),
            Phase::Planning => plan_target_resolution.clone().into_option(),
            Phase::NoActivePlan => None,
        };

        // `latest_implementation_revision` view: present iff phase is
        // implementing AND we have a row matching the resolved target.
        let (latest_implementation_revision, worktree_dirty, worktree_status_hash) = if phase
            == Phase::Implementing
            && let Some(target) = review_target.as_ref()
        {
            let chosen = if let Some(row) = head_match_row {
                Some(row.clone())
            } else {
                latest_impl_revision_row
                    .as_ref()
                    .filter(|r| r.commit_sha == target.id)
                    .cloned()
            };
            let porcelain = super::git::worktree_porcelain(&repo_root)
                .await
                .unwrap_or_default();
            let dirty = !porcelain.trim().is_empty();
            let hash = blake3::hash(porcelain.as_bytes()).to_hex().to_string();
            (chosen, Some(dirty), Some(hash))
        } else {
            (None, None, None)
        };
        let head_sha = head_sha_from_git;

        let plan_expected = match &plan_target_resolution {
            FeedbackTargetResolution::Found(t) => Some(t.clone()),
            _ => None,
        };
        let impl_expected = match &impl_target_resolution {
            FeedbackTargetResolution::Found(t) => Some(t.clone()),
            _ => None,
        };

        let mut plan_files = Vec::new();
        let mut impl_files = Vec::new();
        for row in all_files {
            let exists = std::path::Path::new(&row.path).exists();
            let (target_expected, bucket) = match row.kind() {
                Some(FeedbackKind::Plan) => (plan_expected.as_ref(), &mut plan_files),
                Some(FeedbackKind::Impl) => (impl_expected.as_ref(), &mut impl_files),
                None => continue,
            };
            let status =
                super::feedback_status::derive_feedback_file_status(&row, target_expected, exists);
            bucket.push(FeedbackFileSnapshot {
                row,
                exists_on_disk: exists,
                status,
            });
        }

        Ok(FeedbackContext {
            session_id: session_id.clone(),
            repo_root,
            plan_file_path,
            git_logs_head_path,
            phase,
            review_target,
            plan_target_resolution,
            impl_target_resolution,
            latest_plan_revision,
            latest_implementation_revision,
            head_sha,
            worktree_dirty,
            worktree_status_hash,
            plan_files,
            impl_files,
        })
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

        // The DB commit above is durable. Update the in-memory cache
        // BEFORE the watcher attach so that a watcher failure can't
        // leave the cache lagging behind committed state — a stale
        // cache would route the next reducer call from the old state
        // and re-issue effects (e.g. a second StartPlan on a session
        // that already has an active plan).
        *state = decision.new_active.clone();

        // SSE ping: any apply layer effect lands a row in `events`. The
        // ping fires after the DB commit so subscribers querying the
        // cursor will always see the new row.
        if !apply_outcome.items.is_empty() {
            let _ = self.events_tx.send(session_id.clone());
        }

        if let Some(path) = &path_for_update {
            // Propagate watcher errors so the caller knows the session
            // isn't fully wired. The cache already matches the DB above,
            // so retries operate on a consistent base.
            self.watcher
                .switch_plan_file(session_id, path)
                .map_err(|err| LifecycleServiceError::Watcher(err.to_string()))?;
        }

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
    /// 5. commit and broadcast when the transaction appended an event.
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
            Err(LifecycleServiceError::Reducer(_))
            | Err(LifecycleServiceError::Apply(_))
            | Err(LifecycleServiceError::Watcher(_)) => {
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
        let mut event_appended = false;

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
            event_appended = true;
        }

        let existing = feedback_store::find_by_natural_key(
            &mut *tx,
            active_plan_id,
            kind,
            &target_id,
            &author_label,
        )
        .await?;

        let outcome = match existing {
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
                event_appended = true;
                sessions::touch_updated_at(&mut *tx, session_id, now).await?;
                FeedbackUpsertOutcome {
                    record,
                    was_insert: true,
                    was_no_op: false,
                }
            }
            Some(prior) if prior.body == body => FeedbackUpsertOutcome {
                record: prior.into_record()?,
                was_insert: false,
                was_no_op: true,
            },
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
                event_appended = true;
                sessions::touch_updated_at(&mut *tx, session_id, now).await?;
                FeedbackUpsertOutcome {
                    record,
                    was_insert: false,
                    was_no_op: false,
                }
            }
        };

        tx.commit().await?;

        // Broadcast only when the transaction appended an event row.
        // Byte-identical feedback no-ops stay quiet unless the same call
        // also introduced a new agent_joined event.
        if event_appended {
            let _ = self.events_tx.send(session_id.clone());
        }

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
