use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Agent {
    pub id: i64,
    pub plan_id: String,
    pub role: String,
    pub label: String,
    pub model_hint: Option<String>,
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
    plan_id: &str,
    label: &str,
) -> sqlx::Result<Option<Agent>> {
    sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE plan_id = ? AND label = ?")
        .bind(plan_id)
        .bind(label)
        .fetch_optional(pool)
        .await
}

pub async fn fetch_master(pool: &SqlitePool, plan_id: &str) -> sqlx::Result<Option<Agent>> {
    sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE plan_id = ? AND role = 'master' LIMIT 1")
        .bind(plan_id)
        .fetch_optional(pool)
        .await
}

pub async fn list_for_plan(pool: &SqlitePool, plan_id: &str) -> sqlx::Result<Vec<Agent>> {
    sqlx::query_as::<_, Agent>("SELECT * FROM agents WHERE plan_id = ? ORDER BY first_seen ASC")
        .bind(plan_id)
        .fetch_all(pool)
        .await
}

pub async fn insert<'e, E>(
    executor: E,
    plan_id: &str,
    role: Role,
    label: &str,
    now: i64,
) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO agents (plan_id, role, label, first_seen, last_seen) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(plan_id)
    .bind(role.as_str())
    .bind(label)
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

/// Reset all `last_seen` to a sentinel time so masters/reviewers must re-bind
/// after a daemon restart.
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
