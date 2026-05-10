mod common;

use serde_json::json;

use common::TestApp;

#[tokio::test]
async fn echo_cwd_returns_cwd_and_no_label_until_bound() {
    let app = TestApp::spawn().await;
    let result = app
        .call("echo_cwd", &app.repo, None, json!({}))
        .await
        .unwrap();
    // echo_cwd echoes the raw cwd the shim sent, no canonicalization.
    assert_eq!(result["cwd"], serde_json::json!(app.repo));
    assert!(
        result["label"].is_null(),
        "label should be null pre-binding, got {result}"
    );
}

#[tokio::test]
async fn register_plan_file_creates_plan_revision_agent_and_events() {
    let app = TestApp::spawn().await;
    let plan_path = app.tmp.path().join("plan.md");
    std::fs::write(&plan_path, "# initial body\n").unwrap();

    let result = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();

    let plan_id = result["plan_id"].as_str().expect("plan_id").to_string();
    assert_eq!(result["revision_id"], 1);

    let plan = sqlx::query_as::<_, (String, String, String)>(
        "SELECT id, state, repo_root FROM plans WHERE id = ?",
    )
    .bind(&plan_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(plan.0, plan_id);
    assert_eq!(plan.1, "planning");

    let revisions: Vec<(i64, String)> = sqlx::query_as(
        "SELECT revision_number, detected_by FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(revisions, vec![(1, "register_plan_file".into())]);

    let agent: (String, String) =
        sqlx::query_as("SELECT role, label FROM agents WHERE plan_id = ?")
            .bind(&plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(agent, ("master".into(), "claude-main".into()));

    let event_kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM events WHERE plan_id = ? ORDER BY id")
            .bind(&plan_id)
            .fetch_all(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(event_kinds, vec!["plan_revision_created", "agent_joined"]);
}

#[tokio::test]
async fn register_plan_file_rejects_missing_file() {
    let app = TestApp::spawn().await;
    let bogus = app.tmp.path().join("does-not-exist.md");
    let (status, body) = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": bogus, "label": "claude-main"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(body.contains("does not exist"), "body: {body}");
}

#[tokio::test]
async fn register_plan_file_rejects_when_caller_not_in_repo() {
    let app = TestApp::spawn().await;
    let outside = app.tmp.path().join("not-a-repo");
    std::fs::create_dir_all(&outside).unwrap();
    let plan_path = outside.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();

    let (status, body) = app
        .call(
            "register_plan_file",
            &outside,
            None,
            json!({"path": plan_path, "label": "claude-main"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(body.contains("not in a git repository"), "body: {body}");
}

#[tokio::test]
async fn duplicate_plan_path_across_repos_rejected() {
    let app = TestApp::spawn().await;
    let shared_plan = app.tmp.path().join("shared-plan.md");
    std::fs::write(&shared_plan, "shared body\n").unwrap();

    // First repo registers the plan file.
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"path": &shared_plan, "label": "first-master"}),
    )
    .await
    .unwrap();

    // Second repo (different repo_root) tries to register the same canonical path.
    let other_repo = app.tmp.path().join("other-repo");
    std::fs::create_dir_all(&other_repo).unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["init", "-q"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["config", "user.email", "x@y"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["config", "user.name", "x"])
        .status()
        .unwrap();
    std::fs::write(other_repo.join("k"), b"").unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["add", "k"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["commit", "-q", "-m", "init"])
        .status()
        .unwrap();

    let (status, body) = app
        .call(
            "register_plan_file",
            &other_repo,
            None,
            json!({"path": &shared_plan, "label": "second-master"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("already actively registered"), "body: {body}");
}

#[tokio::test]
async fn watcher_picks_up_external_edits() {
    let app = TestApp::spawn().await;
    let plan_path = app.tmp.path().join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();

    let result = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = result["plan_id"].as_str().unwrap().to_string();

    // Edit the file; the watcher should debounce ~1.5s and snapshot.
    std::fs::write(&plan_path, "# v1\n## changed\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let revisions: Vec<(i64, String)> = sqlx::query_as(
        "SELECT revision_number, detected_by FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        revisions,
        vec![(1, "register_plan_file".into()), (2, "watcher".into())]
    );
}

#[tokio::test]
async fn list_plans_is_repo_scoped() {
    let app = TestApp::spawn().await;
    // Plan in repo A.
    let plan_a = app.repo.join("plan-a.md");
    std::fs::write(&plan_a, "A").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"path": &plan_a, "label": "master-a"}),
    )
    .await
    .unwrap();

    // Plan in a *separate* repo also under the tempdir.
    let other_repo = app.tmp.path().join("other-repo");
    std::fs::create_dir_all(&other_repo).unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["init", "-q"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["config", "user.email", "x@y"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["config", "user.name", "x"])
        .status()
        .unwrap();
    std::fs::write(other_repo.join("k"), b"").unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["add", "k"])
        .status()
        .unwrap();
    std::process::Command::new("git")
        .arg("-C")
        .arg(&other_repo)
        .args(["commit", "-q", "-m", "init"])
        .status()
        .unwrap();
    let plan_b = other_repo.join("plan-b.md");
    std::fs::write(&plan_b, "B").unwrap();
    app.call(
        "register_plan_file",
        &other_repo,
        None,
        json!({"path": &plan_b, "label": "master-b"}),
    )
    .await
    .unwrap();

    // Listing from repo A should only show master-a.
    let result = app
        .call("list_plans", &app.repo, None, json!({}))
        .await
        .unwrap();
    let plans = result["plans"].as_array().expect("plans array");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0]["master_label"], "master-a");
}

#[tokio::test]
async fn reviewer_join_get_context_and_add_feedback() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# proposal\n\n- step 1\n- step 2\n").unwrap();

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

    // Reviewer joins.
    let join = app
        .call(
            "join_plan",
            &app.repo,
            None,
            json!({"plan_id_or_path": &plan_id, "label": "claude-architect"}),
        )
        .await
        .unwrap();
    assert_eq!(join["plan_id"], plan_id);
    assert_eq!(join["state"], "planning");

    // Reviewer fetches context — needs label header now.
    let ctx = app
        .call(
            "get_review_context",
            &app.repo,
            Some("claude-architect"),
            json!({"plan_id": &plan_id, "target": "plan"}),
        )
        .await
        .unwrap();
    let rev = &ctx["latest_plan_revision"];
    let revision_id = rev["revision_id"].as_i64().expect("revision_id");
    assert!(rev["body"].as_str().unwrap().contains("step 1"));

    // Reviewer posts plan-stage feedback.
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("claude-architect"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "plan_revision",
                "target_id": revision_id.to_string(),
                "text": "step 2 is underspecified",
            }),
        )
        .await
        .unwrap();
    assert_eq!(post["status"], "pending");

    // Pending count via list_plans.
    let listed = app
        .call("list_plans", &app.repo, None, json!({}))
        .await
        .unwrap();
    let plans = listed["plans"].as_array().unwrap();
    assert_eq!(plans[0]["pending_plan_count"], 1);
    assert_eq!(plans[0]["pending_impl_count"], 0);
}

#[tokio::test]
async fn add_feedback_rejects_unbound_caller() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
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
    let revision_id = reg["revision_id"].as_i64().unwrap();

    let (status, body) = app
        .call(
            "add_feedback",
            &app.repo,
            None,
            json!({
                "plan_id": &plan_id,
                "target_kind": "plan_revision",
                "target_id": revision_id.to_string(),
                "text": "hi",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(
        body.contains("register_plan_file or join_plan"),
        "body: {body}"
    );
}

#[tokio::test]
async fn master_cannot_add_feedback_to_own_plan() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
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
    let revision_id = reg["revision_id"].as_i64().unwrap();

    let (status, body) = app
        .call(
            "add_feedback",
            &app.repo,
            Some("claude-main"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "plan_revision",
                "target_id": revision_id.to_string(),
                "text": "self review",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("only reviewers"), "body: {body}");
}

#[tokio::test]
async fn label_collision_rejected() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
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

    // Trying to join as a reviewer with the master's label is rejected.
    let (status, body) = app
        .call(
            "join_plan",
            &app.repo,
            None,
            json!({"plan_id_or_path": &plan_id, "label": "claude-main"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("already a master"), "body: {body}");
}

#[tokio::test]
async fn home_page_renders_plan_row() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# proposal\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"path": &plan_path, "label": "claude-main"}),
    )
    .await
    .unwrap();

    let resp = app.get("/").await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Managed plans"), "body: {body}");
    assert!(body.contains("claude-main"), "should show master label");
    assert!(body.contains("planning"), "should show state badge");
}

#[tokio::test]
async fn plan_detail_renders_markdown_body() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# heading\n\n- item\n").unwrap();
    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap();

    let resp = app.get(&format!("/plans/{plan_id}")).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("<h1>heading</h1>"), "should render markdown");
    assert!(body.contains("<li>item</li>"));
}

#[tokio::test]
async fn missing_plan_returns_404() {
    let app = TestApp::spawn().await;
    let resp = app
        .get("/plans/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn origin_guard_blocks_non_loopback_post() {
    let app = TestApp::spawn().await;
    let resp = app
        .client
        .post(format!("{}/internal/tool_call", app.base))
        .header("origin", "http://evil.example")
        .json(&json!({"cwd": app.repo, "tool": "echo_cwd", "arguments": {}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}
