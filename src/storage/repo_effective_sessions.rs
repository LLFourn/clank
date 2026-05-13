//! Per-repo commit-routing claim. Plan discovery is passive; only the
//! effective session for a repo receives `.git/logs/HEAD` movements.

use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::lifecycle::SessionId;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RepoEffectiveSession {
    pub repo_root: String,
    pub session_id: String,
    pub claimed_by: String,
    pub claimed_at: i64,
}

pub async fn fetch_by_repo(
    pool: &SqlitePool,
    repo_root: &str,
) -> sqlx::Result<Option<RepoEffectiveSession>> {
    sqlx::query_as::<_, RepoEffectiveSession>(
        "SELECT repo_root, session_id, claimed_by, claimed_at \
         FROM repo_effective_sessions WHERE repo_root = ?",
    )
    .bind(repo_root)
    .fetch_optional(pool)
    .await
}

pub async fn is_effective(pool: &SqlitePool, session_id: &SessionId) -> sqlx::Result<bool> {
    let exists: Option<i64> =
        sqlx::query_scalar("SELECT 1 FROM repo_effective_sessions WHERE session_id = ?")
            .bind(session_id.as_str())
            .fetch_optional(pool)
            .await?;
    Ok(exists.is_some())
}

pub async fn claim_if_unset(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    actor: &str,
    now: i64,
) -> sqlx::Result<bool> {
    let result = sqlx::query(
        "INSERT OR IGNORE INTO repo_effective_sessions \
            (repo_root, session_id, claimed_by, claimed_at) \
         SELECT repo_root, id, ?, ? FROM sessions WHERE id = ?",
    )
    .bind(actor)
    .bind(now)
    .bind(session_id.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn claim(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    actor: &str,
    now: i64,
) -> sqlx::Result<Option<String>> {
    let repo_root: String = sqlx::query_scalar("SELECT repo_root FROM sessions WHERE id = ?")
        .bind(session_id.as_str())
        .fetch_one(&mut **tx)
        .await?;
    let prior: Option<String> =
        sqlx::query_scalar("SELECT session_id FROM repo_effective_sessions WHERE repo_root = ?")
            .bind(&repo_root)
            .fetch_optional(&mut **tx)
            .await?;
    sqlx::query(
        "INSERT INTO repo_effective_sessions \
            (repo_root, session_id, claimed_by, claimed_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(repo_root) DO UPDATE SET \
            session_id = excluded.session_id, \
            claimed_by = excluded.claimed_by, \
            claimed_at = excluded.claimed_at",
    )
    .bind(&repo_root)
    .bind(session_id.as_str())
    .bind(actor)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(prior.filter(|prior_session_id| prior_session_id != session_id.as_str()))
}

pub async fn clear_session(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
) -> sqlx::Result<bool> {
    let result = sqlx::query("DELETE FROM repo_effective_sessions WHERE session_id = ?")
        .bind(session_id.as_str())
        .execute(&mut **tx)
        .await?;
    Ok(result.rows_affected() > 0)
}
