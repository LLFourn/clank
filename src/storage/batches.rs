use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DirectiveBatch {
    pub id: i64,
    pub plan_id: String,
    pub target_kind: Option<String>,
    pub created_at: i64,
    pub delivered_by: String,
    pub acked_at: Option<i64>,
    pub acked_by: Option<String>,
}

pub async fn create<'e, E>(
    executor: E,
    plan_id: &str,
    target_kind: &str,
    delivered_by: &str,
    now: i64,
) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO directive_batches (plan_id, target_kind, created_at, delivered_by) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(plan_id)
    .bind(target_kind)
    .bind(now)
    .bind(delivered_by)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn add_item<'e, E>(executor: E, batch_id: i64, event_id: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("INSERT INTO directive_batch_items (batch_id, event_id) VALUES (?, ?)")
        .bind(batch_id)
        .bind(event_id)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn oldest_unacked(
    pool: &SqlitePool,
    plan_id: &str,
) -> sqlx::Result<Option<DirectiveBatch>> {
    sqlx::query_as::<_, DirectiveBatch>(
        "SELECT * FROM directive_batches WHERE plan_id = ? AND acked_at IS NULL \
         ORDER BY id ASC LIMIT 1",
    )
    .bind(plan_id)
    .fetch_optional(pool)
    .await
}

pub async fn fetch(pool: &SqlitePool, batch_id: i64) -> sqlx::Result<Option<DirectiveBatch>> {
    sqlx::query_as::<_, DirectiveBatch>("SELECT * FROM directive_batches WHERE id = ?")
        .bind(batch_id)
        .fetch_optional(pool)
        .await
}

pub async fn ack<'e, E>(executor: E, batch_id: i64, acked_by: &str, now: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE directive_batches SET acked_at = ?, acked_by = ? WHERE id = ?")
        .bind(now)
        .bind(acked_by)
        .bind(batch_id)
        .execute(executor)
        .await?;
    Ok(())
}
