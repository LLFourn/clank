mod common;

use std::path::Path;
use std::process::Command;

use serde_json::json;

use common::TestApp;

fn make_commit(repo: &Path, name: &str, contents: &str) -> String {
    std::fs::write(repo.join(name), contents).unwrap();
    run_git(repo, &["add", name]);
    run_git(repo, &["commit", "-q", "-m", &format!("add {name}")]);
    run_git(repo, &["rev-parse", "HEAD"])
}

fn run_git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git spawn");
    if !output.status.success() {
        panic!(
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Once an implementation commit has been registered, plan-file edits no
/// longer create plan_revisions. Instead the daemon emits a
/// `plan_file_changed_after_implementation` event so the human can decide what
/// to do (archive/rename/reset).
#[tokio::test]
async fn watcher_does_not_create_revisions_after_implementation() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();

    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap().to_string();

    // Make a real commit and register implementation.
    let _head = make_commit(&app.repo, "feature.txt", "implementation\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
    )
    .await
    .unwrap();

    // Edit the plan file *after* implementation registration.
    std::fs::write(&plan_path, "# v1\n## post-impl edit\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let revisions: Vec<i64> = sqlx::query_scalar(
        "SELECT revision_number FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        revisions,
        vec![1],
        "watcher must not append a new revision after impl"
    );

    let warnings: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM events WHERE plan_id = ? AND kind = 'plan_file_changed_after_implementation'",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(warnings.len(), 1, "expected exactly one warning event");
}

/// Re-registering an existing plan_file after implementation also surfaces
/// the warning rather than silently creating a resume_snapshot.
#[tokio::test]
async fn register_plan_file_does_not_resume_snapshot_after_implementation() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();

    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap().to_string();

    let _head = make_commit(&app.repo, "feature.txt", "x\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
    )
    .await
    .unwrap();

    // Stop & restart the daemon would normally resume_snapshot here. We can't
    // restart in-process, but re-registering the plan file from a fresh master
    // call hits the same code path.
    std::fs::write(&plan_path, "# v1 reused for new task\n").unwrap();

    app.call(
        "register_plan_file",
        &app.repo,
        Some("claude-main"),
        json!({"path": &plan_path, "label": "claude-main"}),
    )
    .await
    .unwrap();

    let revisions: Vec<i64> = sqlx::query_scalar(
        "SELECT revision_number FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(revisions, vec![1], "no resume_snapshot allowed post-impl");

    let warnings: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM events WHERE plan_id = ? AND kind = 'plan_file_changed_after_implementation'",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(warnings.len(), 1, "expected one warning from re-register");
}

async fn post_form(app: &TestApp, path: &str, body: &str) -> reqwest::Response {
    app.client
        .post(format!("{}{}", app.base, path))
        .header("origin", "http://127.0.0.1")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.to_string())
        .send()
        .await
        .expect("post send")
}

/// A `done` plan with no implementation commit must still refuse new plan
/// revisions. Previously the watcher only checked
/// `current_implementation_id.is_some()`, missing the
/// `plan_approved → done` (no impl) path.
#[tokio::test]
async fn watcher_does_not_create_revisions_when_done_without_impl() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();

    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap().to_string();

    // Approve + mark done without ever registering an implementation commit.
    let resp = post_form(&app, &format!("/plans/{plan_id}/approve"), "").await;
    assert!(resp.status().is_redirection());
    let resp = post_form(&app, &format!("/plans/{plan_id}/mark-done"), "").await;
    assert!(resp.status().is_redirection());

    std::fs::write(&plan_path, "# v1\n## edit while done\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let revisions: Vec<i64> = sqlx::query_scalar(
        "SELECT revision_number FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        revisions,
        vec![1],
        "no new revision allowed once plan is done"
    );

    let warnings: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM events WHERE plan_id = ? AND kind = 'plan_file_changed_after_implementation'",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(warnings.len(), 1);
}

/// Re-registering a plan file whose previous lifecycle is `archived` must be
/// rejected. Otherwise the agent gets back a `plan_id` that the UI hides
/// (since `/` only lists non-archived plans), making it look like
/// registration succeeded into an invisible session.
#[tokio::test]
async fn cannot_re_register_archived_plan() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();

    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap().to_string();
    post_form(&app, &format!("/plans/{plan_id}/archive"), "").await;

    let (status, body) = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("archived"), "body: {body}");
}

/// Same as above but for a `done` plan.
#[tokio::test]
async fn cannot_re_register_done_plan() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();

    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap().to_string();
    post_form(&app, &format!("/plans/{plan_id}/approve"), "").await;
    post_form(&app, &format!("/plans/{plan_id}/mark-done"), "").await;

    let (status, body) = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("done"), "body: {body}");
}

/// Pre-implementation plan-file edits still create revisions (sanity that we
/// only gate post-impl).
#[tokio::test]
async fn watcher_creates_revisions_before_implementation() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();

    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap().to_string();

    std::fs::write(&plan_path, "# v1\n## new\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let revisions: Vec<i64> = sqlx::query_scalar(
        "SELECT revision_number FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(revisions, vec![1, 2]);
}
