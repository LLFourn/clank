//! CRUD on the `agents` table. Agents are per-session: a `label` is unique
//! within a `session_id`. There is no role; agents are just "labels we've
//! seen on this session".

use sqlx::SqlitePool;

use crate::lifecycle::{AgentLabel, SessionId};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Agent {
    pub id: i64,
    pub session_id: String,
    pub label: String,
    pub first_seen: i64,
    pub last_seen: i64,
}

/// Outcome of an `upsert_seen` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeenOutcome {
    /// First time this `(session_id, label)` was seen — caller can emit
    /// an `agent_joined` event.
    Inserted,
    /// Existing row's `last_seen` was bumped. No event should follow.
    Touched,
}

pub async fn fetch_by_label(
    pool: &SqlitePool,
    session_id: &SessionId,
    label: &AgentLabel,
) -> sqlx::Result<Option<Agent>> {
    sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE session_id = ? AND label = ?")
        .bind(session_id.as_str())
        .bind(label.as_str())
        .fetch_optional(pool)
        .await
}

pub async fn list_for_session(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<Vec<Agent>> {
    sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE session_id = ? ORDER BY last_seen DESC")
        .bind(session_id.as_str())
        .fetch_all(pool)
        .await
}

/// Idempotent: INSERT a `(session_id, label)` row if absent, otherwise
/// bump `last_seen`. Reports whether the row was inserted so the caller
/// can decide whether to emit an `agent_joined` audit event.
///
/// Touches **only** the `agents` table. Callers on read-only paths
/// (`get_current_feedback`, `get_review_context`) must not pair this
/// with `sessions::touch_updated_at` or with appending an event on the
/// `Touched` branch.
pub async fn upsert_seen(
    pool: &SqlitePool,
    session_id: &SessionId,
    label: &AgentLabel,
    now: i64,
) -> sqlx::Result<SeenOutcome> {
    // INSERT OR IGNORE is atomic per call. rows_affected() reliably
    // distinguishes "I inserted" from "row already existed" without
    // relying on time-based heuristics that break under same-second
    // concurrent calls.
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO agents (session_id, label, first_seen, last_seen) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(session_id.as_str())
    .bind(label.as_str())
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected()
        == 1;
    if inserted {
        return Ok(SeenOutcome::Inserted);
    }
    sqlx::query("UPDATE agents SET last_seen = ? WHERE session_id = ? AND label = ?")
        .bind(now)
        .bind(session_id.as_str())
        .bind(label.as_str())
        .execute(pool)
        .await?;
    Ok(SeenOutcome::Touched)
}
