mod common;

use std::path::Path;
use std::process::Command;

use serde_json::json;

use common::TestApp;

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

fn make_commit(repo: &Path, name: &str, contents: &str) -> String {
    std::fs::write(repo.join(name), contents).unwrap();
    run_git(repo, &["add", name]);
    run_git(repo, &["commit", "-q", "-m", &format!("add {name}")]);
    run_git(repo, &["rev-parse", "HEAD"])
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

async fn register_a_plan(app: &TestApp) -> String {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# proposal\n").unwrap();
    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    reg["plan_id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn register_impl_at_head_transitions_state() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let head = make_commit(&app.repo, "feature.txt", "implementation\n");

    let result = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": &head}),
        )
        .await
        .unwrap();
    assert_eq!(result["commit_sha"], head);
    assert_eq!(result["is_head"], true);

    let row: (String, Option<i64>) =
        sqlx::query_as("SELECT state, current_implementation_id FROM plans WHERE id = ?")
            .bind(&plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(row.0, "implementation_review");
    assert!(row.1.is_some());
}

#[tokio::test]
async fn register_impl_rejects_non_head_without_force() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let first = make_commit(&app.repo, "feature.txt", "first\n");
    let _second = make_commit(&app.repo, "feature.txt", "second\n");

    let (status, body) = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": &first}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(body.contains("not HEAD"), "body: {body}");
}

#[tokio::test]
async fn register_impl_accepts_non_head_with_force() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let first = make_commit(&app.repo, "feature.txt", "first\n");
    let _second = make_commit(&app.repo, "feature.txt", "second\n");

    let result = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": &first, "force": true}),
        )
        .await
        .unwrap();
    assert_eq!(result["commit_sha"], first);
    assert_eq!(result["is_head"], false);
}

#[tokio::test]
async fn register_impl_idempotent() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let head = make_commit(&app.repo, "feature.txt", "x\n");

    let first = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": &head}),
        )
        .await
        .unwrap();
    let second = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": &head}),
        )
        .await
        .unwrap();
    assert_eq!(
        first["implementation_revision_id"],
        second["implementation_revision_id"]
    );
    assert_eq!(second["already_registered"], true);
}

#[tokio::test]
async fn reviewer_cannot_register_impl() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let head = make_commit(&app.repo, "feature.txt", "x\n");
    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "rev"}),
    )
    .await
    .unwrap();

    let (status, _body) = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("rev"),
            json!({"plan_id": &plan_id, "commit_sha": &head}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn dirty_worktree_emits_warning_event() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let head = make_commit(&app.repo, "feature.txt", "x\n");
    std::fs::write(app.repo.join("uncommitted.txt"), "dirt").unwrap();

    let result = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": &head}),
        )
        .await
        .unwrap();
    assert_eq!(result["dirty_worktree"], true);

    let warns: Vec<String> = sqlx::query_scalar(
        "SELECT kind FROM events WHERE plan_id = ? AND kind = 'dirty_worktree_warning'",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(warns.len(), 1);
}

#[tokio::test]
async fn reviewer_get_review_context_implementation_returns_diff() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let _head = make_commit(&app.repo, "feature.txt", "implementation body\n");
    let registered = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
        )
        .await
        .unwrap();
    let commit_sha = registered["commit_sha"].as_str().unwrap().to_string();

    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "rev"}),
    )
    .await
    .unwrap();

    let ctx = app
        .call(
            "get_review_context",
            &app.repo,
            Some("rev"),
            json!({"plan_id": &plan_id, "target": "implementation"}),
        )
        .await
        .unwrap();
    let rev = &ctx["latest_implementation_revision"];
    assert_eq!(rev["commit_sha"], commit_sha);
    let diff = rev["diff_text"].as_str().expect("diff_text");
    assert!(diff.contains("feature.txt"), "diff: {diff}");
    assert!(diff.contains("implementation body"));
}

#[tokio::test]
async fn impl_feedback_loop_with_outdated_badge() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let sha1 = make_commit(&app.repo, "feature.txt", "v1\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "commit_sha": &sha1}),
    )
    .await
    .unwrap();

    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "rev"}),
    )
    .await
    .unwrap();
    // Post pending feedback against sha1 *but don't deliver yet*.
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "implementation_commit",
                "target_id": &sha1,
                "text": "rename the function",
            }),
        )
        .await
        .unwrap();
    assert_eq!(post["status"], "pending");

    // Master amends before curator delivers → new SHA becomes current.
    let sha2 = make_commit(&app.repo, "feature.txt", "v2 — renamed\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "commit_sha": &sha2}),
    )
    .await
    .unwrap();

    // The plan detail page should show the still-pending feedback as outdated
    // because target_id (sha1) != current_implementation's commit_sha (sha2).
    let body = app
        .get(&format!("/plans/{plan_id}"))
        .await
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("outdated"),
        "page should mark old feedback outdated"
    );

    // The full master poll loop still works on whatever the curator stages next.
    // (Just verify no panic and a directive=none on a fresh poll.)
    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    assert_eq!(poll["directive"], "none");
}

#[tokio::test]
async fn commit_diff_page_renders() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let head = make_commit(&app.repo, "f.txt", "abc\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "commit_sha": &head}),
    )
    .await
    .unwrap();

    let resp = app.get(&format!("/plans/{plan_id}/commits/{head}")).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Diff stat"), "body: ...");
    assert!(body.contains("f.txt"));
}

#[tokio::test]
async fn cannot_register_commit_against_archived_plan() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let _head = make_commit(&app.repo, "f.txt", "x\n");
    post_form(&app, &format!("/plans/{plan_id}/archive"), "").await;

    let (status, body) = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("terminal state"), "body: {body}");
}

#[tokio::test]
async fn cannot_register_commit_against_done_plan() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let _head = make_commit(&app.repo, "f.txt", "x\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
    )
    .await
    .unwrap();
    post_form(&app, &format!("/plans/{plan_id}/mark-done"), "").await;
    // Make a new commit to register against.
    let _next = make_commit(&app.repo, "f.txt", "y\n");
    let (status, body) = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("terminal state"), "body: {body}");
}

#[tokio::test]
async fn get_review_context_omitted_target_follows_state() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let _head = make_commit(&app.repo, "f.txt", "implementation\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
    )
    .await
    .unwrap();
    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "rev"}),
    )
    .await
    .unwrap();

    // State is implementation_review; omitted target should resolve to implementation.
    let ctx = app
        .call(
            "get_review_context",
            &app.repo,
            Some("rev"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    assert!(
        !ctx["latest_implementation_revision"].is_null(),
        "expected impl revision payload when state is implementation_review, got {ctx}"
    );
    // Plan-stage section should be absent when target resolved to implementation.
    assert!(
        ctx.get("latest_plan_revision").is_none(),
        "got plan revision when state is implementation_review: {ctx}"
    );
}

#[tokio::test]
async fn ui_register_head_fallback_works() {
    let app = TestApp::spawn().await;
    let plan_id = register_a_plan(&app).await;
    let head = make_commit(&app.repo, "feature.txt", "x\n");

    let resp = post_form(&app, &format!("/plans/{plan_id}/register-head"), "").await;
    assert!(resp.status().is_redirection());

    let row: (String,) = sqlx::query_as(
        "SELECT registered_by FROM implementation_revisions WHERE plan_id = ? AND commit_sha = ?",
    )
    .bind(&plan_id)
    .bind(&head)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "human:ui_fallback");
}
