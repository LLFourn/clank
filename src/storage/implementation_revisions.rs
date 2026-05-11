//! CRUD on the `implementation_revisions` table. A row is a registered git
//! commit snapshot inside a single `plan_id`.

use sqlx::SqlitePool;

use crate::lifecycle::{CommitSha, CommitSnapshot};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ImplementationRevision {
    pub id: i64,
    pub plan_id: i64,
    pub commit_sha: String,
    pub parent_sha: Option<String>,
    pub branch: Option<String>,
    pub commit_message: String,
    pub diff_stat: String,
    pub worktree_status: Option<String>,
    pub is_head: i64,
    pub registered_by: String,
    pub created_at: i64,
}

pub async fn fetch(pool: &SqlitePool, id: i64) -> sqlx::Result<Option<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn fetch_by_sha(
    pool: &SqlitePool,
    plan_id: i64,
    commit_sha: &CommitSha,
) -> sqlx::Result<Option<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE plan_id = ? AND commit_sha = ?",
    )
    .bind(plan_id)
    .bind(commit_sha.as_str())
    .fetch_optional(pool)
    .await
}

pub async fn list_for_plan(
    pool: &SqlitePool,
    plan_id: i64,
) -> sqlx::Result<Vec<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE plan_id = ? ORDER BY id ASC",
    )
    .bind(plan_id)
    .fetch_all(pool)
    .await
}

pub async fn latest_for_plan(
    pool: &SqlitePool,
    plan_id: i64,
) -> sqlx::Result<Option<ImplementationRevision>> {
    sqlx::query_as::<_, ImplementationRevision>(
        "SELECT * FROM implementation_revisions WHERE plan_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(plan_id)
    .fetch_optional(pool)
    .await
}

/// Insert from a reducer-emitted `CommitSnapshot`. Returns the new row id.
pub async fn append<'e, E>(
    executor: E,
    plan_id: i64,
    commit: &CommitSnapshot,
    registered_by: &str,
    created_at: i64,
) -> sqlx::Result<i64>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let result = sqlx::query(
        "INSERT INTO implementation_revisions \
         (plan_id, commit_sha, parent_sha, branch, commit_message, diff_stat, worktree_status, is_head, registered_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(plan_id)
    .bind(commit.sha.as_str())
    .bind(commit.parent_sha.as_ref().map(|p| p.as_str()))
    .bind(commit.branch.as_deref())
    .bind(&commit.message)
    .bind(&commit.diff_stat)
    .bind(commit.worktree_status.as_deref())
    .bind(if commit.is_head { 1_i64 } else { 0 })
    .bind(registered_by)
    .bind(created_at)
    .execute(executor)
    .await?;
    Ok(result.last_insert_rowid())
}
