mod common;

use serde_json::json;

use common::TestApp;

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

/// Drive a full plan loop: register → reviewer joins/posts → curator stages+delivers
/// → master polls → master acks → next poll returns none.
#[tokio::test]
async fn full_plan_loop_round_trip() {
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
    let revision_id = reg["revision_id"].as_i64().unwrap();

    // Master polls: nothing delivered yet.
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

    // Reviewer joins and posts feedback.
    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "claude-architect"}),
    )
    .await
    .unwrap();
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("claude-architect"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "plan_revision",
                "target_id": revision_id.to_string(),
                "text": "needs more detail on edge cases",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();

    // Curator stages + delivers via HTTP.
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/stage"),
        "",
    )
    .await;
    assert!(resp.status().is_redirection());
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/deliver"),
        "target_kind=plan_revision",
    )
    .await;
    assert!(resp.status().is_redirection());

    // Master polls and gets the batch.
    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    assert_eq!(poll["directive"], "feedback");
    let batch_id = poll["batch_id"].as_i64().unwrap();
    let items = poll["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["target_kind"], "plan_revision");
    assert_eq!(items[0]["text"], "needs more detail on edge cases");
    assert_eq!(items[0]["actor"], "reviewer:claude-architect");

    // Replay safety: another poll without ack returns the SAME batch.
    let poll2 = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    assert_eq!(poll2["batch_id"], poll["batch_id"]);

    // Ack and confirm next poll is empty.
    app.call(
        "ack_directive",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "batch_id": batch_id}),
    )
    .await
    .unwrap();
    let poll3 = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    assert_eq!(poll3["directive"], "none");

    // The batch row records the ack.
    let row: (Option<i64>, Option<String>) =
        sqlx::query_as("SELECT acked_at, acked_by FROM directive_batches WHERE id = ?")
            .bind(batch_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(row.0.is_some());
    assert_eq!(row.1.as_deref(), Some("master:claude-main"));
}

#[tokio::test]
async fn reviewer_cannot_poll() {
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
    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "claude-architect"}),
    )
    .await
    .unwrap();

    let (status, body) = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-architect"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("only the master"), "body: {body}");
}

#[tokio::test]
async fn poll_without_label_is_forbidden() {
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

    let (status, body) = app
        .call(
            "poll_directive",
            &app.repo,
            None,
            json!({"plan_id": &plan_id}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("register_plan_file"), "body: {body}");
}

#[tokio::test]
async fn approve_plan_transitions_state() {
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

    let resp = post_form(&app, &format!("/plans/{plan_id}/approve"), "").await;
    assert!(resp.status().is_redirection());

    let state_now: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
        .bind(&plan_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(state_now, "plan_approved");

    // Approving again from non-planning state is an error.
    let resp = post_form(&app, &format!("/plans/{plan_id}/approve"), "").await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn ack_idempotent() {
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
    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "rev"}),
    )
    .await
    .unwrap();
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "plan_revision",
                "target_id": revision_id.to_string(),
                "text": "x",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();
    post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/stage"),
        "",
    )
    .await;
    post_form(
        &app,
        &format!("/plans/{plan_id}/deliver"),
        "target_kind=plan_revision",
    )
    .await;
    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    let batch_id = poll["batch_id"].as_i64().unwrap();

    let first = app
        .call(
            "ack_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "batch_id": batch_id}),
        )
        .await
        .unwrap();
    assert_eq!(first["ok"], true);

    let second = app
        .call(
            "ack_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "batch_id": batch_id}),
        )
        .await
        .unwrap();
    assert_eq!(second["already_acked"], true);
}

/// Defends against a label that's reused across plans acking another plan's
/// batch. The shim's bound-plan check would normally catch this client-side,
/// but the daemon must also independently verify that the supplied `plan_id`
/// matches the batch's plan.
#[tokio::test]
async fn ack_directive_rejects_plan_id_mismatch() {
    let app = TestApp::spawn().await;

    let plan_a_path = app.repo.join("plan-a.md");
    std::fs::write(&plan_a_path, "A").unwrap();
    let reg_a = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_a_path, "label": "master"}),
        )
        .await
        .unwrap();
    let plan_a = reg_a["plan_id"].as_str().unwrap().to_string();
    let revision_a = reg_a["revision_id"].as_i64().unwrap();

    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_a, "label": "rev"}),
    )
    .await
    .unwrap();
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "plan_id": &plan_a,
                "target_kind": "plan_revision",
                "target_id": revision_a.to_string(),
                "text": "x",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();
    post_form(
        &app,
        &format!("/plans/{plan_a}/feedback/{event_id}/stage"),
        "",
    )
    .await;
    post_form(
        &app,
        &format!("/plans/{plan_a}/deliver"),
        "target_kind=plan_revision",
    )
    .await;
    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("master"),
            json!({"plan_id": &plan_a}),
        )
        .await
        .unwrap();
    let batch_id = poll["batch_id"].as_i64().unwrap();

    let other_repo = app.tmp.path().join("other-repo");
    std::fs::create_dir_all(&other_repo).unwrap();
    for args in [
        ["init", "-q"].as_slice(),
        ["config", "user.email", "x@y"].as_slice(),
        ["config", "user.name", "x"].as_slice(),
    ] {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&other_repo)
            .args(args)
            .status()
            .unwrap();
    }
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
    let plan_b_path = other_repo.join("plan-b.md");
    std::fs::write(&plan_b_path, "B").unwrap();
    let reg_b = app
        .call(
            "register_plan_file",
            &other_repo,
            None,
            json!({"path": &plan_b_path, "label": "master"}),
        )
        .await
        .unwrap();
    let plan_b = reg_b["plan_id"].as_str().unwrap().to_string();

    let (status, body) = app
        .call(
            "ack_directive",
            &other_repo,
            Some("master"),
            json!({"plan_id": &plan_b, "batch_id": batch_id}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(
        body.contains("belongs to plan"),
        "body should explain mismatch: {body}"
    );
}
