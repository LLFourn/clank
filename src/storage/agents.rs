//! CRUD on the `agents` table. Agents are per-session: a `label` is unique
//! within a `session_id`.

use sqlx::SqlitePool;

use crate::lifecycle::{AgentLabel, SessionId};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Agent {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub label: String,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Copy)]
pub enum Role {
    Master,
    Reviewer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Master => "master",
            Role::Reviewer => "reviewer",
        }
    }
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

pub async fn fetch_master(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<Option<Agent>> {
    sqlx::query_as::<_, Agent>(
        "SELECT * FROM agents WHERE session_id = ? AND role = 'master' LIMIT 1",
    )
    .bind(session_id.as_str())
    .fetch_optional(pool)
    .await
}

pub async fn list_for_session(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<Vec<Agent>> {
    sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE session_id = ? ORDER BY first_seen ASC")
        .bind(session_id.as_str())
        .fetch_all(pool)
        .await
}

pub async fn insert<'e, E>(
    executor: E,
    session_id: &SessionId,
    role: Role,
    label: &AgentLabel,
    now: i64,
) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO agents (session_id, role, label, first_seen, last_seen) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(session_id.as_str())
    .bind(role.as_str())
    .bind(label.as_str())
    .bind(now)
    .bind(now)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn touch_last_seen<'e, E>(executor: E, agent_id: i64, now: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE agents SET last_seen = ? WHERE id = ?")
        .bind(now)
        .bind(agent_id)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn reset_all_to_stale<'e, E>(executor: E, stale_at: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE agents SET last_seen = ?")
        .bind(stale_at)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn delete<'e, E>(executor: E, agent_id: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("DELETE FROM agents WHERE id = ?")
        .bind(agent_id)
        .execute(executor)
        .await?;
    Ok(())
}
