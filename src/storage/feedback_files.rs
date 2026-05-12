//! CRUD on the `feedback_files` sidecar table. One row per
//! `(session_id, author_label)`; tracks the lifecycle of an
//! agent-authored markdown file under
//! `<repo_root>/.trinity/feedback/<session_id>/<author_label>.md`.
//!
//! Created on first observation by the feedback directory watcher (not
//! on any explicit registration MCP call). Derived status (current /
//! stale / missing / parse_error / not_yet_ingested) is computed by
//! callers from the columns; we don't persist a status enum.

use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::domain::TargetKind;
use crate::lifecycle::{AgentLabel, SessionId};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedbackFile {
    pub session_id: String,
    pub author_label: String,
    pub path: String,
    pub last_observed_hash: Option<String>,
    pub last_observed_at: Option<i64>,
    pub last_ingested_hash: Option<String>,
    pub last_ingested_at: Option<i64>,
    pub last_ingested_target_kind: Option<String>,
    pub last_ingested_target_id: Option<String>,
    pub parse_error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub async fn fetch(
    pool: &SqlitePool,
    session_id: &SessionId,
    author_label: &AgentLabel,
) -> sqlx::Result<Option<FeedbackFile>> {
    sqlx::query_as::<_, FeedbackFile>(
        "SELECT * FROM feedback_files WHERE session_id = ? AND author_label = ?",
    )
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .fetch_optional(pool)
    .await
}

pub async fn list_for_session(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<Vec<FeedbackFile>> {
    sqlx::query_as::<_, FeedbackFile>(
        "SELECT * FROM feedback_files WHERE session_id = ? ORDER BY author_label ASC",
    )
    .bind(session_id.as_str())
    .fetch_all(pool)
    .await
}

/// Idempotently insert a sidecar row in the `not_yet_ingested` state.
/// Called by the feedback-directory watcher dispatcher on first sight
/// of a new file under a watched directory. Returns the freshly-loaded
/// row.
pub async fn upsert_observed(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    author_label: &AgentLabel,
    path: &str,
    now: i64,
) -> sqlx::Result<FeedbackFile> {
    sqlx::query(
        "INSERT INTO feedback_files \
         (session_id, author_label, path, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(session_id, author_label) DO UPDATE SET path = excluded.path, updated_at = excluded.updated_at",
    )
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .bind(path)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    sqlx::query_as::<_, FeedbackFile>(
        "SELECT * FROM feedback_files WHERE session_id = ? AND author_label = ?",
    )
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .fetch_one(&mut **tx)
    .await
}

/// Record that the watcher observed a file body with the given hash
/// (post-debounce). Clears any prior `parse_error`.
pub async fn set_observed(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    author_label: &AgentLabel,
    hash: &str,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET last_observed_hash = ?, last_observed_at = ?, updated_at = ? \
         WHERE session_id = ? AND author_label = ?",
    )
    .bind(hash)
    .bind(now)
    .bind(now)
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Record a successful ingest of the file into the `feedback` table.
/// Clears `parse_error`.
pub async fn set_ingested(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    author_label: &AgentLabel,
    hash: &str,
    target_kind: TargetKind,
    target_id: &str,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET last_ingested_hash = ?, last_ingested_at = ?, \
             last_ingested_target_kind = ?, last_ingested_target_id = ?, \
             parse_error = NULL, updated_at = ? \
         WHERE session_id = ? AND author_label = ?",
    )
    .bind(hash)
    .bind(now)
    .bind(target_kind.as_str())
    .bind(target_id)
    .bind(now)
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Record an ingest failure (no active plan, empty body, target
/// mismatch, etc.). Leaves the `last_ingested_*` columns untouched so
/// the historical ingest is still visible.
pub async fn set_parse_error(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    author_label: &AgentLabel,
    error: &str,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET parse_error = ?, updated_at = ? \
         WHERE session_id = ? AND author_label = ?",
    )
    .bind(error)
    .bind(now)
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// File disappeared from disk. Clear `last_observed_hash` so the
/// derived status flips to `missing`. Also clears `parse_error` since
/// the prior body that caused it no longer exists on disk — without
/// clearing, the derived-status helpers check `parse_error` before
/// `missing` and would leave a deleted-after-error file stuck in the
/// error state forever. The historical `feedback` row (if any) is
/// left alone — retraction is a v2 concern.
pub async fn mark_missing(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    author_label: &AgentLabel,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET last_observed_hash = NULL, last_observed_at = ?, parse_error = NULL, updated_at = ? \
         WHERE session_id = ? AND author_label = ?",
    )
    .bind(now)
    .bind(now)
    .bind(session_id.as_str())
    .bind(author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}
