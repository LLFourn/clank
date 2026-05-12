use std::path::{Path, PathBuf};

use tokio::process::Command;

/// Stable empty-tree SHA used to diff root commits. See `git hash-object -t
/// tree --stdin < /dev/null` — but treat the constant as the source of truth;
/// `git hash-object -t tree /dev/null` is wrong (it produces a *blob* hash).
pub const EMPTY_TREE_SHA: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git not available or command failed: {0}")]
    CommandFailed(String),
    #[error("not in a git repository: {0}")]
    NotInRepo(PathBuf),
    #[error("git command exited with non-zero status: {context}: {stderr}")]
    NonZero { context: String, stderr: String },
}

async fn run_git(cwd: &Path, args: &[&str]) -> Result<std::process::Output, GitError> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .await
        .map_err(|e| GitError::CommandFailed(format!("spawn: {e}")))
}

async fn run_git_ok(cwd: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = run_git(cwd, args).await?;
    if !output.status.success() {
        return Err(GitError::NonZero {
            context: format!("git {}", args.join(" ")),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

pub async fn rev_parse_show_toplevel(cwd: &Path) -> Result<PathBuf, GitError> {
    let output = run_git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    if !output.status.success() {
        return Err(GitError::NotInRepo(cwd.to_path_buf()));
    }
    let s = String::from_utf8_lossy(&output.stdout);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(GitError::NotInRepo(cwd.to_path_buf()));
    }
    Ok(PathBuf::from(trimmed))
}

pub async fn rev_parse_head(repo: &Path) -> Result<String, GitError> {
    run_git_ok(repo, &["rev-parse", "HEAD"]).await
}

pub async fn parent_sha(repo: &Path, sha: &str) -> Result<Option<String>, GitError> {
    let output = run_git(repo, &["rev-list", "--parents", "-n", "1", sha]).await?;
    if !output.status.success() {
        return Err(GitError::NonZero {
            context: format!("rev-list -n 1 {sha}"),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let s = String::from_utf8_lossy(&output.stdout);
    let mut parts = s.split_ascii_whitespace();
    let _self = parts.next();
    Ok(parts.next().map(str::to_string))
}

pub async fn commit_message(repo: &Path, sha: &str) -> Result<String, GitError> {
    run_git_ok(repo, &["log", "-1", "--format=%B", sha]).await
}

/// Best-effort current branch (None on detached HEAD).
pub async fn current_branch(repo: &Path) -> Result<Option<String>, GitError> {
    let output = run_git(repo, &["symbolic-ref", "--short", "-q", "HEAD"]).await?;
    if !output.status.success() {
        return Ok(None);
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { Ok(None) } else { Ok(Some(s)) }
}

pub async fn diff_stat(repo: &Path, parent: Option<&str>, sha: &str) -> Result<String, GitError> {
    let parent = parent.unwrap_or(EMPTY_TREE_SHA);
    let range = format!("{parent}..{sha}");
    run_git_ok(repo, &["diff", "--stat", &range]).await
}

pub async fn diff_text(repo: &Path, parent: Option<&str>, sha: &str) -> Result<String, GitError> {
    let parent = parent.unwrap_or(EMPTY_TREE_SHA);
    let range = format!("{parent}..{sha}");
    run_git_ok(repo, &["diff", &range]).await
}

pub async fn worktree_porcelain(repo: &Path) -> Result<String, GitError> {
    run_git_ok(repo, &["status", "--porcelain"]).await
}

/// Resolve the path to `logs/HEAD` inside the repo's git directory.
/// Uses `git rev-parse --git-path logs/HEAD` so worktree setups produce
/// the worktree's own reflog file rather than the shared `.git/logs/HEAD`.
pub async fn resolve_git_logs_head(repo: &Path) -> Result<PathBuf, GitError> {
    let raw = run_git_ok(repo, &["rev-parse", "--git-path", "logs/HEAD"]).await?;
    let p = PathBuf::from(&raw);
    if p.is_absolute() {
        Ok(p)
    } else {
        Ok(repo.join(p))
    }
}
