//! Integration tests for `clank feedback write`.

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

fn one_plan_repo() -> (tempfile::TempDir, String) {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);
    (dir, sha)
}

fn run_feedback_write(repo: &Path, args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let home = tempfile::tempdir().expect("isolated test HOME");
    let output = Command::new(clank_bin())
        .arg("feedback")
        .arg("write")
        .arg("--repo")
        .arg(repo)
        .env("HOME", home.path())
        .args(args)
        .output()
        .expect("spawn clank feedback write");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn writes_approve_feedback_to_flat_path() {
    let (dir, sha) = one_plan_repo();
    let repo = dir.path();
    let short = &sha[..7];

    let (status, stdout, stderr) = run_feedback_write(
        repo,
        &[
            "--commit",
            short,
            "--verdict",
            "approve",
            "--author",
            "alice",
            "-m",
            "lgtm",
        ],
    );

    assert!(status.success(), "exit={status:?} stderr={stderr}");
    let expected_rel = format!(".clank/agents/alice/feedback/{short}.md");
    assert!(
        stdout.trim().ends_with(&expected_rel),
        "stdout did not advertise expected path: {stdout}",
    );
    let abs = repo.join(&expected_rel);
    let written = std::fs::read_to_string(&abs).expect("file exists");
    assert_eq!(written, "APPROVE lgtm\n");
}

#[test]
fn writes_finished_feedback() {
    let (dir, sha) = one_plan_repo();
    let repo = dir.path();
    let short = &sha[..7];

    let (status, _stdout, stderr) = run_feedback_write(
        repo,
        &[
            "--commit",
            short,
            "--verdict",
            "finished",
            "--author",
            "alice",
            "-m",
            "ship it",
        ],
    );

    assert!(status.success(), "exit={status:?} stderr={stderr}");
    let abs = repo.join(format!(".clank/agents/alice/feedback/{short}.md"));
    let written = std::fs::read_to_string(&abs).expect("file exists");
    assert_eq!(written, "FINISHED ship it\n");
}

#[test]
fn prepends_request_changes_verdict() {
    let (dir, sha) = one_plan_repo();
    let repo = dir.path();
    let short = &sha[..7];

    let (status, _stdout, _stderr) = run_feedback_write(
        repo,
        &[
            "--commit",
            short,
            "--verdict",
            "request-changes",
            "--author",
            "alice",
            "-m",
            "overwrought API in foo.rs\n\n- [P1] details",
        ],
    );

    assert!(status.success());
    let abs = repo.join(format!(".clank/agents/alice/feedback/{short}.md"));
    let written = std::fs::read_to_string(&abs).expect("file exists");
    assert!(
        written.starts_with("REQUEST_CHANGES overwrought API in foo.rs"),
        "verdict should be prepended: {written}",
    );
}

#[test]
fn errors_on_unknown_commit_ref() {
    let (dir, _sha) = one_plan_repo();
    let repo = dir.path();

    let (status, _stdout, stderr) = run_feedback_write(
        repo,
        &[
            "--commit",
            "deadbeef",
            "--verdict",
            "approve",
            "--author",
            "alice",
            "-m",
            "lgtm",
        ],
    );

    assert!(!status.success());
    assert!(
        stderr.contains("did not match any known commit"),
        "stderr missing resolution diag: {stderr}",
    );
}
