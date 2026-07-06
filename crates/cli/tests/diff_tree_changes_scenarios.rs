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
use clank::git_io::diff_tree_changes_at;
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
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

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
    assert_eq!(changes.plan_touches.len(), 1);
    assert_eq!(changes.plan_touches[0].plan.as_str(), "foo");
    assert!(matches!(
        changes.plan_touches[0].kind,
        PlanTouchKind::Finish
    ));
}

/// Regression for codex on a6b7725: gix's `entry_mode.is_blob()`
/// returns false for symlinks and submodule commits. The legacy
/// `diff-tree --name-status` parser would still see them as leaf
/// changes — so an add/delete/modify of a symlink OUTSIDE
/// `.clank/` must register as `has_non_plan_code_changes`, and
/// a symlink touch UNDER `.clank/` must register as
/// `touched_clank`. The filter is `is_no_tree()`, not
/// `is_blob()`.
#[cfg(unix)]
#[tokio::test]
async fn symlink_outside_clank_is_code_change() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");
    // Add a symlink at repo root.
    std::os::unix::fs::symlink("README.md", repo.join("link-to-readme")).unwrap();
    commit(repo, "add symlink");

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
    assert!(
        changes.plan_touches.is_empty(),
        "symlink outside .clank/ shouldn't produce plan touches"
    );
    assert!(
        changes.has_non_plan_code_changes,
        "symlink outside .clank/ must register as code change; got {changes:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_under_clank_touches_clank() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");
    std::fs::create_dir_all(repo.join(".clank/extras")).unwrap();
    std::os::unix::fs::symlink("../../README.md", repo.join(".clank/extras/link")).unwrap();
    commit(repo, "add clank symlink");

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
    assert!(
        changes.touched_clank,
        "symlink under .clank/ must set touched_clank; got {changes:?}"
    );
    assert!(
        changes
            .clank_paths_touched
            .iter()
            .any(|p| p == ".clank/extras/link"),
        "symlink path should appear in clank_paths_touched; got {:?}",
        changes.clank_paths_touched
    );
}

#[tokio::test]
async fn finished_without_md_extension_ignored() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");
    write(repo, ".clank/finished/foo", "# foo no ext\n");
    commit(repo, "add finished-no-md");

    let changes = diff_tree_changes_at(repo, &head_sha(repo)).unwrap();
    assert!(changes.plan_touches.is_empty());
    assert!(!changes.has_non_plan_code_changes);
}

// ── tui-commit-overlay-stats: commit_numstat ────────────────────

#[tokio::test]
async fn numstat_counts_lines_per_file_vs_first_parent() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "src/a.rs", "one\ntwo\nthree\n");
    write(repo, "keep.txt", "kept\n");
    commit(repo, "base");
    write(repo, "src/a.rs", "one\nTWO\nthree\nfour\n"); // ~ +2 −1
    write(repo, "src/new.rs", "fresh\nfile\n"); // +2 −0
    std::fs::remove_file(repo.join("keep.txt")).unwrap(); // +0 −1
    commit(repo, "change");

    let stats = clank::git_io::commit_numstat_at(repo, &head_sha(repo)).unwrap();
    let by_path: std::collections::BTreeMap<_, _> = stats
        .iter()
        .map(|s| (s.path.as_str(), (s.added, s.removed)))
        .collect();
    assert_eq!(by_path["src/a.rs"], (Some(2), Some(1)), "{stats:?}");
    assert_eq!(by_path["src/new.rs"], (Some(2), Some(0)));
    assert_eq!(by_path["keep.txt"], (Some(0), Some(1)));
    // Untouched files never appear.
    assert_eq!(stats.len(), 3, "{stats:?}");
}

#[tokio::test]
async fn numstat_reports_binary_files_as_none_counts() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "readme.md", "text\n");
    commit(repo, "base");
    std::fs::write(repo.join("blob.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
    commit(repo, "add binary");

    let stats = clank::git_io::commit_numstat_at(repo, &head_sha(repo)).unwrap();
    let bin = stats.iter().find(|s| s.path == "blob.bin").unwrap();
    assert_eq!((bin.added, bin.removed), (None, None), "git shows `-`");
}

#[tokio::test]
async fn numstat_root_commit_diffs_against_the_empty_tree() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "a.txt", "l1\nl2\n");
    commit(repo, "root");
    let stats = clank::git_io::commit_numstat_at(repo, &head_sha(repo)).unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!((stats[0].added, stats[0].removed), (Some(2), Some(0)));
}
