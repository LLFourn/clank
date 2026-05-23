//! Integration tests for `clank feedback write`.
//!
//! Spawns the real `clank` binary against a temp git repo with one
//! active plan + intro commit, then verifies the write produces the
//! expected file (or fails with the expected diagnostic).

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

/// Repo + one plan + an intro commit; returns (dir, head_sha).
fn one_plan_repo() -> (tempfile::TempDir, String) {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);
    (dir, sha)
}

fn run_feedback_write(
    repo: &Path,
    args: &[&str],
    stdin_body: &str,
) -> (std::process::ExitStatus, String, String) {
    use std::io::Write;
    let mut child = Command::new(clank_bin())
        .arg("feedback")
        .arg("write")
        .arg("--repo")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn clank feedback write");
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_body.as_bytes()).expect("write stdin");
    }
    let output = child.wait_with_output().expect("wait");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn writes_approve_feedback_to_canonical_path() {
    let (dir, sha) = one_plan_repo();
    let repo = dir.path();
    let short = &sha[..7];

    let (status, stdout, stderr) = run_feedback_write(
        repo,
        &[
            "--plan",
            "foo",
            "--commit",
            short,
            "--verdict",
            "approve",
            "--author",
            "alice",
        ],
        "APPROVE\n\nlgtm\n",
    );

    assert!(status.success(), "exit={status:?} stderr={stderr}");
    let expected_rel = format!(".clank/agents/alice/feedback/foo/{short}.md");
    assert!(
        stdout.trim().ends_with(&expected_rel),
        "stdout did not advertise expected path: {stdout}",
    );
    let abs = repo.join(&expected_rel);
    let written = std::fs::read_to_string(&abs).expect("file exists");
    assert_eq!(written, "APPROVE\n\nlgtm\n");
}

#[test]
fn rejects_mismatched_verdict() {
    let (dir, sha) = one_plan_repo();
    let repo = dir.path();
    let short = &sha[..7];

    let (status, _stdout, stderr) = run_feedback_write(
        repo,
        &[
            "--plan",
            "foo",
            "--commit",
            short,
            "--verdict",
            "approve",
            "--author",
            "alice",
        ],
        "REQUEST_CHANGES\n\nplease fix\n",
    );

    assert!(!status.success(), "expected failure, got {status:?}");
    assert!(
        stderr.contains("body validation failed"),
        "stderr missing validation msg: {stderr}",
    );
    let abs = repo.join(format!(".clank/agents/alice/feedback/foo/{short}.md"));
    assert!(
        !abs.exists(),
        "no file should be written on validation failure",
    );
}

#[test]
fn errors_on_unknown_commit_ref() {
    let (dir, _sha) = one_plan_repo();
    let repo = dir.path();

    let (status, _stdout, stderr) = run_feedback_write(
        repo,
        &[
            "--plan",
            "foo",
            "--commit",
            "deadbeef",
            "--verdict",
            "approve",
            "--author",
            "alice",
        ],
        "APPROVE\n",
    );

    assert!(!status.success());
    assert!(
        stderr.contains("did not match any reviewable commit"),
        "stderr missing resolution diag: {stderr}",
    );
}

#[test]
fn errors_on_inactive_plan() {
    let (dir, sha) = one_plan_repo();
    let repo = dir.path();

    let (status, _stdout, stderr) = run_feedback_write(
        repo,
        &[
            "--plan",
            "bar",
            "--commit",
            &sha[..7],
            "--verdict",
            "approve",
            "--author",
            "alice",
        ],
        "APPROVE\n",
    );

    assert!(!status.success());
    assert!(
        stderr.contains("not active"),
        "stderr missing inactive-plan diag: {stderr}",
    );
}
