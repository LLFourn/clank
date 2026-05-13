//! CRUD on the `sessions` table. A Trinity session is a master-supplied
//! URL-safe slug; it owns a watched plan-file path and its lifecycle state.

use sqlx::SqlitePool;

use crate::lifecycle::{PlanFilePath, SessionId};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Session {
    pub id: String,
    pub repo_root: String,
    pub plan_file_path: String,
    pub display_title: Option<String>,
    /// Compatibility projection for call sites still carrying a numeric
    /// active-plan handle. It is the SQLite rowid while the session is
    /// planning or implementing, otherwise `None`.
    pub active_plan_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
}

const SELECT_SESSION_BY_ID: &str = "\
    SELECT id, repo_root, plan_file_path, display_title, \
           CASE WHEN state IN ('planning', 'implementing') THEN rowid ELSE NULL END AS active_plan_id, \
           created_at, updated_at, archived_at \
      FROM sessions WHERE id = ?";
const SELECT_ACTIVE_SESSIONS: &str = "\
    SELECT id, repo_root, plan_file_path, display_title, \
           CASE WHEN state IN ('planning', 'implementing') THEN rowid ELSE NULL END AS active_plan_id, \
           created_at, updated_at, archived_at \
      FROM sessions WHERE archived_at IS NULL ORDER BY updated_at DESC";
const SELECT_ACTIVE_SESSIONS_BY_REPO: &str = "\
    SELECT id, repo_root, plan_file_path, display_title, \
           CASE WHEN state IN ('planning', 'implementing') THEN rowid ELSE NULL END AS active_plan_id, \
           created_at, updated_at, archived_at \
      FROM sessions WHERE repo_root = ? AND archived_at IS NULL ORDER BY updated_at DESC";

pub async fn fetch(pool: &SqlitePool, session_id: &SessionId) -> sqlx::Result<Option<Session>> {
    sqlx::query_as::<_, Session>(SELECT_SESSION_BY_ID)
        .bind(session_id.as_str())
        .fetch_optional(pool)
        .await
}

pub async fn fetch_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    session_id: &SessionId,
) -> sqlx::Result<Option<Session>> {
    sqlx::query_as::<_, Session>(SELECT_SESSION_BY_ID)
        .bind(session_id.as_str())
        .fetch_optional(&mut **tx)
        .await
}

pub async fn list_active(pool: &SqlitePool) -> sqlx::Result<Vec<Session>> {
    sqlx::query_as::<_, Session>(SELECT_ACTIVE_SESSIONS)
        .fetch_all(pool)
        .await
}

pub async fn list_by_repo_root(pool: &SqlitePool, repo_root: &str) -> sqlx::Result<Vec<Session>> {
    sqlx::query_as::<_, Session>(SELECT_ACTIVE_SESSIONS_BY_REPO)
        .bind(repo_root)
        .fetch_all(pool)
        .await
}

#[allow(clippy::too_many_arguments)]
pub async fn insert<'e, E>(
    executor: E,
    session_id: &SessionId,
    repo_root: &str,
    plan_file_path: &PlanFilePath,
    display_title: Option<&str>,
    now: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO sessions \
            (id, repo_root, plan_file_path, display_title, base_commit, state, started_at, finished_at, created_at, updated_at, archived_at) \
         VALUES (?, ?, ?, ?, NULL, NULL, NULL, NULL, ?, ?, NULL)",
    )
    .bind(session_id.as_str())
    .bind(repo_root)
    .bind(plan_file_path.as_str())
    .bind(display_title)
    .bind(now)
    .bind(now)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn set_plan_file_path<'e, E>(
    executor: E,
    session_id: &SessionId,
    path: &PlanFilePath,
    now: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE sessions SET plan_file_path = ?, updated_at = ? WHERE id = ?")
        .bind(path.as_str())
        .bind(now)
        .bind(session_id.as_str())
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn active_plan_rowid<'e, E>(
    executor: E,
    session_id: &SessionId,
    require_active: bool,
) -> sqlx::Result<Option<i64>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let sql = if require_active {
        "SELECT rowid FROM sessions WHERE id = ? AND state IN ('planning', 'implementing')"
    } else {
        "SELECT rowid FROM sessions WHERE id = ?"
    };
    sqlx::query_scalar(sql)
        .bind(session_id.as_str())
        .fetch_optional(executor)
        .await
}

pub async fn set_display_title<'e, E>(
    executor: E,
    session_id: &SessionId,
    display_title: &str,
    now: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE sessions SET display_title = ?, updated_at = ? WHERE id = ?")
        .bind(display_title)
        .bind(now)
        .bind(session_id.as_str())
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn touch_updated_at<'e, E>(
    executor: E,
    session_id: &SessionId,
    now: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE sessions SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(session_id.as_str())
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn set_current_event_floor<'e, E>(
    executor: E,
    session_id: &SessionId,
    event_floor: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE sessions SET current_event_floor = ? WHERE id = ?")
        .bind(event_floor)
        .bind(session_id.as_str())
        .execute(executor)
        .await?;
    Ok(())
}

/// Validate a session_id slug. Returns the trimmed id if valid; error otherwise.
/// Allowed chars: A-Z, a-z, 0-9, `_`, `-`, `.`. Length 1..=64.
pub fn validate_slug(s: &str) -> Result<SessionId, String> {
    if s.is_empty() {
        return Err("session_id must be non-empty".into());
    }
    if s.len() > 64 {
        return Err(format!(
            "session_id too long ({} chars; max 64)",
            s.chars().count()
        ));
    }
    for c in s.chars() {
        let ok = c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.';
        if !ok {
            return Err(format!(
                "session_id contains invalid character `{}` (allowed: letters, digits, `_`, `-`, `.`)",
                c
            ));
        }
    }
    Ok(SessionId::from(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_valid() {
        assert!(validate_slug("gzip-compression").is_ok());
        assert!(validate_slug("plan_v2").is_ok());
        assert!(validate_slug("a.b.c").is_ok());
        assert!(validate_slug("X1").is_ok());
    }

    #[test]
    fn slug_rejects_invalid() {
        assert!(validate_slug("").is_err());
        assert!(validate_slug("has spaces").is_err());
        assert!(validate_slug("slash/here").is_err());
        assert!(validate_slug("emoji-✨").is_err());
        assert!(validate_slug(&"a".repeat(65)).is_err());
    }
}
