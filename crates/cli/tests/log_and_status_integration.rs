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
fn status_trims_finished_plans_to_three() {
    let dir = init_repo();
    let repo = dir.path();

    for i in 0..5 {
        let name = format!("plan-{i}");
        write(
            repo,
            &format!(".clank/plans/{name}.md"),
            &format!("# {name}\n"),
        );
        commit(repo, &format!("[{name}] intro"));
        write(
            repo,
            &format!(".clank/finished/{name}/codex.md"),
            "APPROVE\n\nlgtm\n",
        );
        commit(repo, &format!("Finalize {name}"));
    }

    let out = run_clank(repo, &["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("3 of 5"),
        "should show '3 of 5'; got:\n{stdout}"
    );
    assert!(
        stdout.contains("plan-4") && stdout.contains("plan-3") && stdout.contains("plan-2"),
        "should show the 3 most recent; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("plan-0") && !stdout.contains("plan-1"),
        "should NOT show older plans; got:\n{stdout}"
    );
}

#[test]
fn status_all_shows_all_finished_plans() {
    let dir = init_repo();
    let repo = dir.path();

    for i in 0..5 {
        let name = format!("plan-{i}");
        write(
            repo,
            &format!(".clank/plans/{name}.md"),
            &format!("# {name}\n"),
        );
        commit(repo, &format!("[{name}] intro"));
        write(
            repo,
            &format!(".clank/finished/{name}/codex.md"),
            "APPROVE\n\nlgtm\n",
        );
        commit(repo, &format!("Finalize {name}"));
    }

    let out = run_clank(repo, &["status", "--all"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for i in 0..5 {
        assert!(
            stdout.contains(&format!("plan-{i}")),
            "status --all should show plan-{i}; got:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("of 5"),
        "should NOT show truncation notice with --all; got:\n{stdout}"
    );
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
        &format!(".clank/agents/alice/feedback/foo/{intro_sha}.md"),
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
        &format!(".clank/agents/alice/feedback/foo/{intro_sha}.md"),
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
        &format!(".clank/agents/codex/feedback/bar/{intro_sha}.md"),
        "APPROVE\n\nok\n",
    );

    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[bar] implement");
    let impl_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/codex/feedback/bar/{impl_sha}.md"),
        "APPROVE\n\nimpl ok\n",
    );

    write(
        repo,
        ".clank/finished/bar/codex.md",
        "APPROVE\n\nfinalized\n",
    );
    commit(repo, "Finalize bar");

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
        stdout.contains("Finalize"),
        "should include finalize; got:\n{stdout}"
    );
    assert!(
        stdout.contains("codex") && stdout.contains("approve"),
        "should include reviews; got:\n{stdout}"
    );
}
