use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PlanRevision {
    pub id: i64,
    pub plan_id: String,
    pub revision_number: i64,
    pub content_hash: String,
    pub body: String,
    pub created_at: i64,
    pub detected_by: String,
}

pub fn compute_content_hash(body: &str) -> String {
    blake3::hash(body.as_bytes()).to_hex().to_string()
}

pub async fn latest(pool: &SqlitePool, plan_id: &str) -> sqlx::Result<Option<PlanRevision>> {
    sqlx::query_as::<_, PlanRevision>(
        "SELECT * FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number DESC LIMIT 1",
    )
    .bind(plan_id)
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &SqlitePool, plan_id: &str) -> sqlx::Result<Vec<PlanRevision>> {
    sqlx::query_as::<_, PlanRevision>(
        "SELECT * FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number ASC",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
}

/// Insert a new revision with `revision_number = max + 1`. Returns the new row id.
pub async fn append<'e, E>(
    executor: E,
    plan_id: &str,
    content_hash: &str,
    body: &str,
    created_at: i64,
    detected_by: &str,
) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO plan_revisions (plan_id, revision_number, content_hash, body, created_at, detected_by) \
         VALUES (?, COALESCE((SELECT MAX(revision_number) FROM plan_revisions WHERE plan_id = ?), 0) + 1, ?, ?, ?, ?)",
    )
    .bind(plan_id)
    .bind(plan_id)
    .bind(content_hash)
    .bind(body)
    .bind(created_at)
    .bind(detected_by)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}
