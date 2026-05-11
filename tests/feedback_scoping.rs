//! Regression tests for the "feedback must be scoped to the active plan"
//! invariant + the evict-master FK fix.

mod common;

use serde_json::json;

use common::{TestApp, make_commit};

/// P1 regression: pending and staged feedback from a previous (now-archived)
/// lifecycle must NOT leak into a new active plan's pending/staged display
/// or deliver pickup.
#[tokio::test]
async fn staged_feedback_does_not_leak_across_archive_to_new_plan() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let revision_a = r["revision_id"].as_i64().unwrap();

    // Reviewer posts feedback under plan A.
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
                "target_id": revision_a.to_string(),
                "text": "stage me",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();

    // Curator stages it. Now archive the active plan via curator route.
    app.post_form(&format!("/sessions/s/feedback/{event_id}/stage"), "")
        .await;
    let resp = app.post_form("/sessions/s/archive", "").await;
    assert!(resp.status().is_redirection());

    // The previously-staged event must now be `withdrawn` (apply layer does this on archive).
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM events WHERE id = ?")
        .bind(event_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("withdrawn"));

    // Start a NEW lifecycle by re-registering with different body.
    std::fs::write(&plan_path, "# v2 — new task\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();

    // Homepage / session detail must show 0 pending plan-feedback for the new active plan.
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(
        body.contains("No pending plan feedback") || !body.contains("stage me"),
        "old-cycle feedback must not appear under the new active plan"
    );

    // Curator deliver must also refuse to pick up the old event.
    let resp = app
        .post_form("/sessions/s/deliver", "target_kind=plan_revision")
        .await;
    assert_eq!(
        resp.status(),
        400,
        "deliver should refuse: nothing staged for the new active plan"
    );
}

/// P1 regression: re-staging a withdrawn (old-cycle) event must be rejected
/// even by event_id.
#[tokio::test]
async fn stage_route_refuses_old_cycle_feedback() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let rev_a = r["revision_id"].as_i64().unwrap();
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
                "target_id": rev_a.to_string(),
                "text": "x",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();

    // Archive + start a new plan.
    app.post_form("/sessions/s/archive", "").await;
    std::fs::write(&plan_path, "# v2\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();

    // Try to stage the OLD event_id. Must refuse.
    let resp = app
        .post_form(&format!("/sessions/s/feedback/{event_id}/stage"), "")
        .await;
    assert_eq!(resp.status(), 400);
}

/// P1 race regression: the reducer-under-lock target validation must reject
/// feedback whose target belongs to a plan that has just been archived.
/// Simulated by archiving directly via the curator route between the
/// reviewer's two interactions.
#[tokio::test]
async fn add_feedback_rejects_target_from_archived_plan() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let rev_a = r["revision_id"].as_i64().unwrap();
    app.call(
        "join_session",
        &app.repo,
        None,
        json!({"session_id": "s", "label": "rev"}),
    )
    .await
    .unwrap();

    // Archive while the reviewer "still thinks" rev_a is valid.
    app.post_form("/sessions/s/archive", "").await;
    // Start a fresh lifecycle.
    std::fs::write(&plan_path, "# v2\n").unwrap();
    let r2 = app
        .call(
            "register_plan_file",
            &app.repo,
            Some("m"),
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let new_revision = r2["revision_id"].as_i64().unwrap();
    assert_ne!(new_revision, rev_a);

    // Reviewer posts feedback aimed at the OLD plan's revision. Must be rejected.
    let (status, body) = app
        .call(
            "add_feedback",
            &app.repo,
            Some("rev"),
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_a.to_string(),
                "text": "stale",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(
        body.contains("non-active") || body.contains("does not belong"),
        "body: {body}"
    );
}

/// Same-body re-registration is idempotent: it returns the current active
/// plan_id and the latest revision_id (not -1/-1 sentinels).
#[tokio::test]
async fn register_no_op_returns_real_ids() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    let first = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let plan_id = first["plan_id"].as_i64().unwrap();
    let revision_id = first["revision_id"].as_i64().unwrap();
    assert!(plan_id > 0 && revision_id > 0);

    let second = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    assert_eq!(second["noop"], true);
    assert_eq!(second["plan_id"].as_i64().unwrap(), plan_id);
    assert_eq!(second["revision_id"].as_i64().unwrap(), revision_id);
}

/// P1 regression: evict_master must not violate the
/// `sessions.master_agent_id → agents.id` FK by trying to point at agent 0.
#[tokio::test]
async fn evict_master_works_with_fk_enabled() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();
    let resp = app.post_form("/sessions/s/evict-master", "").await;
    assert!(
        resp.status().is_redirection(),
        "expected redirect, got {} body={}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // master_agent_id should be NULL and the agents row gone.
    let master: Option<i64> =
        sqlx::query_scalar("SELECT master_agent_id FROM sessions WHERE id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(master.is_none());
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agents WHERE session_id = 's' AND role = 'master'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
}

/// And after eviction, a fresh master claim with a new label should work
/// (the unique constraint must not block).
#[tokio::test]
async fn fresh_master_after_eviction() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "first"}),
    )
    .await
    .unwrap();
    app.post_form("/sessions/s/evict-master", "").await;
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "second"}),
    )
    .await
    .unwrap();
    let master_label: Option<String> =
        sqlx::query_scalar("SELECT label FROM agents WHERE session_id = 's' AND role = 'master'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(master_label.as_deref(), Some("second"));
    // unused suppression
    let _ = make_commit;
}
