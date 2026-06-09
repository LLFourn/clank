//! Integration tests for `clank unfinish` rewriting history.

use std::path::Path;
use std::process::Command;

fn clank_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clank")
}

fn git(repo: &Path, args: &[&str]) {
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
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@test"]);
    git(path, &["config", "user.name", "test"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    dir
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn commit(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", msg]);
}

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn run_clank(repo: &Path, args: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().expect("isolated test HOME");
    Command::new(clank_bin())
        .args(args)
        .arg("--repo")
        .arg(repo)
        .env("HOME", home.path())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank")
}

/// Seed a repo + plan + a hand-rolled finish commit (rename
/// from plans/ to finished/). Skips `clank finish` to keep the
/// setup self-contained — that command requires a FINISHED
/// feedback file and would also write to `.clank/cache/`,
/// which would make the post-finish worktree look dirty.
/// Returns the pre-finish HEAD sha so tests can assert exact
/// restoration.
fn seed_finished_plan(stem: &str) -> (tempfile::TempDir, String) {
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        &format!(".clank/plans/{stem}.md"),
        &format!("# {stem}\n"),
    );
    commit(repo, &format!("[{stem}] intro"));
    let pre_finish = head_sha(repo);

    write(
        repo,
        &format!(".clank/finished/{stem}.md"),
        &format!("# {stem}\n"),
    );
    git(repo, &["rm", "--quiet", &format!(".clank/plans/{stem}.md")]);
    git(repo, &["add", &format!(".clank/finished/{stem}.md")]);
    git(
        repo,
        &["commit", "--quiet", "-m", &format!("[{stem}] finish")],
    );

    (dir, pre_finish)
}

#[test]
fn unfinish_drops_finish_commit_and_restores_plan_file() {
    let (dir, pre_finish) = seed_finished_plan("foo");
    let repo = dir.path();
    let finish_sha = head_sha(repo);
    assert_ne!(finish_sha, pre_finish);

    let out = run_clank(repo, &["unfinish", "foo"]);
    assert!(
        out.status.success(),
        "clank unfinish failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after = head_sha(repo);
    assert_eq!(
        after, pre_finish,
        "HEAD should equal pre-finish sha after unfinish"
    );
    assert!(
        repo.join(".clank/plans/foo.md").is_file(),
        "plan file should be back under plans/"
    );
    assert!(
        !repo.join(".clank/finished/foo.md").exists(),
        "finished marker should be gone"
    );
}

#[test]
fn unfinish_refuses_when_worktree_dirty_unstaged() {
    let (dir, _pre) = seed_finished_plan("foo");
    let repo = dir.path();
    let finish_sha = head_sha(repo);
    write(repo, "src/lib.rs", "// in-flight\n");

    let out = run_clank(repo, &["unfinish", "foo"]);
    assert!(
        !out.status.success(),
        "unfinish must refuse a dirty worktree"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("dirty") || stderr.contains("stash") || stderr.contains("commit"),
        "error should explain how to recover; got: {stderr}"
    );
    assert_eq!(
        head_sha(repo),
        finish_sha,
        "HEAD must NOT move when the precondition fails"
    );
    assert!(
        repo.join("src/lib.rs").is_file(),
        "user's edit must be preserved"
    );
}

#[test]
fn unfinish_refuses_when_index_dirty_staged() {
    let (dir, _pre) = seed_finished_plan("foo");
    let repo = dir.path();
    let finish_sha = head_sha(repo);
    write(repo, "src/lib.rs", "// staged\n");
    git(repo, &["add", "src/lib.rs"]);

    let out = run_clank(repo, &["unfinish", "foo"]);
    assert!(!out.status.success(), "unfinish must refuse a dirty index");
    assert_eq!(
        head_sha(repo),
        finish_sha,
        "HEAD must NOT move with dirty index"
    );
}

#[test]
fn unfinish_refuses_when_head_is_not_a_finish_commit() {
    let (dir, _pre) = seed_finished_plan("foo");
    let repo = dir.path();
    // Land an unrelated commit on top.
    write(repo, "src/lib.rs", "// later work\n");
    commit(repo, "unrelated impl");
    let new_head = head_sha(repo);

    let out = run_clank(repo, &["unfinish", "foo"]);
    assert!(
        !out.status.success(),
        "unfinish must refuse when HEAD isn't the finish commit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not at HEAD") || stderr.contains("finish for"),
        "error should explain the HEAD mismatch; got: {stderr}"
    );
    assert_eq!(head_sha(repo), new_head, "HEAD must NOT move");
}

#[test]
fn unfinish_refuses_add_only_finished_marker() {
    // A commit that ADDs .clank/finished/<stem>.md without
    // deleting .clank/plans/<stem>.md isn't a trivial rename;
    // dropping it would obliterate the finished marker but
    // leave the plan file behind, no inverse semantics.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    // Add ONLY the finished marker, leaving the plans file in
    // place.
    write(repo, ".clank/finished/foo.md", "# foo\n");
    git(repo, &["add", ".clank/finished/foo.md"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] add-only"]);

    let before = head_sha(repo);
    let out = run_clank(repo, &["unfinish", "foo"]);
    assert!(
        !out.status.success(),
        "unfinish must refuse an add-only finish commit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("did not delete") || stderr.contains("trivial"),
        "error should explain the missing delete; got: {stderr}"
    );
    assert_eq!(head_sha(repo), before, "HEAD must NOT move");
}

#[test]
fn unfinish_refuses_content_mismatched_rename() {
    // A commit that renames plans/ → finished/ but changes the
    // file's content along the way also isn't a trivial finish.
    // `git reset --hard HEAD~` would silently lose the content
    // change.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\noriginal body\n");
    commit(repo, "[foo] intro");

    // Rename AND edit the body in the same commit.
    write(repo, ".clank/finished/foo.md", "# foo\nEDITED body\n");
    git(repo, &["rm", "--quiet", ".clank/plans/foo.md"]);
    git(repo, &["add", ".clank/finished/foo.md"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] finish+edit"]);

    let before = head_sha(repo);
    let out = run_clank(repo, &["unfinish", "foo"]);
    assert!(
        !out.status.success(),
        "unfinish must refuse a rename-with-edit"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("changed plan body") || stderr.contains("manually"),
        "error should explain the content mismatch; got: {stderr}"
    );
    assert_eq!(head_sha(repo), before, "HEAD must NOT move");
}

#[test]
fn unfinish_refuses_when_finish_commit_has_unrelated_changes() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    // Hand-craft a commit that ADDs the finished marker AND
    // modifies an unrelated file in the same commit. clank
    // finish never does this; we're simulating a user
    // hand-edit to verify the precondition fires.
    write(repo, ".clank/finished/foo.md", "# foo\n");
    git(repo, &["rm", "--quiet", ".clank/plans/foo.md"]);
    git(repo, &["add", ".clank/finished/foo.md"]);
    write(repo, "src/lib.rs", "// unrelated\n");
    git(repo, &["add", "src/lib.rs"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] finish + extra"]);

    let finish_sha = head_sha(repo);
    let out = run_clank(repo, &["unfinish", "foo"]);
    assert!(
        !out.status.success(),
        "unfinish must refuse when the finish commit carries unrelated changes"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("beyond the rename") || stderr.contains("manually"),
        "error should explain the unrelated changes; got: {stderr}"
    );
    assert_eq!(head_sha(repo), finish_sha, "HEAD must NOT move");
}
