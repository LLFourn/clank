//! Behavioral scenarios for `git_io::diff_tree_changes`. Each test
//! builds a real git repo matching one scenario and asserts the
//! same shape the legacy `parse_diff_tree_*` unit tests did.
//!
//! Scenario names carry over one-to-one from the deleted parser
//! tests in `git_io.rs`. If a scenario regresses, the failing
//! name still grep-matches the historical bug.

use std::path::Path;
use std::process::Command;

use clank::disk_snapshot::PlanTouchKind;
use clank::git_io::diff_tree_changes;
use clank::lifecycle::CommitSha;

fn run_git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    run_git(path, &["init", "--quiet", "--initial-branch=main"]);
    run_git(path, &["config", "user.email", "test@test"]);
    run_git(path, &["config", "user.name", "test"]);
    run_git(path, &["config", "commit.gpgsign", "false"]);
    dir
}

fn write(repo: &Path, rel: &str, body: &str) {
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

fn head_sha(repo: &Path) -> CommitSha {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse");
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    CommitSha::parse(&s).expect("valid sha")
}

#[tokio::test]
async fn single_plan_intro() {
    let dir = init_repo();
    let repo = dir.path();
    // Seed commit so HEAD has a parent (root-commit semantics tested separately
    // by other suites; here we want the standard 1-parent diff).
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");

    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "intro foo");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert_eq!(changes.plan_touches.len(), 1);
    assert_eq!(changes.plan_touches[0].plan.as_str(), "foo");
    assert!(matches!(changes.plan_touches[0].kind, PlanTouchKind::Intro));
    assert_eq!(
        changes.plan_touches[0].new_path.as_deref(),
        Some(Path::new(".clank/plans/foo.md"))
    );
    assert!(!changes.has_non_plan_code_changes);
}

#[tokio::test]
async fn rename_out_of_plans_is_delete() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "intro foo");
    std::fs::create_dir_all(repo.join(".clank/plans/done")).unwrap();
    run_git(
        repo,
        &["mv", ".clank/plans/foo.md", ".clank/plans/done/foo.md"],
    );
    commit(repo, "move out");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert_eq!(changes.plan_touches.len(), 1);
    assert_eq!(changes.plan_touches[0].plan.as_str(), "foo");
    assert!(matches!(
        changes.plan_touches[0].kind,
        PlanTouchKind::Revision
    ));
    assert!(
        changes.plan_touches[0].new_path.is_none(),
        "rename out of plans/ must produce new_path=None (Delete)",
    );
}

#[tokio::test]
async fn rename_into_plans_is_intro() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/done/foo.md", "# foo\n");
    commit(repo, "seed done/foo");
    run_git(
        repo,
        &["mv", ".clank/plans/done/foo.md", ".clank/plans/foo.md"],
    );
    commit(repo, "promote foo");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert_eq!(changes.plan_touches.len(), 1);
    assert_eq!(changes.plan_touches[0].plan.as_str(), "foo");
    assert!(matches!(changes.plan_touches[0].kind, PlanTouchKind::Intro));
    assert!(changes.plan_touches[0].new_path.is_some());
}

#[tokio::test]
async fn plan_revision_with_code() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# v1\n");
    write(repo, "src/lib.rs", "// v1\n");
    commit(repo, "seed");
    write(repo, ".clank/plans/foo.md", "# v2\n");
    write(repo, "src/lib.rs", "// v2\n");
    commit(repo, "update foo and code");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert_eq!(changes.plan_touches.len(), 1);
    assert!(matches!(
        changes.plan_touches[0].kind,
        PlanTouchKind::Revision
    ));
    assert!(changes.has_non_plan_code_changes);
}

#[tokio::test]
async fn pure_code() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/foo.rs", "// v1\n");
    commit(repo, "seed src");
    write(repo, "src/foo.rs", "// v2\n");
    write(repo, "tests/bar.rs", "// added\n");
    commit(repo, "more code");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert!(changes.plan_touches.is_empty());
    assert!(changes.has_non_plan_code_changes);
}

#[tokio::test]
async fn multi_plan_touch() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo v1\n");
    write(repo, "src/lib.rs", "// v1\n");
    commit(repo, "seed foo + code");
    write(repo, ".clank/plans/foo.md", "# foo v2\n");
    write(repo, ".clank/plans/bar.md", "# bar\n");
    write(repo, "src/lib.rs", "// v2\n");
    commit(repo, "touch foo + bar + code");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert_eq!(changes.plan_touches.len(), 2);
    assert!(changes.has_non_plan_code_changes);
}

#[tokio::test]
async fn ignores_other_clank_paths() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");
    write(repo, ".clank/feedback/foo/plan/alice.md", "review\n");
    commit(repo, "add feedback");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert!(changes.plan_touches.is_empty());
    assert!(!changes.has_non_plan_code_changes);
}

#[tokio::test]
async fn finish_detected_when_plan_deleted_and_finished_added() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "intro foo");
    write(repo, ".clank/finished/foo.md", "# foo finished\n");
    std::fs::remove_file(repo.join(".clank/plans/foo.md")).unwrap();
    commit(repo, "finish foo");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert_eq!(changes.plan_touches.len(), 1);
    assert_eq!(changes.plan_touches[0].plan.as_str(), "foo");
    assert!(matches!(
        changes.plan_touches[0].kind,
        PlanTouchKind::Finish
    ));
}

#[tokio::test]
async fn finished_added_alone_is_finish() {
    // Note: rename detection with 50% similarity can convert a
    // plain Add-of-finished-only into a Rewrite from
    // .clank/plans/foo.md if both exist. This test verifies the
    // standalone case: no plan file ever existed.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");
    write(repo, ".clank/finished/foo.md", "# foo\n");
    commit(repo, "add finished only");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert_eq!(changes.plan_touches.len(), 1);
    assert_eq!(changes.plan_touches[0].plan.as_str(), "foo");
    assert!(matches!(
        changes.plan_touches[0].kind,
        PlanTouchKind::Finish
    ));
}

#[tokio::test]
async fn finished_without_md_extension_ignored() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");
    write(repo, ".clank/finished/foo", "# foo no ext\n");
    commit(repo, "add finished-no-md");

    let changes = diff_tree_changes(repo, &head_sha(repo)).await.unwrap();
    assert!(changes.plan_touches.is_empty());
    assert!(!changes.has_non_plan_code_changes);
}
