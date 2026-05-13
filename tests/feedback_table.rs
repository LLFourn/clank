//! Structural tests on the `feedback` table + `SessionService::put_feedback`
//! semantics. Drives the service path directly via the in-process harness;
//! the `put_feedback` MCP tool was removed (file-ingest dispatcher is the
//! external write path now).

mod common;

use serde_json::json;

use common::TestApp;
use trinity::daemon::CurrentFeedbackView;
use trinity::domain::{FeedbackTargetRef, TargetKind};
use trinity::lifecycle::SessionId;

async fn setup_with_plan(app: &TestApp) -> (i64, i64) {
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

#[tokio::test]
async fn put_feedback_inserts_then_updates() {
    let app = TestApp::spawn().await;
    let (_plan_id, rev_id) = setup_with_plan(&app).await;

    let first = app
        .put_feedback_via_service("s", "rev", FeedbackTargetRef::PlanRevision(rev_id), "v1")
        .await
        .unwrap();
    assert!(first.was_insert);
    assert!(!first.was_no_op);
    let feedback_id = first.record.id;
    let updated_at_first = first.record.updated_at;

    let second = app
        .put_feedback_via_service("s", "rev", FeedbackTargetRef::PlanRevision(rev_id), "v2")
        .await
        .unwrap();
    assert!(!second.was_insert);
    assert!(!second.was_no_op);
    assert_eq!(second.record.id, feedback_id);
    assert!(second.record.updated_at >= updated_at_first);

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
async fn verdict_feedback_emits_review_gate_changed_only_on_state_transition() {
    let app = TestApp::spawn().await;
    let (_plan_id, rev_id) = setup_with_plan(&app).await;

    app.put_feedback_via_service(
        "s",
        "rev",
        FeedbackTargetRef::PlanRevision(rev_id),
        "APPROVE\nship it",
    )
    .await
    .unwrap();
    app.put_feedback_via_service(
        "s",
        "rev",
        FeedbackTargetRef::PlanRevision(rev_id),
        "APPROVE\nstill ship it",
    )
    .await
    .unwrap();

    let events: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM events WHERE session_id = 's' ORDER BY id ASC")
            .fetch_all(&app.state.pool)
            .await
            .unwrap();
    let gate_events = events
        .iter()
        .filter(|kind| kind.as_str() == "review_gate_changed")
        .count();
    assert_eq!(
        gate_events, 1,
        "gate events should track state crossings, not every feedback body update: {events:?}"
    );

    let payload: String = sqlx::query_scalar(
        "SELECT payload FROM events WHERE session_id = 's' AND kind = 'review_gate_changed'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["phase"], "plan");
    assert_eq!(payload["from"], "needs_review");
    assert_eq!(payload["to"], "ready");
    assert_eq!(payload["approvals"], json!(["rev"]));
}

#[tokio::test]
async fn put_feedback_identical_body_is_noop() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_plan(&app).await;

    let first = app
        .put_feedback_via_service(
            "s",
            "rev",
            FeedbackTargetRef::PlanRevision(rev_id),
            "same body",
        )
        .await
        .unwrap();
    let feedback_id = first.record.id;
    let updated_at_first: i64 = sqlx::query_scalar("SELECT updated_at FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();

    let digest_first = current_feedback_digest(&app, "s", None).await;

    let again = app
        .put_feedback_via_service(
            "s",
            "rev",
            FeedbackTargetRef::PlanRevision(rev_id),
            "same body",
        )
        .await
        .unwrap();
    assert!(!again.was_insert);
    assert!(again.was_no_op);
    assert_eq!(again.record.id, feedback_id);

    let updated_at_now: i64 = sqlx::query_scalar("SELECT updated_at FROM feedback WHERE id = ?")
        .bind(feedback_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(updated_at_now, updated_at_first);

    let feedback_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' AND kind LIKE 'feedback_%'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(feedback_events, 1);

    let digest_again = current_feedback_digest(&app, "s", None).await;
    assert_eq!(digest_again, digest_first);
}

#[tokio::test]
async fn unique_key_enforced_per_author() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_plan(&app).await;

    app.put_feedback_via_service("s", "rev-a", FeedbackTargetRef::PlanRevision(rev_id), "a1")
        .await
        .unwrap();
    app.put_feedback_via_service("s", "rev-a", FeedbackTargetRef::PlanRevision(rev_id), "a2")
        .await
        .unwrap();
    let count_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count_a, 1);

    app.put_feedback_via_service("s", "rev-b", FeedbackTargetRef::PlanRevision(rev_id), "b1")
        .await
        .unwrap();
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn current_feedback_reads_from_feedback_table() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_plan(&app).await;
    let posted = app
        .put_feedback_via_service(
            "s",
            "rev",
            FeedbackTargetRef::PlanRevision(rev_id),
            "real body",
        )
        .await
        .unwrap();
    let feedback_id = posted.record.id;

    sqlx::query("UPDATE events SET payload = '{}' WHERE kind = 'feedback_added'")
        .execute(&app.state.pool)
        .await
        .unwrap();

    let view = app
        .state
        .lifecycle
        .current_feedback(&SessionId::from("s"), None)
        .await
        .unwrap();
    let CurrentFeedbackView::Active { feedback, .. } = view else {
        panic!("expected Active view");
    };
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].id, feedback_id);
    assert_eq!(feedback[0].body, "real body");
}

#[tokio::test]
async fn put_feedback_rejects_empty_body() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_plan(&app).await;
    for body in ["", "   "] {
        let err = app
            .put_feedback_via_service(
                "s",
                "rev",
                FeedbackTargetRef::PlanRevision(rev_id),
                body.to_string(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            trinity::daemon::ServiceError::EmptyFeedbackBody
        ));
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
    let (_, rev_id) = setup_with_plan(&app).await;

    let empty = current_feedback_digest(&app, "s", None).await;

    app.put_feedback_via_service(
        "s",
        "rev-a",
        FeedbackTargetRef::PlanRevision(rev_id),
        "first",
    )
    .await
    .unwrap();
    let after_insert = current_feedback_digest(&app, "s", None).await;
    assert_ne!(empty, after_insert, "insert must change digest");

    app.put_feedback_via_service(
        "s",
        "rev-a",
        FeedbackTargetRef::PlanRevision(rev_id),
        "first",
    )
    .await
    .unwrap();
    let after_noop = current_feedback_digest(&app, "s", None).await;
    assert_eq!(after_insert, after_noop);

    app.put_feedback_via_service(
        "s",
        "rev-a",
        FeedbackTargetRef::PlanRevision(rev_id),
        "different",
    )
    .await
    .unwrap();
    let after_update = current_feedback_digest(&app, "s", None).await;
    assert_ne!(after_noop, after_update);

    app.put_feedback_via_service(
        "s",
        "rev-b",
        FeedbackTargetRef::PlanRevision(rev_id),
        "second-reviewer",
    )
    .await
    .unwrap();
    let after_second_author = current_feedback_digest(&app, "s", None).await;
    assert_ne!(after_update, after_second_author);

    let filtered = current_feedback_digest(&app, "s", Some(TargetKind::ImplementationCommit)).await;
    assert_ne!(filtered, after_second_author);
}

#[tokio::test]
async fn feedback_session_id_matches_plans_session_id() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = setup_with_plan(&app).await;
    app.put_feedback_via_service("s", "rev", FeedbackTargetRef::PlanRevision(rev_id), "a")
        .await
        .unwrap();

    let plan_path = app.repo.join("plan.md");
    app.archive_via_service("s").await;
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
    app.put_feedback_via_service("s", "rev", FeedbackTargetRef::PlanRevision(new_rev), "b")
        .await
        .unwrap();

    let mismatched: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback f \
         LEFT JOIN sessions s ON s.id = f.session_id \
         WHERE s.id IS NULL",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(mismatched, 0);
}

async fn current_feedback_digest(
    app: &TestApp,
    session_id: &str,
    filter: Option<TargetKind>,
) -> String {
    let view = app
        .state
        .lifecycle
        .current_feedback(&SessionId::from(session_id), filter)
        .await
        .unwrap();
    match view {
        CurrentFeedbackView::NoActivePlan { .. } => "no_active_plan".into(),
        CurrentFeedbackView::Active {
            feedback_digest, ..
        } => feedback_digest,
    }
}
