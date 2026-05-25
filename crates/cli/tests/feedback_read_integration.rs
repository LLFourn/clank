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

#[test]
fn human_output_shows_author_verdict_summary_and_body() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/codex/feedback/{sha}.md"),
        "APPROVE clean impl\n\nNo blocking findings.\nAll good.\n",
    );

    let out = Command::new(clank_bin())
        .args(["feedback", "read", "--commit", &sha, "--repo"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("codex"), "should show author; got:\n{stdout}");
    assert!(stdout.contains("APPROVE"), "should show verdict; got:\n{stdout}");
    assert!(stdout.contains("clean impl"), "should show summary; got:\n{stdout}");
    assert!(
        stdout.contains("No blocking findings"),
        "should show body; got:\n{stdout}"
    );
}

#[test]
fn json_output_includes_summary_and_details() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{sha}.md"),
        "REQUEST_CHANGES overwrought API\n\n- [P1] simplify\n- [P2] rename\n",
    );

    let out = Command::new(clank_bin())
        .args(["feedback", "read", "--commit", &sha, "--json", "--repo"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let entries: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["author"], "alice");
    assert_eq!(entries[0]["verdict"], "request_changes");
    assert_eq!(entries[0]["summary"], "overwrought API");
    let details = entries[0]["details"].as_str().unwrap();
    assert!(
        details.contains("simplify"),
        "details should include body; got: {details}"
    );
}

#[test]
fn short_ref_finds_full_sha_feedback() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/codex/feedback/{sha}.md"),
        "APPROVE full sha feedback\n",
    );

    let short = &sha[..7];
    let out = Command::new(clank_bin())
        .args(["feedback", "read", "--commit", short, "--repo"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("codex") && stdout.contains("APPROVE"),
        "short ref should find full-SHA feedback; got:\n{stdout}"
    );
}

#[test]
fn invalid_ref_errors() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "seed\n");
    commit(repo, "seed");

    let out = Command::new(clank_bin())
        .args(["feedback", "read", "--commit", "not-a-ref", "--repo"])
        .arg(repo)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot resolve"),
        "should error on invalid ref; got:\n{stderr}"
    );
}
