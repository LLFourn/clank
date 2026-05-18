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
///
/// Two-phase: `git_io::snapshot` builds a `DiskSnapshot` (a sequence
/// of per-commit events plus working-tree feedback);
/// `derive_state` folds it into `RepoState`.
pub async fn rebuild_repo(repo_root: &Path) -> Result<RepoState, RebuildError> {
    let snapshot = git_io::snapshot(repo_root).await?;
    Ok(derive_state(repo_root.to_path_buf(), snapshot))
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
        assert!(state.plans.is_empty());
    }

    #[tokio::test]
    async fn single_plan_commit_creates_session() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");

        let state = rebuild_repo(dir.path()).await.unwrap();
        assert_eq!(state.plans.len(), 1);
        let session = &state.plans[&PlanKey::parse("foo").unwrap()];
        assert_eq!(session.id.as_str(), "foo");
        assert_eq!(session.body, "# foo\n");
        let revs = crate::projection::all_plan_revisions(session, &state);
        let impls = crate::projection::all_implementation_commits(session, &state);
        assert_eq!(revs, vec![session.plan_intro.clone()]);
        assert!(impls.is_empty());
    }

    #[tokio::test]
    async fn impl_commit_attributes_via_walkback() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        write_file(dir.path(), "src/lib.rs", "fn main() {}\n");
        commit(dir.path(), "Implement foo");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let plan = &state.plans[&PlanKey::parse("foo").unwrap()];
        let impls = crate::projection::all_implementation_commits(plan, &state);
        assert_eq!(impls.len(), 1);
    }

    #[tokio::test]
    async fn feedback_file_at_target_sha_loads() {
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo plan");
        let state0 = rebuild_repo(dir.path()).await.unwrap();
        let intro = state0.plans[&PlanKey::parse("foo").unwrap()]
            .plan_intro
            .clone();
        let feedback_rel = format!(".trinity/feedback/foo/{}/alice.md", intro.as_str());
        write_file(dir.path(), &feedback_rel, "APPROVE\n\nLooks good.\n");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let session = &state.plans[&PlanKey::parse("foo").unwrap()];
        let event = session.event_for(&intro).expect("event for intro");
        let gate = event.gate.as_ref().expect("gate for intro");
        let entries: Vec<_> = gate.feedback.values().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].verdict, crate::repo_state::Verdict::Approve);
    }

    // `duplicate_stem_active_and_done_lands_in_plan_conflicts_via_real_git`
    // retired with `.trinity/plans/done/` (Phase 5, event-log-and-finished).
    // Plan paths under done/ no longer parse as plan keys at all.

    #[tokio::test]
    async fn finished_plan_survives_plan_file_deletion_from_head() {
        // Monotone-finished: once frozen, deleting the plan file from
        // HEAD does NOT unfinish or hide the plan. Body renders from
        // the freeze commit, not HEAD.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo body\n");
        commit(dir.path(), "Add foo");
        write_file(
            dir.path(),
            ".trinity/finished/foo/alice.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize foo");

        let frozen_state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        assert!(frozen_state.plans[&key].is_frozen());
        assert_eq!(frozen_state.plans[&key].body, "# foo body\n");

        // Delete the plan file from HEAD.
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();
        commit(dir.path(), "Delete foo");

        let after = rebuild_repo(dir.path()).await.unwrap();
        let plan = after
            .plans
            .get(&key)
            .expect("finished plan must survive its plan-file deletion");
        assert!(
            plan.is_frozen(),
            "plan must still be frozen after HEAD-deletion"
        );
        // Body renders from the freeze commit.
        assert_eq!(plan.body, "# foo body\n");
    }

    #[tokio::test]
    async fn request_changes_in_finished_then_plan_delete_does_not_leak_a_ghost() {
        // Codex's on-chain repro for the prior placeholder design:
        // c1 adds foo. c2 adds .trinity/finished/foo/alice.md with
        // first line `REQUEST_CHANGES` (rule does not hold). c3
        // deletes the plan file. The plan never froze and HEAD has no
        // plan file, so the plan must not surface in `state.plans`.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo");
        write_file(
            dir.path(),
            ".trinity/finished/foo/alice.md",
            "REQUEST_CHANGES\n\nneeds work\n",
        );
        commit(dir.path(), "Reviewer request");
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();
        commit(dir.path(), "Delete foo");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        assert!(
            !state.plans.contains_key(&key),
            "REQUEST_CHANGES in finished/ never freezes; \
             after plan-file deletion the plan has no anchor and must not surface; \
             got {:?}",
            state.plans.keys().map(|k| k.as_str()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn delete_then_readd_same_stem_does_not_leak_prior_history() {
        // Same-stem re-add: the original foo gets deleted (unfrozen,
        // no anchor), then a fresh foo is introduced at a later
        // commit. The new Plan record has `plan_intro = re-add commit`.
        // Projections must scope to commits >= plan_intro so the
        // pre-deletion plan_touches / attribution entries (which are
        // still in state — they correctly record what happened then)
        // don't leak into the new plan's revisions / impl list.
        let dir = init_repo();
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v1\n");
        commit(dir.path(), "Add foo v1");
        write_file(dir.path(), "src/lib.rs", "fn old() {}\n");
        commit(dir.path(), "Impl on foo v1");
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();
        commit(dir.path(), "Delete foo");
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo v2 (fresh)\n");
        commit(dir.path(), "Re-add foo v2");

        let state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        let plan = state
            .plans
            .get(&key)
            .expect("re-added foo must exist in state");
        assert_eq!(plan.body, "# foo v2 (fresh)\n");

        let revisions = crate::projection::all_plan_revisions(plan, &state);
        assert_eq!(
            revisions.len(),
            1,
            "only the re-add commit is a plan revision of the fresh foo; \
             pre-deletion history must not leak. got: {revisions:?}"
        );

        let impls = crate::projection::all_implementation_commits(plan, &state);
        assert!(
            impls.is_empty(),
            "the impl commit happened during the deleted foo; \
             the fresh foo has no impl commits yet. got: {impls:?}"
        );
    }

    #[tokio::test]
    async fn finalize_on_off_chain_branch_does_not_leak_into_main() {
        // Codex's stale-branch scenario: main adds and deletes a plan
        // file without finalizing; an off-chain branch carries the
        // finalize commit. The off-chain finalize must NOT discover a
        // history-rooted placeholder for `foo` on main — main's first-
        // parent fold never sees the freeze event, and the previous
        // `git log --all` discovery would leak an empty-body active
        // plan into `state.plans`.
        let dir = init_repo();

        // main: add foo, then delete foo. No finalize commit reachable
        // from main.
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
        commit(dir.path(), "Add foo on main");
        std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();
        commit(dir.path(), "Delete foo on main");

        // Sidebar branch: create a finalize commit reachable from no
        // ancestor of main's HEAD.
        run_git(dir.path(), &["checkout", "-q", "-b", "sidebar"]);
        write_file(dir.path(), ".trinity/plans/foo.md", "# foo on sidebar\n");
        commit(dir.path(), "Re-add foo on sidebar");
        write_file(
            dir.path(),
            ".trinity/finished/foo/alice.md",
            "APPROVE\n\nlgtm\n",
        );
        commit(dir.path(), "Finalize foo on sidebar");
        run_git(dir.path(), &["checkout", "-q", "main"]);

        let state = rebuild_repo(dir.path()).await.unwrap();
        let key = PlanKey::parse("foo").unwrap();
        assert!(
            !state.plans.contains_key(&key),
            "off-chain finalize must not leak a placeholder into main's projection; got {:?}",
            state.plans.keys().map(|k| k.as_str()).collect::<Vec<_>>()
        );
    }
}
