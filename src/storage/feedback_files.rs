//! CRUD on the `feedback_files` sidecar. PK is
//! `(session_id, feedback_kind, author_label)` — same author can hold
//! both a `plan` and an `impl` row simultaneously. Created on first
//! observation by the feedback directory watcher.

use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::domain::{FeedbackKind, TargetKind};
use crate::lifecycle::{AgentLabel, SessionId};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FeedbackFile {
    pub session_id: String,
    pub feedback_kind: String,
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

impl FeedbackFile {
    pub fn kind(&self) -> Option<FeedbackKind> {
        FeedbackKind::parse(&self.feedback_kind)
    }
}

pub async fn fetch(
    pool: &SqlitePool,
    session_id: &SessionId,
    kind: FeedbackKind,
    author_label: &AgentLabel,
) -> sqlx::Result<Option<FeedbackFile>> {
    sqlx::query_as::<_, FeedbackFile>(
        "SELECT * FROM feedback_files WHERE session_id = ? AND feedback_kind = ? AND author_label = ?",
    )
    .bind(session_id.as_str())
    .bind(kind.as_str())
    .bind(author_label.as_str())
    .fetch_optional(pool)
    .await
}

pub async fn list_for_session(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<Vec<FeedbackFile>> {
    sqlx::query_as::<_, FeedbackFile>(
        "SELECT * FROM feedback_files WHERE session_id = ? ORDER BY feedback_kind, author_label ASC",
    )
    .bind(session_id.as_str())
    .fetch_all(pool)
    .await
}

pub async fn upsert_observed(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    kind: FeedbackKind,
    author_label: &AgentLabel,
    path: &str,
    now: i64,
) -> sqlx::Result<FeedbackFile> {
    sqlx::query(
        "INSERT INTO feedback_files \
         (session_id, feedback_kind, author_label, path, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?) \
         ON CONFLICT(session_id, feedback_kind, author_label) \
         DO UPDATE SET path = excluded.path, updated_at = excluded.updated_at",
    )
    .bind(session_id.as_str())
    .bind(kind.as_str())
    .bind(author_label.as_str())
    .bind(path)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    sqlx::query_as::<_, FeedbackFile>(
        "SELECT * FROM feedback_files WHERE session_id = ? AND feedback_kind = ? AND author_label = ?",
    )
    .bind(session_id.as_str())
    .bind(kind.as_str())
    .bind(author_label.as_str())
    .fetch_one(&mut **tx)
    .await
}

/// Reset the observation state for every sidecar row in this session.
/// Used after reactivation so that the next `dispatch_feedback_changed`
/// call doesn't short-circuit on the unchanged-hash guard — the file
/// content hasn't changed on disk, but the active plan has, so the
/// dispatch needs to insert a fresh feedback row against the new
/// target.
pub async fn clear_observed_for_session(
    pool: &sqlx::SqlitePool,
    session_id: &SessionId,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files SET last_observed_hash = NULL, last_observed_at = NULL \
         WHERE session_id = ?",
    )
    .bind(session_id.as_str())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_observed(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    kind: FeedbackKind,
    author_label: &AgentLabel,
    hash: &str,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET last_observed_hash = ?, last_observed_at = ?, updated_at = ? \
         WHERE session_id = ? AND feedback_kind = ? AND author_label = ?",
    )
    .bind(hash)
    .bind(now)
    .bind(now)
    .bind(session_id.as_str())
    .bind(kind.as_str())
    .bind(author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Inputs to `set_ingested`. Grouped into a struct so the call sites
/// don't risk swapping positional arguments — five `&str` parameters
/// next to each other is a recipe for confusion.
pub struct Ingested<'a> {
    pub session_id: &'a SessionId,
    pub kind: FeedbackKind,
    pub author_label: &'a AgentLabel,
    pub hash: &'a str,
    pub target_kind: TargetKind,
    pub target_id: &'a str,
    pub now: i64,
}

pub async fn set_ingested(
    tx: &mut Transaction<'_, Sqlite>,
    args: Ingested<'_>,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET last_ingested_hash = ?, last_ingested_at = ?, \
             last_ingested_target_kind = ?, last_ingested_target_id = ?, \
             parse_error = NULL, updated_at = ? \
         WHERE session_id = ? AND feedback_kind = ? AND author_label = ?",
    )
    .bind(args.hash)
    .bind(args.now)
    .bind(args.target_kind.as_str())
    .bind(args.target_id)
    .bind(args.now)
    .bind(args.session_id.as_str())
    .bind(args.kind.as_str())
    .bind(args.author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn set_parse_error(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    kind: FeedbackKind,
    author_label: &AgentLabel,
    error: &str,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET parse_error = ?, updated_at = ? \
         WHERE session_id = ? AND feedback_kind = ? AND author_label = ?",
    )
    .bind(error)
    .bind(now)
    .bind(session_id.as_str())
    .bind(kind.as_str())
    .bind(author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// File disappeared from disk. Clears `last_observed_hash` and
/// `parse_error` so the derived status flips to `missing` (and doesn't
/// stay stuck at a parse_error from a body no longer on disk).
/// Historical `feedback` rows are left alone — retraction is v2.
pub async fn mark_missing(
    tx: &mut Transaction<'_, Sqlite>,
    session_id: &SessionId,
    kind: FeedbackKind,
    author_label: &AgentLabel,
    now: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE feedback_files \
         SET last_observed_hash = NULL, last_observed_at = ?, parse_error = NULL, updated_at = ? \
         WHERE session_id = ? AND feedback_kind = ? AND author_label = ?",
    )
    .bind(now)
    .bind(now)
    .bind(session_id.as_str())
    .bind(kind.as_str())
    .bind(author_label.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}
