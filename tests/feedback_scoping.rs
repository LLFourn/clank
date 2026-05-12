//! Regression tests for "feedback must be scoped to the active plan".
//! Scoping is enforced two ways:
//!
//! 1. `SessionService::put_feedback`'s upsert key is `(plan_id,
//!    target_kind, target_id, author_label)`, with `plan_id` resolved
//!    from `active_plan_id` under the per-session lock — an archived
//!    plan's slot is no longer addressable.
//! 2. `current_feedback` joins through `sessions.active_plan_id`, so
//!    archived feedback rows are invisible.

mod common;

use serde_json::json;

use common::TestApp;
use trinity::daemon::{CurrentFeedbackView, ServiceError};
use trinity::domain::FeedbackTargetRef;
use trinity::lifecycle::SessionId;

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
        .put_feedback_via_service(
            "s",
            "rev",
            FeedbackTargetRef::PlanRevision(rev_a),
            "stage me",
        )
        .await
        .unwrap();
    let feedback_id = posted.record.id;

    app.archive_via_service("s").await;
    let exists: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(exists, 1, "archive must not delete or mutate feedback rows");

    std::fs::write(&plan_path, "# v2 — new task\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();

    let view = app
        .state
        .lifecycle
        .current_feedback(&SessionId::from("s"), None)
        .await
        .unwrap();
    let CurrentFeedbackView::Active {
        plan_id: plan_b,
        feedback,
        ..
    } = view
    else {
        panic!("expected active plan view");
    };
    assert_ne!(plan_a, plan_b);
    assert!(feedback.is_empty());
    // The structural `feedback.is_empty()` above is the real assertion.
    // The earlier UI text check ("No plan feedback yet") was coupled to the
    // dense per-section session_detail; the flat-timeline rewrite doesn't
    // render a "Plan feedback" section so the text isn't a stable surface.
}

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

    app.archive_via_service("s").await;
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

    let err = app
        .put_feedback_via_service("s", "rev", FeedbackTargetRef::PlanRevision(rev_a), "stale")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ServiceError::FeedbackTargetNotInActivePlan { .. }),
        "got {err:?}"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

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
