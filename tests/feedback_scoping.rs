//! Regression tests for "feedback must be scoped to the active plan".
//! After the structural-feedback rewrite, scoping is enforced two ways:
//!
//! 1. `put_feedback`'s upsert key is `(plan_id, target_kind, target_id,
//!    author_label)`, and `plan_id` is the *current* `active_plan_id`
//!    resolved under the per-session lock — so an archived plan's slot
//!    can no longer be addressed.
//! 2. `get_current_feedback` joins through `sessions.active_plan_id`,
//!    so archived feedback rows are invisible to the master read.

mod common;

use serde_json::json;

use common::{TestApp, make_commit};

/// Feedback posted under plan A does not appear under plan B after
/// archive + register-new.
#[tokio::test]
async fn feedback_does_not_leak_across_archive_to_new_plan() {
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
    let plan_a = r["plan_id"].as_i64().unwrap();
    let rev_a = r["revision_id"].as_i64().unwrap();

    let posted = app
        .call(
            "put_feedback",
            &app.repo,
            None,
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_a.to_string(),
                "body": "stage me",
                "author_label": "rev",
            }),
        )
        .await
        .unwrap();
    let feedback_id = posted["feedback_id"].as_i64().unwrap();

    // Archive plan A. The feedback row stays in the DB.
    let resp = app.post_form("/sessions/s/archive", "").await;
    assert!(resp.status().is_redirection());
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(exists, 1, "archive must not delete or mutate feedback rows");

    // Start a new lifecycle under the same session.
    std::fs::write(&plan_path, "# v2 — new task\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();

    // Master read returns no feedback for plan B.
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
    let plan_b = view["plan_id"].as_i64().unwrap();
    assert_ne!(plan_a, plan_b);
    assert!(view["feedback"].as_array().unwrap().is_empty());

    // Session detail's "Plan feedback" section is empty for plan B.
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(
        body.contains("No plan feedback yet"),
        "expected empty plan-feedback section for the new active plan"
    );
}

/// `put_feedback` rejects a target that doesn't belong to the active
/// plan. The natural-key upsert lives under the per-session lock so an
/// archive raced into mid-call cannot redirect the write.
#[tokio::test]
async fn put_feedback_rejects_target_from_archived_plan() {
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

    // Archive A and start a new lifecycle.
    app.post_form("/sessions/s/archive", "").await;
    std::fs::write(&plan_path, "# v2\n").unwrap();
    let r2 = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let new_rev = r2["revision_id"].as_i64().unwrap();
    assert_ne!(new_rev, rev_a);

    // Reviewer aimed at the OLD plan_revision: must be rejected.
    let (status, body) = app
        .call(
            "put_feedback",
            &app.repo,
            None,
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": rev_a.to_string(),
                "body": "stale",
                "author_label": "rev",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);
    assert!(body.contains("does not belong"), "body: {body}");
    // No feedback row inserted.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

/// Same-body re-registration is idempotent: returns real `plan_id` and
/// `revision_id` (not -1/-1 sentinels).
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
    let _ = make_commit;
}
