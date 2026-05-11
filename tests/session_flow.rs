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

    let post = app
        .call(
            "put_feedback",
            &app.repo,
            None,
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_id.to_string(),
                "body": "consider X",
                "author_label": "rev",
            }),
        )
        .await
        .unwrap();
    assert_eq!(post["was_insert"], true);
    assert_eq!(post["was_no_op"], false);
    let feedback_id = post["feedback_id"].as_i64().unwrap();

    let body: String = sqlx::query_scalar("SELECT body FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(body, "consider X");
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

    let resp = app.post_form("/sessions/s/archive", "").await;
    assert!(resp.status().is_redirection());

    // No active plan now; put_feedback must refuse.
    let (status, body) = app
        .call(
            "put_feedback",
            &app.repo,
            None,
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_id.to_string(),
                "body": "late",
                "author_label": "rev",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("no active plan"), "body: {body}");
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
async fn get_current_feedback_returns_active_plan_rows() {
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
        "put_feedback",
        &app.repo,
        None,
        json!({
            "session_id": "s",
            "target_kind": "plan_revision",
            "target_id": rev_id.to_string(),
            "body": "comment",
            "author_label": "rev",
        }),
    )
    .await
    .unwrap();

    let view = app
        .call(
            "get_current_feedback",
            &app.repo,
            None,
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    assert_eq!(view["state"], "active");
    let items = view["feedback"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["body"], "comment");
    assert_eq!(items[0]["author_label"], "rev");
    let digest_first = view["feedback_digest"].as_str().unwrap().to_string();

    // Re-read with no changes: digest is byte-stable.
    let view_again = app
        .call(
            "get_current_feedback",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    assert_eq!(view_again["feedback_digest"], digest_first);
}

#[tokio::test]
async fn get_current_feedback_no_active_plan() {
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
    app.post_form("/sessions/s/archive", "").await;

    let view = app
        .call(
            "get_current_feedback",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    assert_eq!(view["state"], "no_active_plan");
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
