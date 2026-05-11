mod common;

use serde_json::json;

use common::{TestApp, make_commit};

#[tokio::test]
async fn register_plan_file_creates_session_and_first_revision() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# initial\n").unwrap();

    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "demo", "path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    assert_eq!(r["session_id"], "demo");
    assert!(r["plan_id"].as_i64().unwrap() >= 1);

    let s: (String, String, Option<i64>) =
        sqlx::query_as("SELECT id, plan_file_path, active_plan_id FROM sessions WHERE id = ?")
            .bind("demo")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(s.0, "demo");
    assert!(s.1.ends_with("plan.md"));
    assert!(s.2.is_some());

    let revisions: Vec<(i64, String)> = sqlx::query_as(
        "SELECT revision_number, content_hash FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(s.2.unwrap())
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].0, 1);
}

#[tokio::test]
async fn register_plan_file_rejects_invalid_slug() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
    let (status, body) = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "has spaces", "path": &plan_path, "label": "m"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid character"), "body: {body}");
}

#[tokio::test]
async fn register_plan_file_same_body_is_no_op() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# same\n").unwrap();
    let first = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = first["plan_id"].as_i64().unwrap();

    let second = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    // No new revision; the same lifecycle is still active.
    assert_eq!(second["noop"], true);

    let revs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plan_revisions WHERE plan_id = ?")
        .bind(plan_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(revs, 1, "no extra revision on same-body re-register");
}

#[tokio::test]
async fn planning_edit_via_register_records_revision_same_plan() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    let first = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = first["plan_id"].as_i64().unwrap();

    std::fs::write(&plan_path, "# v2\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        Some("claude-main"),
        json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
    )
    .await
    .unwrap();

    let revs: Vec<(i64, String)> = sqlx::query_as(
        "SELECT revision_number, content_hash FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(revs.len(), 2, "planning edit adds a revision, same plan_id");
}

#[tokio::test]
async fn implementing_then_plan_edit_archives_and_starts_new() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_a = r["plan_id"].as_i64().unwrap();
    let _head = make_commit(&app.repo, "feature.txt", "x\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("claude-main"),
        json!({"session_id": "s", "commit_sha": "HEAD"}),
    )
    .await
    .unwrap();

    // Now edit plan file → archive + start new (re-register triggers).
    std::fs::write(&plan_path, "# new task body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        Some("claude-main"),
        json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
    )
    .await
    .unwrap();

    let old_state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
        .bind(plan_a)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(old_state, "archived");

    let new_id: i64 = sqlx::query_scalar(
        "SELECT id FROM plans WHERE session_id = 's' AND state = 'planning' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert!(new_id > plan_a, "new plan_id should be later than old");
    let session_active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(session_active, Some(new_id));
}

#[tokio::test]
async fn register_impl_same_sha_is_idempotent() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let plan_id = r["plan_id"].as_i64().unwrap();
    let _head = make_commit(&app.repo, "f.txt", "x\n");

    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "commit_sha": "HEAD"}),
    )
    .await
    .unwrap();
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "commit_sha": "HEAD"}),
    )
    .await
    .unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 1, "same SHA must not duplicate impl revision");
}

#[tokio::test]
async fn reviewer_join_and_post_feedback() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let rev_id = r["revision_id"].as_i64().unwrap();

    app.call(
        "join_session",
        &app.repo,
        None,
        json!({"session_id": "s", "label": "rev"}),
    )
    .await
    .unwrap();
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_id.to_string(),
                "text": "consider X",
            }),
        )
        .await
        .unwrap();
    assert_eq!(post["status"], "pending");
}

#[tokio::test]
async fn feedback_on_archived_plan_is_rejected() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let plan_a = r["plan_id"].as_i64().unwrap();
    let rev_id = r["revision_id"].as_i64().unwrap();

    // Reviewer joins and the curator archives the active plan.
    app.call(
        "join_session",
        &app.repo,
        None,
        json!({"session_id": "s", "label": "rev"}),
    )
    .await
    .unwrap();
    let resp = app.post_form("/sessions/s/archive", "").await;
    assert!(resp.status().is_redirection());

    // Feedback against the now-archived plan's revision must be rejected.
    let (status, body) = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_id.to_string(),
                "text": "late",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(
        body.contains("no active plan") || body.contains("non-active plan"),
        "body: {body}"
    );
    let _ = plan_a;
}

#[tokio::test]
async fn curator_archive_route_returns_303() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();
    let resp = app.post_form("/sessions/s/archive", "").await;
    assert!(resp.status().is_redirection());
    let state: String = sqlx::query_scalar(
        "SELECT state FROM plans WHERE session_id = 's' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(state, "archived");
}

#[tokio::test]
async fn poll_and_ack_full_loop() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let rev_id = r["revision_id"].as_i64().unwrap();

    app.call(
        "join_session",
        &app.repo,
        None,
        json!({"session_id": "s", "label": "rev"}),
    )
    .await
    .unwrap();
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_id.to_string(),
                "text": "comment",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();

    app.post_form(&format!("/sessions/s/feedback/{event_id}/stage"), "")
        .await;
    app.post_form("/sessions/s/deliver", "target_kind=plan_revision")
        .await;

    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    let batch_id = poll["batch_id"].as_i64().unwrap();
    // Replay-safe: polling again returns the same batch.
    let again = app
        .call(
            "poll_directive",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    assert_eq!(again["batch_id"], poll["batch_id"]);

    app.call(
        "ack_directive",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "batch_id": batch_id}),
    )
    .await
    .unwrap();
    let post_ack = app
        .call(
            "poll_directive",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    assert_eq!(post_ack["directive"], "none");
}

#[tokio::test]
async fn ack_directive_rejects_session_mismatch() {
    let app = TestApp::spawn().await;
    let plan_a = app.repo.join("a.md");
    std::fs::write(&plan_a, "a").unwrap();
    let r_a = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "alpha", "path": &plan_a, "label": "m"}),
        )
        .await
        .unwrap();
    let rev_id = r_a["revision_id"].as_i64().unwrap();
    app.call(
        "join_session",
        &app.repo,
        None,
        json!({"session_id": "alpha", "label": "rev"}),
    )
    .await
    .unwrap();
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "session_id": "alpha",
                "target_kind": "plan_revision",
                "target_id": rev_id.to_string(),
                "text": "x",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();
    app.post_form(&format!("/sessions/alpha/feedback/{event_id}/stage"), "")
        .await;
    app.post_form("/sessions/alpha/deliver", "target_kind=plan_revision")
        .await;
    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("m"),
            json!({"session_id": "alpha"}),
        )
        .await
        .unwrap();
    let batch_id = poll["batch_id"].as_i64().unwrap();

    // Another session, also master-claimed.
    let plan_b = app.repo.join("b.md");
    std::fs::write(&plan_b, "b").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "beta", "path": &plan_b, "label": "m2"}),
    )
    .await
    .unwrap();

    let (status, body) = app
        .call(
            "ack_directive",
            &app.repo,
            Some("m2"),
            json!({"session_id": "beta", "batch_id": batch_id}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("belongs to session"), "body: {body}");
}

#[tokio::test]
async fn home_page_lists_session() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# proposal\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "demo", "path": &plan_path, "label": "claude-main"}),
    )
    .await
    .unwrap();
    let resp = app.get("/").await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("Sessions"));
    assert!(body.contains("demo"));
    assert!(body.contains("claude-main"));
}
