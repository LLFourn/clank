//! Cold-start / HEAD-change rebuild. Composes:
//!
//! 1. `git_io::snapshot` → `CommitSnapshot`
//! 2. `disk_snapshot::derive_state` → [`RepoState`] (sans-io fold)
//!    — or, when a valid exact-HEAD or ancestor cache is available,
//!    load from `state_cache` (and fold-forward if needed).
//!
//! Feedback files are NOT folded into state; consumers project them
//! on demand via `git_io::collect_feedback_files`.

use std::path::Path;

use crate::disk_snapshot::{apply_commit, derive_state};
use crate::git_io::{self, GitIoError};
use crate::lifecycle::CommitSha;
use crate::repo_state::RepoState;
use crate::state_cache;
use clank_core::repo_state::LogEvent;

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("git io: {0}")]
    Git(#[from] GitIoError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    Use,
    Bypass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebuildDiagnostics {
    pub cache_hit: bool,
}

pub async fn rebuild_repo(repo_root: &Path) -> Result<RepoState, RebuildError> {
    rebuild_repo_with_policy(repo_root, CachePolicy::Use).await
}

/// Rebuild a range `(from, to]` collecting log events. `from` is
/// exclusive (`None` = repo root), `to` is inclusive. Uses the cache
/// to skip history before `from`; the cache is invisible to callers.
pub async fn rebuild_from(
    repo_root: &Path,
    from: Option<&CommitSha>,
    to: &CommitSha,
) -> Result<(RepoState, Vec<LogEvent>), RebuildError> {
    // Find the best cache at or before `from`. When `from=None`
    // (from root), no cache helps — we need to fold everything
    // to collect all events.
    let mut best_cache: Option<(CommitSha, RepoState)> = None;
    if let Some(f) = from {
        for cached_head in state_cache::list_cached_heads(repo_root) {
            let usable = &cached_head == f
                || git_io::is_ancestor(repo_root, &cached_head, f).unwrap_or(false);
            if !usable {
                continue;
            }
            if let Ok(Some(state)) = state_cache::try_load(repo_root, &cached_head) {
                best_cache = Some((cached_head, state));
                break;
            }
        }
    }

    let mut state = match best_cache {
        Some((_ch, s)) => s,
        None => RepoState::empty(repo_root.to_path_buf()),
    };

    // Phase 1 (silent): fold from current state head through `from`.
    // This builds fold context without emitting log events.
    let silent_target = from.cloned();
    if let Some(ref target) = silent_target {
        let base = state.head.clone();
        for event in git_io::commit_events_between(repo_root, base.as_ref(), target)? {
            let _ = apply_commit(&mut state, &event);
        }
        state.head = Some(target.clone());
    }

    // Phase 2 (collecting): fold from `from` (exclusive) through `to` (inclusive).
    let base = state.head.clone();
    let mut log_events = Vec::new();
    for event in git_io::commit_events_between(repo_root, base.as_ref(), to)? {
        log_events.extend(apply_commit(&mut state, &event));
    }
    state.head = Some(to.clone());

    Ok((state, log_events))
}

pub async fn rebuild_repo_with_policy(
    repo_root: &Path,
    policy: CachePolicy,
) -> Result<RepoState, RebuildError> {
    let (state, _diag) = rebuild_with_diagnostics(repo_root, policy).await?;
    Ok(state)
}

pub async fn rebuild_with_diagnostics(
    repo_root: &Path,
    policy: CachePolicy,
) -> Result<(RepoState, RebuildDiagnostics), RebuildError> {
    let head = git_io::rev_parse_head(repo_root)?;

    if policy == CachePolicy::Use
        && let Some(ref h) = head
    {
        // Exact-HEAD match.
        match state_cache::try_load(repo_root, h) {
            Ok(Some(state)) => return Ok((state, RebuildDiagnostics { cache_hit: true })),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    repo = %repo_root.display(),
                    head = %h,
                    error = ?e,
                    "exact-HEAD cache load failed; trying ancestor cache",
                );
            }
        }
        // Phase 2: ancestor match — fold-forward from a cached ancestor.
        for cached_head in state_cache::list_cached_heads(repo_root) {
            if &cached_head == h {
                continue;
            }
            match git_io::is_ancestor(repo_root, &cached_head, h) {
                Ok(true) => {}
                _ => continue,
            }
            match state_cache::try_load(repo_root, &cached_head) {
                Ok(Some(mut state)) => match fold_forward(repo_root, &mut state, h).await {
                    Ok(()) => {
                        write_and_prune(repo_root, &state);
                        return Ok((state, RebuildDiagnostics { cache_hit: true }));
                    }
                    Err(e) => {
                        tracing::warn!(
                            repo = %repo_root.display(),
                            from = %cached_head,
                            to = %h,
                            error = ?e,
                            "incremental fold-forward failed; falling back to cold fold",
                        );
                    }
                },
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(
                        repo = %repo_root.display(),
                        cached_head = %cached_head,
                        error = ?e,
                        "ancestor cache load failed; trying next candidate",
                    );
                }
            }
        }
    }

    let snapshot = git_io::snapshot(repo_root)?;
    let state = derive_state(repo_root.to_path_buf(), snapshot).await?;

    if policy == CachePolicy::Use {
        write_and_prune(repo_root, &state);
    }

    Ok((state, RebuildDiagnostics { cache_hit: false }))
}

fn write_and_prune(repo_root: &Path, state: &RepoState) {
    if let Err(e) = state_cache::write(repo_root, state) {
        tracing::warn!(
            repo = %repo_root.display(),
            error = ?e,
            "state cache write failed",
        );
    }
    if let Err(e) = state_cache::prune(repo_root) {
        tracing::warn!(
            repo = %repo_root.display(),
            error = ?e,
            "state cache prune failed",
        );
    }
}

/// Walk commits from `state.head` (exclusive) to `target_head`
/// (inclusive), applying each through the sans-io fold.
async fn fold_forward(
    repo_root: &Path,
    state: &mut RepoState,
    target_head: &CommitSha,
) -> Result<(), GitIoError> {
    let Some(base) = state.head.clone() else {
        return Err(GitIoError::Parse {
            context: "fold_forward".into(),
            detail: "cached state has no head".into(),
        });
    };
    for event in git_io::commit_events_between(repo_root, Some(&base), target_head)? {
        apply_commit(state, &event);
    }
    state.head = Some(target_head.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::PlanKey;
    use std::path::Path;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        run_git(path, &["init", "--quiet", "--initial-branch=main"]);
        run_git(path, &["config", "user.email", "test@test"]);
        run_git(path, &["config", "user.name", "test"]);
        run_git(path, &["config", "commit.gpgsign", "false"]);
        dir
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn write_file(repo: &Path, rel: &str, body: &str) {
        let abs = repo.join(rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(abs, body).unwrap();
    }

    fn commit(repo: &Path, msg: &str) {
        run_git(repo, &["add", "-A"]);
        run_git(repo, &["commit", "--quiet", "-m", msg]);
    }

    #[tokio::test]
    async fn empty_repo_yields_empty_state() {
        let dir = init_repo();
        let state = rebuild_repo(dir.path()).await.unwrap();
        assert!(state.head.is_none());
        assert!(state.fold.plans.is_empty());
    }

    #[tokio::test]
    async fn single_plan_intro_appears_in_fold() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        assert!(state.fold.plans.contains_key(&key));
        assert!(state.fold.plans[&key].commits[0].touched_plan);
    }

    #[tokio::test]
    async fn finalize_moves_plan_to_finished() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        // Move the plan file to finished/ to trigger Finish detection.
        write_file(dir.path(), ".clank/finished/foo.md", "# foo\n");
        run_git(dir.path(), &["rm", "--quiet", ".clank/plans/foo.md"]);
        commit(dir.path(), "[foo] finish");
        let state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        assert!(!state.fold.plans.contains_key(&key));
        assert_eq!(state.fold.finished_plans.len(), 1);
    }

    #[tokio::test]
    async fn warm_cache_skips_the_fold() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");

        let (_cold, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        assert!(!diag.cache_hit);

        let (_warm, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        assert!(diag.cache_hit);

        let (_bypass, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Bypass)
            .await
            .unwrap();
        assert!(!diag.cache_hit);
    }

    #[tokio::test]
    async fn ancestor_cache_fold_forwards() {
        let dir = init_repo();
        write_file(dir.path(), ".clank/plans/foo.md", "# foo\n");
        commit(dir.path(), "[foo] intro");
        let _ = rebuild_repo(dir.path()).await.unwrap();
        write_file(dir.path(), "src.rs", "fn main() {}\n");
        commit(dir.path(), "[foo] implement");
        let (state, diag) = rebuild_with_diagnostics(dir.path(), CachePolicy::Use)
            .await
            .unwrap();
        assert!(diag.cache_hit);
        let key = PlanKey::parse("foo").unwrap();
        assert_eq!(state.fold.plans[&key].commits.len(), 2);
    }
}
