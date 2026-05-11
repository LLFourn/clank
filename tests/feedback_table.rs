//! Structural tests on the new `feedback` table + `put_feedback` /
//! `get_current_feedback` semantics.

mod common;

use serde_json::json;

use common::TestApp;

async fn setup_with_reviewer(app: &TestApp, _label_reviewer: &str) -> (i64, i64) {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
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
    let rev_id = r["revision_id"].as_i64().unwrap();
    (plan_id, rev_id)
}

async fn put_plan_feedback(
    app: &TestApp,
    label: &str,
    rev_id: i64,
    body: &str,
) -> serde_json::Value {
    app.call(
        "put_feedback",
        &app.repo,
        Some(label),
        json!({
            "session_id": "s",
            "target_kind": "plan_revision",
            "target_id": rev_id.to_string(),
            "body": body,
        }),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn put_feedback_inserts_then_updates() {
    let app = TestApp::spawn().await;
    let (_plan_id, rev_id) = setup_with_reviewer(&app, "rev").await;

    let first = put_plan_feedback(&app, "rev", rev_id, "v1").await;
    assert_eq!(first["was_insert"], true);
    assert_eq!(first["was_no_op"], false);
    let feedback_id = first["feedback_id"].as_i64().unwrap();
    let updated_at_first = first["updated_at"].as_i64().unwrap();

    // Second call with new body: UPDATE the same row.
    let second = put_plan_feedback(&app, "rev", rev_id, "v2").await;
    assert_eq!(second["was_insert"], false);
    assert_eq!(second["was_no_op"], false);
    assert_eq!(second["feedback_id"].as_i64().unwrap(), feedback_id);
    assert!(second["updated_at"].as_i64().unwrap() >= updated_at_first);

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    let body: String = sqlx::query_scalar("SELECT body FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(body, "v2");

    // Audit chain: feedback_added then feedback_updated with prior_body.
    let events: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, payload FROM events WHERE session_id = 's' AND kind LIKE 'feedback_%' ORDER BY id ASC",
    )
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "feedback_added");
    assert_eq!(events[1].0, "feedback_updated");
    let updated_payload: serde_json::Value = serde_json::from_str(&events[1].1).unwrap();
    assert_eq!(updated_payload["prior_body"], "v1");
    assert_eq!(updated_payload["feedback_id"], feedback_id);
}

#[tokio::test]
async fn put_feedback_identical_body_is_noop() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_reviewer(&app, "rev").await;

    let first = put_plan_feedback(&app, "rev", rev_id, "same body").await;
    let feedback_id = first["feedback_id"].as_i64().unwrap();
    let updated_at_first: i64 = sqlx::query_scalar("SELECT updated_at FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();

    let view_first = app
        .call(
            "get_current_feedback",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    let digest_first = view_first["feedback_digest"].as_str().unwrap().to_string();

    // Identical body re-post: no-op.
    let again = put_plan_feedback(&app, "rev", rev_id, "same body").await;
    assert_eq!(again["was_insert"], false);
    assert_eq!(again["was_no_op"], true);
    assert_eq!(again["feedback_id"].as_i64().unwrap(), feedback_id);

    // updated_at must not have moved.
    let updated_at_now: i64 = sqlx::query_scalar("SELECT updated_at FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(updated_at_now, updated_at_first);

    // No new audit event.
    let feedback_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' AND kind LIKE 'feedback_%'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(feedback_events, 1);

    // Digest unchanged.
    let view_again = app
        .call(
            "get_current_feedback",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    assert_eq!(
        view_again["feedback_digest"].as_str().unwrap(),
        digest_first
    );
}

#[tokio::test]
async fn unique_key_enforced_per_author() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_reviewer(&app, "rev-a").await;

    put_plan_feedback(&app, "rev-a", rev_id, "a1").await;
    put_plan_feedback(&app, "rev-a", rev_id, "a2").await;
    // Same author + target -> one row.
    let count_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count_a, 1);

    put_plan_feedback(&app, "rev-b", rev_id, "b1").await;
    // Different author -> second row.
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn current_feedback_reads_from_feedback_table() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_reviewer(&app, "rev").await;
    let posted = put_plan_feedback(&app, "rev", rev_id, "real body").await;
    let feedback_id = posted["feedback_id"].as_i64().unwrap();

    // Corrupt the corresponding events.payload to prove that get_current_feedback
    // sources `body` from `feedback`, not from `events.payload`.
    sqlx::query("UPDATE events SET payload = '{}' WHERE kind = 'feedback_added'")
        .execute(&app.state.pool)
        .await
        .unwrap();

    let view = app
        .call(
            "get_current_feedback",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap();
    let items = view["feedback"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["feedback_id"].as_i64().unwrap(), feedback_id);
    assert_eq!(items[0]["body"], "real body");
}

#[tokio::test]
async fn put_feedback_rejects_empty_body() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_reviewer(&app, "rev").await;
    for body in ["", "   "] {
        let (status, _) = app
            .call(
                "put_feedback",
                &app.repo,
                Some("rev"),
                json!({
                    "session_id": "s",
                    "target_kind": "plan_revision",
                    "target_id": rev_id.to_string(),
                    "body": body,
                }),
            )
            .await
            .expect_err();
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn current_feedback_digest_change_conditions() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_reviewer(&app, "rev-a").await;

    async fn digest(app: &TestApp) -> String {
        app.call(
            "get_current_feedback",
            &app.repo,
            Some("m"),
            json!({"session_id": "s"}),
        )
        .await
        .unwrap()["feedback_digest"]
            .as_str()
            .unwrap()
            .to_string()
    }

    let empty = digest(&app).await;

    put_plan_feedback(&app, "rev-a", rev_id, "first").await;
    let after_insert = digest(&app).await;
    assert_ne!(empty, after_insert, "insert must change digest");

    // No-op upsert keeps digest stable.
    put_plan_feedback(&app, "rev-a", rev_id, "first").await;
    let after_noop = digest(&app).await;
    assert_eq!(
        after_insert, after_noop,
        "identical-body no-op must keep digest stable"
    );

    // New body bumps digest.
    put_plan_feedback(&app, "rev-a", rev_id, "different").await;
    let after_update = digest(&app).await;
    assert_ne!(after_noop, after_update, "body change must bump digest");

    // Another reviewer adds a row → digest changes.
    put_plan_feedback(&app, "rev-b", rev_id, "second-reviewer").await;
    let after_second_author = digest(&app).await;
    assert_ne!(after_update, after_second_author);

    // Filter to a non-matching target_kind reduces row count to 0; digest
    // also folds the filter byte, so it differs from the no-filter call.
    let filtered = app
        .call(
            "get_current_feedback",
            &app.repo,
            Some("m"),
            json!({"session_id": "s", "target_kind": "implementation_commit"}),
        )
        .await
        .unwrap()["feedback_digest"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(filtered, after_second_author);
}

#[tokio::test]
async fn feedback_session_id_matches_plans_session_id() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_reviewer(&app, "rev").await;
    put_plan_feedback(&app, "rev", rev_id, "a").await;

    // Archive + new plan + new feedback to exercise multiple plan_ids.
    let plan_path = app.repo.join("plan.md");
    app.post_form("/sessions/s/archive", "").await;
    std::fs::write(&plan_path, "# v2\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            Some("m"),
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let new_rev = r["revision_id"].as_i64().unwrap();
    put_plan_feedback(&app, "rev", new_rev, "b").await;

    let mismatched: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback f JOIN plans p ON p.id = f.plan_id WHERE f.session_id != p.session_id",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(mismatched, 0);
}

// No master/reviewer role gating anymore: any caller can call any tool. The
// removed test (`master_cannot_put_feedback_and_reviewer_cannot_read`) was
// the role check; cross-session calls are now allowed at the shim layer
// too (see tests/mcp_shim_autofill.rs).
