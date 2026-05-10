use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImplementationRevision {
    pub id: i64,
    pub plan_id: String,
    pub commit_sha: String,
    pub parent_sha: Option<String>,
    pub branch: Option<String>,
    pub commit_message: String,
    pub diff_stat: String,
    pub worktree_status: Option<String>,
    pub is_head: i64,
    pub registered_by: String,
    pub created_at: i64,
    pub detected_by: String,
}

pub async fn fetch_by_sha(
    pool: &SqlitePool,
    plan_id: &str,
    commit_sha: &str,
) -> sqlx::Result<Option<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE plan_id = ? AND commit_sha = ?",
    )
    .bind(plan_id)
    .bind(commit_sha)
    .fetch_optional(pool)
    .await
}

pub async fn fetch_by_id(
    pool: &SqlitePool,
    id: i64,
) -> sqlx::Result<Option<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn list(pool: &SqlitePool, plan_id: &str) -> sqlx::Result<Vec<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE plan_id = ? ORDER BY id ASC",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
}

pub async fn latest(
    pool: &SqlitePool,
    plan_id: &str,
) -> sqlx::Result<Option<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE plan_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(plan_id)
    .fetch_optional(pool)
    .await
}

pub struct NewImplementationRevision<'a> {
    pub plan_id: &'a str,
    pub commit_sha: &'a str,
    pub parent_sha: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub commit_message: &'a str,
    pub diff_stat: &'a str,
    pub worktree_status: Option<&'a str>,
    pub is_head: bool,
    pub registered_by: &'a str,
    pub detected_by: &'a str,
    pub created_at: i64,
}

pub async fn insert<'e, E>(executor: E, new: &NewImplementationRevision<'_>) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO implementation_revisions \
         (plan_id, commit_sha, parent_sha, branch, commit_message, diff_stat, \
          worktree_status, is_head, registered_by, created_at, detected_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new.plan_id)
    .bind(new.commit_sha)
    .bind(new.parent_sha)
    .bind(new.branch)
    .bind(new.commit_message)
    .bind(new.diff_stat)
    .bind(new.worktree_status)
    .bind(if new.is_head { 1_i64 } else { 0 })
    .bind(new.registered_by)
    .bind(new.created_at)
    .bind(new.detected_by)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}

pub async fn set_current<'e, E>(
    executor: E,
    plan_id: &str,
    impl_revision_id: i64,
) -> sqlx::Result<()>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    sqlx::query("UPDATE plans SET current_implementation_id = ? WHERE id = ?")
        .bind(impl_revision_id)
        .bind(plan_id)
        .execute(executor)
        .await?;
    Ok(())
}
