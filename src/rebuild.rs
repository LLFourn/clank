//! Cold-start / HEAD-change rebuild. Composes `git_io::snapshot` (IO)
//! with `disk_snapshot::derive_state` (pure) into a single async entry
//! point used by the runtime.

use std::path::Path;

use crate::disk_snapshot::derive_state;
use crate::git_io::{self, GitIoError};
use crate::repo_state::RepoState;

#[derive(Debug, thiserror::Error)]
pub enum RebuildError {
    #[error("git io: {0}")]
    Git(#[from] GitIoError),
}

/// Build a fresh `RepoState` from disk + git for `repo_root`. Empty
/// repos (no commits) produce an empty state.
pub async fn rebuild_repo(repo_root: &Path) -> Result<RepoState, RebuildError> {
    let snapshot = git_io::snapshot(repo_root).await?;
    Ok(derive_state(repo_root.to_path_buf(), snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::SessionId;
    use crate::repo_state::AttributionResult;
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
        assert!(status.success(), "git {:?} failed", args);
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
        assert!(state.sessions.is_empty());
        assert!(state.attribution.is_empty());
    }

    #[tokio::test]
    async fn single_plan_commit_creates_session() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");

        let state = rebuild_repo(dir.path()).await.unwrap();
        assert_eq!(state.sessions.len(), 1);
        let session = &state.sessions[&SessionId::from("foo".to_string())];
        assert_eq!(session.id.as_str(), "foo");
        assert_eq!(session.body, "# foo\n");
        let intro = &session.plan_intro;
        let attr = state.attribution.get(intro).expect("intro in attribution");
        assert!(matches!(
            attr,
            AttributionResult::Attributed {
                session,
                plan_touch: Some(_),
                has_code_changes: false
            } if session.as_str() == "foo"
        ));
    }

    #[tokio::test]
    async fn impl_commit_attributes_via_walkback() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        write_file(dir.path(), "src/lib.rs", "fn main() {}\n");
        commit(dir.path(), "Implement foo");

        let state = rebuild_repo(dir.path()).await.unwrap();
        assert_eq!(state.attribution.len(), 2);
        let impl_attrs: Vec<_> = state
            .attribution
            .values()
            .filter(|a| matches!(
                a,
                AttributionResult::Attributed { has_code_changes: true, plan_touch: None, .. }
            ))
            .collect();
        assert_eq!(impl_attrs.len(), 1);
    }

    #[tokio::test]
    async fn feedback_file_at_target_sha_loads() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.sessions[&SessionId::from("foo".to_string())]
            .plan_intro
            .clone();
        let feedback_rel = format!(".trinity/feedback/foo/plan/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nLooks good.\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = &state.sessions[&SessionId::from("foo".to_string())];
        let entries: Vec<_> = session.plan_feedback.values().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].verdict, crate::repo_state::Verdict::Approve);
    }
}
