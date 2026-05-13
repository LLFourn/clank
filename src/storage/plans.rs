//! CRUD on the `plans` table. A "plan" row in this schema represents a single
//! lifecycle attempt inside a session (NOT the long-lived coordination
//! thread; that's `sessions`).

use sqlx::SqlitePool;

use crate::lifecycle::{CommitSha, ContentHash, SessionId};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Plan {
    pub id: i64,
    pub session_id: String,
    pub base_commit: String,
    pub state: String,
    pub started_at: i64,
    pub archived_at: Option<i64>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanWithLatest {
    pub id: i64,
    pub session_id: String,
    pub base_commit: String,
    pub state: String,
    pub latest_plan_revision_content_hash: Option<String>,
    pub latest_impl_commit_sha: Option<String>,
}

pub async fn fetch(pool: &SqlitePool, plan_id: i64) -> sqlx::Result<Option<Plan>> {
    sqlx::query_as::<_, Plan>("SELECT * FROM plans WHERE id = ?")
        .bind(plan_id)
        .fetch_optional(pool)
        .await
}

pub async fn list_for_session(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<Vec<Plan>> {
    sqlx::query_as::<_, Plan>("SELECT * FROM plans WHERE session_id = ? ORDER BY id ASC")
        .bind(session_id.as_str())
        .fetch_all(pool)
        .await
}

pub async fn list_archived_for_session(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<Vec<Plan>> {
    sqlx::query_as::<_, Plan>(
        "SELECT * FROM plans WHERE session_id = ? AND state = 'archived' ORDER BY archived_at DESC",
    )
    .bind(session_id.as_str())
    .fetch_all(pool)
    .await
}

pub async fn insert<'e, E>(
    executor: E,
    session_id: &SessionId,
    base_commit: &CommitSha,
    now: i64,
) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO plans (session_id, base_commit, state, started_at, archived_at) \
         VALUES (?, ?, 'planning', ?, NULL)",
    )
    .bind(session_id.as_str())
    .bind(base_commit.as_str())
    .bind(now)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn set_state<'e, E>(executor: E, plan_id: i64, state: &str) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE plans SET state = ? WHERE id = ?")
        .bind(state)
        .bind(plan_id)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn archive<'e, E>(executor: E, plan_id: i64, now: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE plans SET state = 'archived', archived_at = ? WHERE id = ?")
        .bind(now)
        .bind(plan_id)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn finish<'e, E>(executor: E, plan_id: i64, now: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE plans SET state = 'finished', finished_at = ? WHERE id = ?")
        .bind(now)
        .bind(plan_id)
        .execute(executor)
        .await?;
    Ok(())
}

/// Code-level invariant check failure: `sessions.active_plan_id` references
/// a row that doesn't satisfy the active-plan invariant.
#[derive(Debug, thiserror::Error)]
pub enum ActivePlanInconsistency {
    #[error("active_plan_id={active_plan_id} not found for session {session_id}")]
    Dangling {
        session_id: String,
        active_plan_id: i64,
    },
    #[error(
        "active_plan_id={active_plan_id} belongs to a different session ({plan_session_id}), expected {session_id}"
    )]
    CrossSession {
        session_id: String,
        active_plan_id: i64,
        plan_session_id: String,
    },
    #[error("active_plan_id={active_plan_id} is archived")]
    Archived {
        session_id: String,
        active_plan_id: i64,
    },
    #[error("active_plan_id={active_plan_id} is finished")]
    Finished {
        session_id: String,
        active_plan_id: i64,
    },
}

/// Single source of truth for resolving a session's active plan. Returns
/// `Ok(None)` if `sessions.active_plan_id IS NULL`. Returns
/// `Err(ActivePlanInconsistency)` if the pointer exists but violates the
/// invariant (archived, cross-session, dangling). Returns
/// `Ok(Some(PlanWithLatest))` for a healthy active plan, carrying the
/// latest plan_revision content_hash and (if implementing) the latest impl
/// commit_sha — enough to rebuild `ActivePlan` without further queries.
pub async fn load_active_plan(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> Result<Option<PlanWithLatest>, LoadActivePlanError> {
    let session =
        sqlx::query_as::<_, (Option<i64>,)>("SELECT active_plan_id FROM sessions WHERE id = ?")
            .bind(session_id.as_str())
            .fetch_optional(pool)
            .await
            .map_err(LoadActivePlanError::Sql)?;
    let Some((Some(active_plan_id),)) = session else {
        return Ok(None);
    };
    let plan = fetch(pool, active_plan_id)
        .await
        .map_err(LoadActivePlanError::Sql)?
        .ok_or_else(|| {
            LoadActivePlanError::Inconsistent(ActivePlanInconsistency::Dangling {
                session_id: session_id.as_str().into(),
                active_plan_id,
            })
        })?;
    if plan.session_id != session_id.as_str() {
        return Err(LoadActivePlanError::Inconsistent(
            ActivePlanInconsistency::CrossSession {
                session_id: session_id.as_str().into(),
                active_plan_id,
                plan_session_id: plan.session_id,
            },
        ));
    }
    if plan.state == "archived" {
        return Err(LoadActivePlanError::Inconsistent(
            ActivePlanInconsistency::Archived {
                session_id: session_id.as_str().into(),
                active_plan_id,
            },
        ));
    }
    if plan.state == "finished" {
        return Err(LoadActivePlanError::Inconsistent(
            ActivePlanInconsistency::Finished {
                session_id: session_id.as_str().into(),
                active_plan_id,
            },
        ));
    }
    // Latest plan revision hash for this plan.
    let latest_plan: Option<(String,)> = sqlx::query_as(
        "SELECT content_hash FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number DESC LIMIT 1",
    )
    .bind(plan.id)
    .fetch_optional(pool)
    .await
    .map_err(LoadActivePlanError::Sql)?;
    let latest_impl: Option<(String,)> = if plan.state == "implementing" {
        sqlx::query_as(
            "SELECT commit_sha FROM implementation_revisions WHERE plan_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(plan.id)
        .fetch_optional(pool)
        .await
        .map_err(LoadActivePlanError::Sql)?
    } else {
        None
    };
    Ok(Some(PlanWithLatest {
        id: plan.id,
        session_id: plan.session_id,
        base_commit: plan.base_commit,
        state: plan.state,
        latest_plan_revision_content_hash: latest_plan.map(|(h,)| h),
        latest_impl_commit_sha: latest_impl.map(|(s,)| s),
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum LoadActivePlanError {
    #[error("sql: {0}")]
    Sql(#[source] sqlx::Error),
    #[error("active-plan invariant violated: {0}")]
    Inconsistent(#[source] ActivePlanInconsistency),
}

impl PlanWithLatest {
    pub fn to_active_plan(&self) -> crate::lifecycle::ActivePlan {
        let base = CommitSha::from(self.base_commit.clone());
        let hash = ContentHash::from(
            self.latest_plan_revision_content_hash
                .clone()
                .expect("a non-archived plan must have at least one revision"),
        );
        if self.state == "implementing" {
            let lic = CommitSha::from(
                self.latest_impl_commit_sha
                    .clone()
                    .expect("implementing plan must have at least one implementation_revision"),
            );
            crate::lifecycle::ActivePlan::Implementing {
                base_commit: base,
                latest_plan_hash: hash,
                latest_impl_commit: lic,
            }
        } else {
            crate::lifecycle::ActivePlan::Planning {
                base_commit: base,
                latest_plan_hash: hash,
            }
        }
    }
}
