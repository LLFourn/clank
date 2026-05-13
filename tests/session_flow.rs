mod common;

use serde_json::json;

use common::{TestApp, make_commit};
use trinity::daemon::{CurrentFeedbackView, ServiceError};
use trinity::domain::FeedbackTargetRef;
use trinity::lifecycle::SessionId;

async fn active_plan_id(app: &TestApp, session_id: &str) -> Option<i64> {
    sqlx::query_scalar(
        "SELECT CASE WHEN state IN ('planning', 'implementing') THEN rowid ELSE NULL END \
         FROM sessions WHERE id = ?",
    )
    .bind(session_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap()
}

async fn plan_revisions(app: &TestApp, plan_id: i64) -> Vec<(i64, String)> {
    sqlx::query_as(
        "SELECT pr.revision_number, pr.content_hash \
         FROM plan_revisions pr JOIN sessions s ON s.id = pr.session_id \
         WHERE s.rowid = ? ORDER BY pr.revision_number",
    )
    .bind(plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap()
}

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

    let s: (String, String) =
        sqlx::query_as("SELECT id, plan_file_path FROM sessions WHERE id = ?")
            .bind("demo")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(s.0, "demo");
    assert!(s.1.ends_with("plan.md"));
    let active_plan_id = active_plan_id(&app, "demo").await;
    assert!(active_plan_id.is_some());

    let revisions = plan_revisions(&app, active_plan_id.unwrap()).await;
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
    assert_eq!(second["noop"], true);

    let revs = plan_revisions(&app, plan_id).await.len();
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

    let revs = plan_revisions(&app, plan_id).await;
    assert_eq!(revs.len(), 2, "planning edit adds a revision, same plan_id");
}

/// Implementing → plan-file edit is sealed out. It must not archive the
/// active plan or start a new one.
#[tokio::test]
async fn implementing_then_plan_edit_is_ignored() {
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
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let before_edit_max_event: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(id), 0) FROM events WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();

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
    assert_eq!(old_state, "implementing");

    let plan_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plans WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(plan_count, 1, "sealed plan edit must not start a new plan");
    let session_active = active_plan_id(&app, "s").await;
    assert_eq!(session_active, Some(plan_a));

    let plan_revision_events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' AND kind = 'plan_revision_created'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        plan_revision_events, 1,
        "sealed plan edit must not emit a new plan revision event"
    );

    let visible_events_after_edit: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE session_id = 's' AND id > ?")
            .bind(before_edit_max_event)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(
        visible_events_after_edit, 0,
        "sealed plan edit must not emit any lifecycle/UI event"
    );
}

#[tokio::test]
async fn finished_visible_session_cannot_start_second_plan_cycle() {
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
    app.state
        .lifecycle
        .finish_plan(&trinity::lifecycle::SessionId::from("s"), "test:finish")
        .await
        .unwrap();

    std::fs::write(&plan_path, "# new work\n").unwrap();
    let (status, body) = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
    assert!(
        body.contains("create a new session"),
        "unexpected body: {body}"
    );

    let plan_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plans WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(plan_count, 1);
}

#[tokio::test]
async fn feedback_via_service_succeeds_on_active_plan() {
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
        .put_feedback_via_service(
            "s",
            "rev",
            FeedbackTargetRef::PlanRevision(rev_id),
            "consider X",
        )
        .await
        .unwrap();
    assert!(post.was_insert);
    assert!(!post.was_no_op);
    let feedback_id = post.record.id;

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
    let rev_id = r["revision_id"].as_i64().unwrap();

    app.archive_via_service("s").await;

    let err = app
        .put_feedback_via_service("s", "rev", FeedbackTargetRef::PlanRevision(rev_id), "late")
        .await
        .unwrap_err();
    assert!(matches!(err, ServiceError::NoActivePlanForFeedback(_)));
}

#[tokio::test]
async fn current_feedback_view_via_service() {
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

    app.put_feedback_via_service(
        "s",
        "rev",
        FeedbackTargetRef::PlanRevision(rev_id),
        "comment",
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
        feedback,
        feedback_digest,
        ..
    } = view
    else {
        panic!("expected active");
    };
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].body, "comment");
    assert_eq!(feedback[0].author_label.as_str(), "rev");

    let view2 = app
        .state
        .lifecycle
        .current_feedback(&SessionId::from("s"), None)
        .await
        .unwrap();
    let CurrentFeedbackView::Active {
        feedback_digest: d2,
        ..
    } = view2
    else {
        panic!("expected active");
    };
    assert_eq!(feedback_digest, d2, "digest stable on re-read");
}

#[tokio::test]
async fn current_feedback_no_active_plan_via_service() {
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
    app.archive_via_service("s").await;

    let view = app
        .state
        .lifecycle
        .current_feedback(&SessionId::from("s"), None)
        .await
        .unwrap();
    assert!(matches!(view, CurrentFeedbackView::NoActivePlan { .. }));
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
