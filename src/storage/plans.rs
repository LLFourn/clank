use std::path::{Path, PathBuf};

use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Plan {
    pub id: String,
    pub repo_root: String,
    pub plan_path: Option<String>,
    pub display_title: Option<String>,
    pub state: String,
    pub master_agent_id: Option<i64>,
    pub current_implementation_id: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
}

impl Plan {
    pub fn plan_path(&self) -> Option<PathBuf> {
        self.plan_path.as_ref().map(PathBuf::from)
    }
}

pub fn compute_plan_id(repo_root: &Path, plan_path: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(repo_root.as_os_str().as_encoded_bytes());
    hasher.update(b"\0");
    hasher.update(plan_path.as_os_str().as_encoded_bytes());
    hasher.finalize().to_hex().to_string()
}

pub async fn fetch(pool: &SqlitePool, plan_id: &str) -> sqlx::Result<Option<Plan>> {
    sqlx::query_as::<_, Plan>("SELECT * FROM plans WHERE id = ?")
        .bind(plan_id)
        .fetch_optional(pool)
        .await
}

pub async fn list_active(pool: &SqlitePool) -> sqlx::Result<Vec<Plan>> {
    sqlx::query_as::<_, Plan>(
        "SELECT * FROM plans WHERE archived_at IS NULL ORDER BY updated_at DESC",
    )
    .fetch_all(pool)
    .await
}

pub async fn list_by_repo_root(pool: &SqlitePool, repo_root: &str) -> sqlx::Result<Vec<Plan>> {
    sqlx::query_as::<_, Plan>(
        "SELECT * FROM plans WHERE repo_root = ? AND archived_at IS NULL ORDER BY updated_at DESC",
    )
    .bind(repo_root)
    .fetch_all(pool)
    .await
}

pub async fn insert<'e, E>(executor: E, plan: &Plan) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query(
        "INSERT INTO plans (id, repo_root, plan_path, display_title, state, \
         master_agent_id, current_implementation_id, created_at, updated_at, archived_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&plan.id)
    .bind(&plan.repo_root)
    .bind(&plan.plan_path)
    .bind(&plan.display_title)
    .bind(&plan.state)
    .bind(plan.master_agent_id)
    .bind(plan.current_implementation_id)
    .bind(plan.created_at)
    .bind(plan.updated_at)
    .bind(plan.archived_at)
    .execute(executor)
    .await?;
    Ok(())
}

pub async fn set_master_agent<'e, E>(
    executor: E,
    plan_id: &str,
    master_agent_id: i64,
    now: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE plans SET master_agent_id = ?, updated_at = ? WHERE id = ?")
        .bind(master_agent_id)
        .bind(now)
        .bind(plan_id)
        .execute(executor)
        .await?;
    Ok(())
}

pub async fn touch_updated_at<'e, E>(executor: E, plan_id: &str, now: i64) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE plans SET updated_at = ? WHERE id = ?")
        .bind(now)
        .bind(plan_id)
        .execute(executor)
        .await?;
    Ok(())
}
