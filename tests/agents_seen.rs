//! `agents.last_seen` semantics:
//! - Any tool call that carries a label upserts the agents row.
//! - First insert emits one `agent_joined` event; subsequent calls don't.
//! - Read-path calls (`get_current_feedback`, `get_review_context`) bump
//!   `last_seen` but never touch `sessions.updated_at` or emit events.

mod common;

use serde_json::json;

use common::TestApp;

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

    app.call(
        "put_feedback",
        &app.repo,
        None,
        json!({
            "session_id": "s",
            "target_kind": "plan_revision",
            "target_id": rev_id.to_string(),
            "body": "feedback from rev",
            "author_label": "rev",
        }),
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
    // Re-register with a different label immediately — must succeed.
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

    // Read-path calls multiple times — must not pile up agent_joined events.
    for _ in 0..3 {
        app.call(
            "get_current_feedback",
            &app.repo,
            None,
            json!({"session_id": "s", "label": "alice"}),
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

    // Sleep so a naive (still-erroneous) implementation that bumped
    // updated_at on reads would write a strictly-greater timestamp.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    for _ in 0..5 {
        app.call(
            "get_current_feedback",
            &app.repo,
            None,
            json!({"session_id": "s", "label": "alice"}),
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
        "read-only get_current_feedback must not bump sessions.updated_at"
    );
}

/// The `agent_joined` invariant is "exactly once per (session_id, label)"
/// — read paths count, not just write paths. A label that first shows up
/// via `get_current_feedback` must produce one event, not zero.
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

    // 'bob' has never been seen. A get_current_feedback with this label
    // is bob's first appearance. We expect ONE agent_joined event.
    for _ in 0..3 {
        app.call(
            "get_current_feedback",
            &app.repo,
            None,
            json!({"session_id": "s", "label": "bob"}),
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

/// An unknown session_id must surface as 404 from
/// `register_implementation_commit` and must not leave a phantom
/// `agents` row. (The upsert lives behind the existence check so a
/// non-existent session_id can't trip the agents.session_id FK and
/// surface as a 500.)
#[tokio::test]
async fn register_impl_on_unknown_session_is_404_no_agent_row() {
    let app = TestApp::spawn().await;
    let (status, body) = app
        .call(
            "register_implementation_commit",
            &app.repo,
            None,
            json!({
                "session_id": "ghost",
                "commit_sha": "HEAD",
                "label": "carol",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "body: {body}");

    let agent_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE label = 'carol'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(
        agent_rows, 0,
        "an unknown session must not insert an agents row"
    );
}

/// Same invariant as the write-tool case, but for the read tools. The
/// shim's per-tool autofill means `label` is frequently present on
/// reads without the caller thinking about it, so an unknown session_id
/// must still surface as 404 — not as a 500 from the agents FK.
#[tokio::test]
async fn read_tools_on_unknown_session_are_404_with_label() {
    let app = TestApp::spawn().await;

    let (status, _) = app
        .call(
            "get_current_feedback",
            &app.repo,
            None,
            json!({"session_id": "ghost", "label": "alice"}),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::NOT_FOUND);

    let (status, _) = app
        .call(
            "get_review_context",
            &app.repo,
            None,
            json!({"session_id": "ghost", "label": "alice"}),
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

/// The toolbar must not advertise actions whose POST routes have been
/// deleted. (`/evict-master` was the last role-era handler; clicking
/// would 404.)
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

/// A rejected `put_feedback` (no active plan, target not in active
/// plan, empty body) must not leave a phantom `agent_joined` event or
/// `agents` row. The label upsert sits behind validation.
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

    // Archive so the session has no active plan; the next put_feedback
    // will be rejected at the service layer.
    let resp = app.post_form("/sessions/s/archive", "").await;
    assert!(resp.status().is_redirection());

    let (status, _) = app
        .call(
            "put_feedback",
            &app.repo,
            None,
            json!({
                "session_id": "s",
                "target_kind": "plan_revision",
                "target_id": "1",
                "body": "should be rejected",
                "author_label": "dave",
            }),
        )
        .await
        .expect_err();
    assert_eq!(status, reqwest::StatusCode::FORBIDDEN);

    // 'dave' has never been seen successfully, so no agent_joined event.
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

    // Also: no agents row for 'dave'.
    let dave_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE session_id = 's' AND label = 'dave'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(dave_rows, 0);
}
