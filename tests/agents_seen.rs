//! `agents.last_seen` semantics under the watcher-coordinator model.
//!
//! - Any tool call that carries a label upserts the agents row.
//! - First insert emits one `agent_joined` event; subsequent calls don't.
//! - Read-path calls (`get_context`) bump `last_seen` but never touch
//!   `sessions.updated_at` or emit events beyond the first-sight one.
//! - Service-layer writes (`SessionService::put_feedback`) follow the
//!   same first-sight rule when the label hadn't been seen before.

mod common;

use serde_json::json;

use common::TestApp;
use trinity::domain::FeedbackTargetRef;

#[tokio::test]
async fn any_labeled_call_upserts_seen() {
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

    app.put_feedback_via_service(
        "s",
        "rev",
        FeedbackTargetRef::PlanRevision(rev_id),
        "feedback from rev",
    )
    .await
    .unwrap();

    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT label, last_seen FROM agents WHERE session_id = 's' ORDER BY label")
            .fetch_all(&app.state.pool)
            .await
            .unwrap();
    let labels: Vec<&str> = rows.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(labels, vec!["claude-main", "rev"]);
}

#[tokio::test]
async fn register_plan_file_is_not_a_claim() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();

    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "alice"}),
    )
    .await
    .unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "bob"}),
    )
    .await
    .unwrap();

    let labels: Vec<String> =
        sqlx::query_scalar("SELECT label FROM agents WHERE session_id = 's' ORDER BY label")
            .fetch_all(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(labels, vec!["alice", "bob"]);
}

#[tokio::test]
async fn first_seen_label_emits_event_subsequent_calls_silent() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();

    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "alice"}),
    )
    .await
    .unwrap();

    for _ in 0..3 {
        app.call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "alice"}),
        )
        .await
        .unwrap();
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' AND kind = 'agent_joined'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "exactly one agent_joined event for 'alice'");
}

#[tokio::test]
async fn read_path_does_not_churn_session_updated_at() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();

    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "alice"}),
    )
    .await
    .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT updated_at FROM sessions WHERE id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    for _ in 0..5 {
        app.call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "alice"}),
        )
        .await
        .unwrap();
    }

    let after: i64 = sqlx::query_scalar("SELECT updated_at FROM sessions WHERE id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "read-only get_context must not bump sessions.updated_at"
    );
}

#[tokio::test]
async fn first_seen_via_read_path_emits_agent_joined() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "alice"}),
    )
    .await
    .unwrap();

    for _ in 0..3 {
        app.call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "bob"}),
        )
        .await
        .unwrap();
    }
    let bob_joins: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' \
         AND kind = 'agent_joined' AND actor = 'agent:bob'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(bob_joins, 1, "exactly one agent_joined for 'bob'");
}

#[tokio::test]
async fn read_tools_on_unknown_session_are_404_with_label() {
    let app = TestApp::spawn().await;

    let (status, _) = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "ghost", "author_label": "alice"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    let agent_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE label = 'alice'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(
        agent_rows, 0,
        "unknown-session reads must not leave a phantom agents row"
    );
}

/// Multiple `get_context` calls with the same label must produce
/// exactly one `agent_joined` event (the first-sight one) and exactly
/// one `agents` row.
#[tokio::test]
async fn get_context_with_label_upserts_seen_once() {
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

    for _ in 0..4 {
        app.call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "polly"}),
        )
        .await
        .unwrap();
    }

    let polly_joins: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' AND kind = 'agent_joined' AND actor = 'agent:polly'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(polly_joins, 1);

    let polly_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agents WHERE session_id = 's' AND label = 'polly'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(polly_rows, 1);
}

#[tokio::test]
async fn session_detail_does_not_render_evict_master() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "alice"}),
    )
    .await
    .unwrap();
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(
        !body.contains("/evict-master"),
        "session detail still mentions the removed /evict-master action"
    );
    assert!(
        !body.contains("Evict master"),
        "session detail still mentions removed 'Evict master' wording"
    );
}

#[tokio::test]
async fn rejected_put_feedback_does_not_record_agent_joined() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "alice"}),
    )
    .await
    .unwrap();

    app.archive_via_service("s").await;

    let err = app
        .put_feedback_via_service(
            "s",
            "dave",
            FeedbackTargetRef::PlanRevision(1),
            "should be rejected",
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        trinity::daemon::ServiceError::NoActivePlanForFeedback(_)
    ));

    let dave_joins: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' \
         AND kind = 'agent_joined' AND actor = 'agent:dave'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        dave_joins, 0,
        "rejected put_feedback must not log agent_joined"
    );

    let dave_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE session_id = 's' AND label = 'dave'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(dave_rows, 0);
}
