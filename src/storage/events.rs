use serde_json::Value;
use sqlx::SqlitePool;

use crate::domain::{EventKind, FeedbackStatus, TargetKind};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub plan_id: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub ts: i64,
    pub kind: String,
    pub actor: String,
    pub payload: String,
    pub status: Option<String>,
}

/// Strongly-typed event row to insert. Replaces the previous 9-arg `append`
/// signature so call sites can't accidentally swap two consecutive
/// `Option<&str>` arguments.
pub struct NewEvent<'a> {
    pub plan_id: &'a str,
    pub kind: EventKind,
    pub actor: &'a str,
    pub payload: &'a Value,
    pub ts: i64,
    pub target: Option<EventTarget<'a>>,
    pub status: Option<FeedbackStatus>,
}

#[derive(Debug, Clone, Copy)]
pub struct EventTarget<'a> {
    pub kind: TargetKind,
    pub id: &'a str,
}

impl<'a> NewEvent<'a> {
    pub fn note(
        plan_id: &'a str,
        kind: EventKind,
        actor: &'a str,
        payload: &'a Value,
        ts: i64,
    ) -> Self {
        Self {
            plan_id,
            kind,
            actor,
            payload,
            ts,
            target: None,
            status: None,
        }
    }

    pub fn against_target(
        plan_id: &'a str,
        kind: EventKind,
        actor: &'a str,
        payload: &'a Value,
        ts: i64,
        target: EventTarget<'a>,
    ) -> Self {
        Self {
            plan_id,
            kind,
            actor,
            payload,
            ts,
            target: Some(target),
            status: None,
        }
    }
}

pub async fn append<'e, E>(executor: E, ev: &NewEvent<'_>) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let payload_str = serde_json::to_string(ev.payload).unwrap_or_else(|_| "{}".to_string());
    let target_kind = ev.target.map(|t| t.kind.as_str());
    let target_id = ev.target.map(|t| t.id);
    let status = ev.status.map(|s| s.as_str());
    let result = sqlx::query(
        "INSERT INTO events (plan_id, target_kind, target_id, ts, kind, actor, payload, status) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(ev.plan_id)
    .bind(target_kind)
    .bind(target_id)
    .bind(ev.ts)
    .bind(ev.kind.as_str())
    .bind(ev.actor)
    .bind(payload_str)
    .bind(status)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn for_plan(pool: &SqlitePool, plan_id: &str) -> sqlx::Result<Vec<Event>> {
    sqlx::query_as::<_, Event>("SELECT * FROM events WHERE plan_id = ? ORDER BY id ASC")
        .bind(plan_id)
        .fetch_all(pool)
        .await
}
