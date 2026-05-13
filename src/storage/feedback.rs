//! Structural feedback storage. One row per `(session, target, author)`;
//! `put_feedback` upserts on that natural key.
//!
//! Two row types live here:
//! - `FeedbackRow` (pub(crate)): mirrors the SQL schema verbatim, carries
//!   `target_kind` + `target_id: String`. Only flows between this module
//!   and `SessionService::put_feedback` (which needs the prior body).
//! - `FeedbackRecord` (pub): service/API/UI-shaped, carries the typed
//!   `FeedbackTargetRef`. Raw `target_id: String` does not leak past this
//!   module.

use sqlx::SqlitePool;

use crate::domain::{FeedbackTargetRef, TargetKind};
use crate::lifecycle::{AgentLabel, SessionId};

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct FeedbackRow {
    pub id: i64,
    pub session_id: String,
    /// Compatibility projection: the owning session rowid.
    pub plan_id: i64,
    pub target_kind: String,
    pub target_id: String,
    pub author_label: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackRecord {
    pub id: i64,
    pub session_id: SessionId,
    pub plan_id: i64,
    pub target: FeedbackTargetRef,
    pub author_label: AgentLabel,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unknown target_kind `{0}` in feedback row {id}", id = .1)]
    UnknownTargetKind(String, i64),
    #[error("malformed target_id `{target_id}` for kind {kind} in feedback row {id}")]
    MalformedTargetId {
        kind: TargetKind,
        target_id: String,
        id: i64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sql: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
}

impl FeedbackRow {
    pub(crate) fn into_record(self) -> Result<FeedbackRecord, DecodeError> {
        let kind = TargetKind::parse(&self.target_kind)
            .ok_or_else(|| DecodeError::UnknownTargetKind(self.target_kind.clone(), self.id))?;
        let target = FeedbackTargetRef::from_storage(kind, &self.target_id).ok_or_else(|| {
            DecodeError::MalformedTargetId {
                kind,
                target_id: self.target_id.clone(),
                id: self.id,
            }
        })?;
        Ok(FeedbackRecord {
            id: self.id,
            session_id: SessionId::from(self.session_id),
            plan_id: self.plan_id,
            target,
            author_label: AgentLabel::from(self.author_label),
            body: self.body,
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

/// Look up a feedback row by its natural key. Used by `put_feedback` to
/// decide INSERT vs UPDATE vs no-op; returns the storage row so the prior
/// body is available for the audit event payload.
pub(crate) async fn find_by_natural_key<'e, E>(
    executor: E,
    plan_id: i64,
    target_kind: TargetKind,
    target_id: &str,
    author_label: &str,
) -> sqlx::Result<Option<FeedbackRow>>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query_as::<_, FeedbackRow>(
        "SELECT f.id, f.session_id, s.rowid AS plan_id, f.target_kind, f.target_id, \
                f.author_label, f.body, f.created_at, f.updated_at \
         FROM feedback f JOIN sessions s ON s.id = f.session_id \
         WHERE s.rowid = ? AND f.target_kind = ? AND f.target_id = ? AND f.author_label = ?",
    )
    .bind(plan_id)
    .bind(target_kind.as_str())
    .bind(target_id)
    .bind(author_label)
    .fetch_optional(executor)
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert<'e, E>(
    executor: E,
    session_id: &SessionId,
    _plan_id: i64,
    target_kind: TargetKind,
    target_id: &str,
    author_label: &str,
    body: &str,
    now: i64,
) -> sqlx::Result<FeedbackRow>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query_as::<_, FeedbackRow>(
        "INSERT INTO feedback \
            (session_id, target_kind, target_id, author_label, body, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         RETURNING id, session_id, (SELECT rowid FROM sessions WHERE id = feedback.session_id) AS plan_id, \
                   target_kind, target_id, author_label, body, created_at, updated_at",
    )
    .bind(session_id.as_str())
    .bind(target_kind.as_str())
    .bind(target_id)
    .bind(author_label)
    .bind(body)
    .bind(now)
    .bind(now)
    .fetch_one(executor)
    .await?;
    Ok(row)
}

pub(crate) async fn update_body<'e, E>(
    executor: E,
    feedback_id: i64,
    new_body: &str,
    now: i64,
) -> sqlx::Result<FeedbackRow>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query_as::<_, FeedbackRow>(
        "UPDATE feedback SET body = ?, updated_at = ? WHERE id = ? \
         RETURNING id, session_id, (SELECT rowid FROM sessions WHERE id = feedback.session_id) AS plan_id, \
                   target_kind, target_id, author_label, body, created_at, updated_at",
    )
    .bind(new_body)
    .bind(now)
    .bind(feedback_id)
    .fetch_one(executor)
    .await?;
    Ok(row)
}

pub async fn fetch<'e, E>(executor: E, feedback_id: i64) -> Result<Option<FeedbackRecord>, Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let row = sqlx::query_as::<_, FeedbackRow>(
        "SELECT f.id, f.session_id, s.rowid AS plan_id, f.target_kind, f.target_id, \
                f.author_label, f.body, f.created_at, f.updated_at \
         FROM feedback f JOIN sessions s ON s.id = f.session_id \
         WHERE f.id = ?",
    )
    .bind(feedback_id)
    .fetch_optional(executor)
    .await?;
    row.map(|r| r.into_record().map_err(Error::from))
        .transpose()
}

fn decode_all(rows: Vec<FeedbackRow>) -> Result<Vec<FeedbackRecord>, Error> {
    rows.into_iter()
        .map(|r| r.into_record().map_err(Error::from))
        .collect()
}

/// List rows for a specific session rowid, optionally filtered by target_kind,
/// ordered by id ASC. Used by `current_feedback` and the archived-plan
/// history pages.
pub async fn list_for_plan<'e, E>(
    executor: E,
    plan_id: i64,
    filter: Option<TargetKind>,
) -> Result<Vec<FeedbackRecord>, Error>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let rows = match filter {
        Some(kind) => {
            sqlx::query_as::<_, FeedbackRow>(
                "SELECT f.id, f.session_id, s.rowid AS plan_id, f.target_kind, f.target_id, \
                        f.author_label, f.body, f.created_at, f.updated_at \
                 FROM feedback f JOIN sessions s ON s.id = f.session_id \
                 WHERE s.rowid = ? AND f.target_kind = ? ORDER BY f.id ASC",
            )
            .bind(plan_id)
            .bind(kind.as_str())
            .fetch_all(executor)
            .await?
        }
        None => {
            sqlx::query_as::<_, FeedbackRow>(
                "SELECT f.id, f.session_id, s.rowid AS plan_id, f.target_kind, f.target_id, \
                        f.author_label, f.body, f.created_at, f.updated_at \
                 FROM feedback f JOIN sessions s ON s.id = f.session_id \
                 WHERE s.rowid = ? ORDER BY f.id ASC",
            )
            .bind(plan_id)
            .fetch_all(executor)
            .await?
        }
    };
    decode_all(rows)
}

/// List the feedback rows belonging to the session's current active lifecycle.
/// Returns `[]` when the session has no active plan. Resolves
/// the session lifecycle state in one
/// statement, so the result is a consistent snapshot.
pub async fn list_for_active_plan(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> Result<Vec<FeedbackRecord>, Error> {
    let rows = sqlx::query_as::<_, FeedbackRow>(
        "SELECT f.id, f.session_id, s.rowid AS plan_id, f.target_kind, f.target_id, \
                f.author_label, f.body, f.created_at, f.updated_at \
         FROM feedback f JOIN sessions s ON s.id = f.session_id \
         WHERE s.id = ? AND s.state IN ('planning', 'implementing') \
         ORDER BY f.id ASC",
    )
    .bind(session_id.as_str())
    .fetch_all(pool)
    .await?;
    decode_all(rows)
}

/// All feedback rows for a session across every plan (active + archived).
/// Used by archived-plan history rendering.
pub async fn list_for_session(
    pool: &SqlitePool,
    session_id: &SessionId,
) -> Result<Vec<FeedbackRecord>, Error> {
    let rows = sqlx::query_as::<_, FeedbackRow>(
        "SELECT f.id, f.session_id, s.rowid AS plan_id, f.target_kind, f.target_id, \
                f.author_label, f.body, f.created_at, f.updated_at \
         FROM feedback f JOIN sessions s ON s.id = f.session_id \
         WHERE f.session_id = ? ORDER BY f.id ASC",
    )
    .bind(session_id.as_str())
    .fetch_all(pool)
    .await?;
    decode_all(rows)
}
