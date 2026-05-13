//! CRUD on the `plan_revisions` table. A revision is a body snapshot for
//! a session's one plan.

use sqlx::SqlitePool;

use crate::lifecycle::ContentHash;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanRevision {
    pub id: i64,
    /// Compatibility projection: the owning session rowid.
    pub plan_id: i64,
    pub revision_number: i64,
    pub content_hash: String,
    pub body: String,
    pub created_at: i64,
}

pub fn compute_content_hash(body: &str) -> ContentHash {
    crate::lifecycle::content_hash(body)
}

pub async fn fetch(pool: &SqlitePool, revision_id: i64) -> sqlx::Result<Option<PlanRevision>> {
    sqlx::query_as::<_, PlanRevision>(
        "SELECT pr.id, s.rowid AS plan_id, pr.revision_number, pr.content_hash, pr.body, pr.created_at \
         FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id \
         WHERE pr.id = ?",
    )
        .bind(revision_id)
        .fetch_optional(pool)
        .await
}

pub async fn list_for_plan(pool: &SqlitePool, plan_id: i64) -> sqlx::Result<Vec<PlanRevision>> {
    sqlx::query_as::<_, PlanRevision>(
        "SELECT pr.id, s.rowid AS plan_id, pr.revision_number, pr.content_hash, pr.body, pr.created_at \
         FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id \
         WHERE s.rowid = ? ORDER BY pr.revision_number ASC",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
}

pub async fn latest_for_plan(
    pool: &SqlitePool,
    plan_id: i64,
) -> sqlx::Result<Option<PlanRevision>> {
    sqlx::query_as::<_, PlanRevision>(
        "SELECT pr.id, s.rowid AS plan_id, pr.revision_number, pr.content_hash, pr.body, pr.created_at \
         FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id \
         WHERE s.rowid = ? ORDER BY pr.revision_number DESC LIMIT 1",
    )
    .bind(plan_id)
    .fetch_optional(pool)
    .await
}

/// Fetch a revision by id, returning `None` if the id doesn't belong to a
/// plan under `session_id`. Joins through `plans.session_id` so page handlers
/// can 404 cross-session revs in one round trip.
pub async fn fetch_in_session(
    pool: &SqlitePool,
    session_id: &str,
    revision_id: i64,
) -> sqlx::Result<Option<PlanRevision>> {
    sqlx::query_as::<_, PlanRevision>(
        "SELECT pr.id, s.rowid AS plan_id, pr.revision_number, pr.content_hash, pr.body, pr.created_at \
         FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id \
         WHERE pr.id = ? AND pr.session_id = ?",
    )
    .bind(revision_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

/// The revision immediately preceding `revision_number` in the same plan
/// (i.e. `revision_number - 1`). Returns `None` for revision #1.
pub async fn previous_in_plan(
    pool: &SqlitePool,
    plan_id: i64,
    revision_number: i64,
) -> sqlx::Result<Option<PlanRevision>> {
    if revision_number <= 1 {
        return Ok(None);
    }
    sqlx::query_as::<_, PlanRevision>(
        "SELECT pr.id, s.rowid AS plan_id, pr.revision_number, pr.content_hash, pr.body, pr.created_at \
         FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id \
         WHERE s.rowid = ? AND pr.revision_number = ?",
    )
    .bind(plan_id)
    .bind(revision_number - 1)
    .fetch_optional(pool)
    .await
}

pub async fn count_for_plan(pool: &sqlx::SqlitePool, plan_id: i64) -> sqlx::Result<i64> {
    let c: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id WHERE s.rowid = ?",
    )
        .bind(plan_id)
        .fetch_one(pool)
        .await?;
    Ok(c)
}

/// Insert a new revision under the session identified by `plan_id`.
/// Revision number = `max + 1` per session.
/// Returns `(new_revision_id, new_revision_number)`.
pub async fn append(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    plan_id: i64,
    content_hash: &str,
    body: &str,
    created_at: i64,
) -> sqlx::Result<(i64, i64)> {
    let result = sqlx::query(
        "INSERT INTO plan_revisions (session_id, revision_number, content_hash, body, created_at) \
         VALUES ((SELECT id FROM sessions WHERE rowid = ?), \
                 COALESCE((SELECT MAX(pr.revision_number) FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id WHERE s.rowid = ?), 0) + 1, \
                 ?, ?, ?)",
    )
    .bind(plan_id)
    .bind(plan_id)
    .bind(content_hash)
    .bind(body)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    let id = result.last_insert_rowid();
    let (rev_num,): (i64,) =
        sqlx::query_as("SELECT revision_number FROM plan_revisions WHERE id = ?")
            .bind(id)
            .fetch_one(&mut **tx)
            .await?;
    Ok((id, rev_num))
}
