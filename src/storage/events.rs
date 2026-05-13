//! Append-only audit timeline. The reducer never reads from this table;
//! source attribution rides on the `actor` field.

use serde_json::Value;
use sqlx::SqlitePool;

use crate::lifecycle::SessionId;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub session_id: String,
    pub plan_id: Option<i64>,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub ts: i64,
    pub kind: String,
    pub actor: String,
    pub payload: String,
    pub status: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub async fn append<'e, E>(
    executor: E,
    session_id: &SessionId,
    plan_id: Option<i64>,
    target_kind: Option<&str>,
    target_id: Option<&str>,
    kind: &str,
    actor: &str,
    payload: &Value,
    status: Option<&str>,
    ts: i64,
) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let payload_str = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    let result = sqlx::query(
        "INSERT INTO events (session_id, plan_id, target_kind, target_id, ts, kind, actor, payload, status) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(session_id.as_str())
    .bind(plan_id)
    .bind(target_kind)
    .bind(target_id)
    .bind(ts)
    .bind(kind)
    .bind(actor)
    .bind(payload_str)
    .bind(status)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn for_session(pool: &SqlitePool, session_id: &SessionId) -> sqlx::Result<Vec<Event>> {
    sqlx::query_as::<_, Event>("SELECT * FROM events WHERE session_id = ? ORDER BY id ASC")
        .bind(session_id.as_str())
        .fetch_all(pool)
        .await
}

pub async fn for_plan(pool: &SqlitePool, plan_id: i64) -> sqlx::Result<Vec<Event>> {
    sqlx::query_as::<_, Event>("SELECT * FROM events WHERE plan_id = ? ORDER BY id ASC")
        .bind(plan_id)
        .fetch_all(pool)
        .await
}

pub async fn recent_for_active_sessions(pool: &SqlitePool, limit: i64) -> sqlx::Result<Vec<Event>> {
    sqlx::query_as::<_, Event>(
        "SELECT e.* FROM events e \
         JOIN sessions s ON s.id = e.session_id \
         WHERE s.archived_at IS NULL AND s.active_plan_id IS NOT NULL \
         ORDER BY e.id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn active_session_events_after(
    pool: &SqlitePool,
    cursor: i64,
) -> sqlx::Result<Vec<Event>> {
    sqlx::query_as::<_, Event>(
        "SELECT e.* FROM events e \
         JOIN sessions s ON s.id = e.session_id \
         WHERE s.archived_at IS NULL AND s.active_plan_id IS NOT NULL AND e.id > ? \
         ORDER BY e.id ASC",
    )
    .bind(cursor)
    .fetch_all(pool)
    .await
}

pub async fn max_id(pool: &SqlitePool) -> sqlx::Result<i64> {
    let max_id: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM events")
        .fetch_one(pool)
        .await?;
    Ok(max_id.unwrap_or(0))
}
