//! Integration tests for `clank log` and `clank status` trim.

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
    Command::new(clank_bin())
        .args(args)
        .arg("--repo")
        .arg(repo)
        .env("HOME", repo)
        .output()
        .expect("spawn clank")
}

// ---- status trim tests ----

#[test]
fn status_shows_multiple_active_plans_without_error() {
    let dir = init_repo();
    let repo = dir.path();

    write(repo, ".clank/plans/alpha.md", "# alpha\n");
    write(repo, ".clank/plans/beta.md", "# beta\n");
    commit(repo, "[alpha,beta] intro both");

    let out = run_clank(repo, &["status"]);
    assert!(
        out.status.success(),
        "status should succeed with multiple plans; exit={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("alpha"), "should show alpha; got:\n{stdout}");
    assert!(stdout.contains("beta"), "should show beta; got:\n{stdout}");
}

#[test]
fn status_active_plans_hide_finished() {
    let dir = init_repo();
    let repo = dir.path();

    write(repo, ".clank/plans/old.md", "# old\n");
    commit(repo, "[old] intro");
    write(repo, ".clank/finished/old.md", "# old\n");
    git(repo, &["rm", "--quiet", ".clank/plans/old.md"]);
    commit(repo, "Finish old");

    write(repo, ".clank/plans/active.md", "# active\n");
    commit(repo, "[active] intro");

    let out = run_clank(repo, &["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("active"), "should show active; got:\n{stdout}");
    assert!(!stdout.contains("finished"), "should not show finished section; got:\n{stdout}");

    let out = run_clank(repo, &["status", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(json["finished_plans"].as_array().unwrap().is_empty());
}

#[test]
fn status_no_active_shows_last_finished() {
    let dir = init_repo();
    let repo = dir.path();

    write(repo, ".clank/plans/first.md", "# first\n");
    commit(repo, "[first] intro");
    write(repo, ".clank/finished/first.md", "# first\n");
    git(repo, &["rm", "--quiet", ".clank/plans/first.md"]);
    commit(repo, "Finish first");

    write(repo, ".clank/plans/second.md", "# second\n");
    commit(repo, "[second] intro");
    write(repo, ".clank/finished/second.md", "# second\n");
    git(repo, &["rm", "--quiet", ".clank/plans/second.md"]);
    commit(repo, "Finish second");

    let out = run_clank(repo, &["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("last finished: second"), "should show last finished; got:\n{stdout}");
    assert!(!stdout.contains("first"), "should not show older finished; got:\n{stdout}");

    let out = run_clank(repo, &["status", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let fp = json["finished_plans"].as_array().unwrap();
    assert_eq!(fp.len(), 1);
    assert_eq!(fp[0]["plan"], "second");
}

// ---- log tests ----

#[test]
fn log_shows_intro_and_reviews_for_active_plan() {
    let dir = init_repo();
    let repo = dir.path();

    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n\nlgtm\n",
    );

    let out = run_clank(repo, &["log"]);
    assert!(out.status.success(), "log failed: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[foo] intro"),
        "should show intro commit; got:\n{stdout}"
    );
    assert!(
        stdout.contains("alice") && stdout.contains("approve"),
        "should show alice's approval; got:\n{stdout}"
    );
}

#[test]
fn log_json_emits_review_as_separate_event() {
    let dir = init_repo();
    let repo = dir.path();

    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "REQUEST_CHANGES\n\nfix it\n",
    );

    let out = run_clank(repo, &["log", "--json"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let events: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("valid JSON");

    let review_events: Vec<_> = events.iter().filter(|e| e["kind"] == "review").collect();
    assert!(
        !review_events.is_empty(),
        "should have typed review events; got:\n{stdout}"
    );
    assert_eq!(review_events[0]["author"], "alice");
    assert_eq!(review_events[0]["verdict"], "request_changes");
}

#[test]
fn log_finished_plan_shows_full_timeline() {
    let dir = init_repo();
    let repo = dir.path();

    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");
    let intro_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/codex/feedback/{intro_sha}.md"),
        "APPROVE\n\nok\n",
    );

    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[bar] implement");
    let impl_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/codex/feedback/{impl_sha}.md"),
        "APPROVE\n\nimpl ok\n",
    );

    // Move plan file to finished/ to trigger Finish detection.
    write(repo, ".clank/finished/bar.md", "# bar\n");
    git(repo, &["rm", "--quiet", ".clank/plans/bar.md"]);
    commit(repo, "[bar] finish");

    let out = run_clank(repo, &["log", "--plan", "bar"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[bar] intro"),
        "should include intro; got:\n{stdout}"
    );
    assert!(
        stdout.contains("[bar] implement"),
        "should include impl; got:\n{stdout}"
    );
    assert!(
        stdout.contains("finalize"),
        "should include finalize; got:\n{stdout}"
    );
    assert!(
        stdout.contains("codex") && stdout.contains("approve"),
        "should include reviews; got:\n{stdout}"
    );
}
