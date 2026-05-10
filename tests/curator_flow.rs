mod common;

use serde_json::json;

use common::TestApp;

async fn seed_plan_with_feedback(app: &TestApp) -> (String, i64, i64) {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# proposal\n").unwrap();
    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap().to_string();
    let revision_id = reg["revision_id"].as_i64().unwrap();

    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "claude-architect"}),
    )
    .await
    .unwrap();

    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("claude-architect"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "plan_revision",
                "target_id": revision_id.to_string(),
                "text": "consider edge case X",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();
    (plan_id, revision_id, event_id)
}

async fn post_form(app: &TestApp, path: &str, body: &str) -> reqwest::Response {
    app.client
        .post(format!("{}{}", app.base, path))
        .header("origin", "http://127.0.0.1")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.to_string())
        .send()
        .await
        .expect("post send")
}

async fn fetch_event_status(app: &TestApp, event_id: i64) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>("SELECT status FROM events WHERE id = ?")
        .bind(event_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn stage_then_unstage_round_trip() {
    let app = TestApp::spawn().await;
    let (plan_id, _, event_id) = seed_plan_with_feedback(&app).await;

    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/stage"),
        "",
    )
    .await;
    assert!(resp.status().is_redirection(), "status: {}", resp.status());
    assert_eq!(
        fetch_event_status(&app, event_id).await.as_deref(),
        Some("staged")
    );

    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/unstage"),
        "",
    )
    .await;
    assert!(resp.status().is_redirection(), "status: {}", resp.status());
    assert_eq!(
        fetch_event_status(&app, event_id).await.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn edit_changes_payload_text() {
    let app = TestApp::spawn().await;
    let (plan_id, _, event_id) = seed_plan_with_feedback(&app).await;
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/edit"),
        "text=curator+rewrote+this",
    )
    .await;
    assert!(resp.status().is_redirection());
    let payload: String = sqlx::query_scalar("SELECT payload FROM events WHERE id = ?")
        .bind(event_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert!(
        payload.contains("curator rewrote this"),
        "payload: {payload}"
    );
    assert!(
        payload.contains("edited_from"),
        "should keep prior payload as edited_from"
    );
}

#[tokio::test]
async fn delete_marks_feedback_withdrawn() {
    let app = TestApp::spawn().await;
    let (plan_id, _, event_id) = seed_plan_with_feedback(&app).await;
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/delete"),
        "",
    )
    .await;
    assert!(resp.status().is_redirection());
    assert_eq!(
        fetch_event_status(&app, event_id).await.as_deref(),
        Some("withdrawn")
    );
}

#[tokio::test]
async fn deliver_creates_batch_and_flips_status() {
    let app = TestApp::spawn().await;
    let (plan_id, _, event_id) = seed_plan_with_feedback(&app).await;

    // Stage first.
    post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/stage"),
        "",
    )
    .await;
    // Deliver.
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/deliver"),
        "target_kind=plan_revision&author=lloyd",
    )
    .await;
    assert!(resp.status().is_redirection(), "status: {}", resp.status());

    assert_eq!(
        fetch_event_status(&app, event_id).await.as_deref(),
        Some("delivered")
    );

    let batch: (i64, String, String, Option<i64>) = sqlx::query_as(
        "SELECT id, target_kind, delivered_by, acked_at FROM directive_batches WHERE plan_id = ?",
    )
    .bind(&plan_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(batch.1, "plan_revision");
    assert_eq!(batch.2, "human:lloyd");
    assert!(batch.3.is_none(), "fresh batch should be unacked");

    let item: (i64, i64) =
        sqlx::query_as("SELECT batch_id, event_id FROM directive_batch_items WHERE batch_id = ?")
            .bind(batch.0)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(item, (batch.0, event_id));
}

#[tokio::test]
async fn deliver_with_nothing_staged_errors() {
    let app = TestApp::spawn().await;
    let (plan_id, _, _) = seed_plan_with_feedback(&app).await;
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/deliver"),
        "target_kind=plan_revision",
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(body.contains("nothing staged"), "body: {body}");
}

#[tokio::test]
async fn comment_appends_event() {
    let app = TestApp::spawn().await;
    let (plan_id, _, _) = seed_plan_with_feedback(&app).await;
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/comment"),
        "text=please+reconsider+section+2&author=lloyd",
    )
    .await;
    assert!(resp.status().is_redirection());
    let row: (String, String, String) = sqlx::query_as(
        "SELECT actor, payload, kind FROM events WHERE plan_id = ? AND kind = 'human_comment'",
    )
    .bind(&plan_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(row.0, "human:lloyd");
    assert!(row.1.contains("please reconsider section 2"));
    assert_eq!(row.2, "human_comment");
}

#[tokio::test]
async fn archive_transitions_state_and_unwatches() {
    let app = TestApp::spawn().await;
    let (plan_id, _, _) = seed_plan_with_feedback(&app).await;
    let resp = post_form(&app, &format!("/plans/{plan_id}/archive"), "").await;
    assert!(resp.status().is_redirection());
    let row: (String, Option<i64>) =
        sqlx::query_as("SELECT state, archived_at FROM plans WHERE id = ?")
            .bind(&plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(row.0, "archived");
    assert!(row.1.is_some());
}

#[tokio::test]
async fn rename_updates_display_title() {
    let app = TestApp::spawn().await;
    let (plan_id, _, _) = seed_plan_with_feedback(&app).await;
    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/rename"),
        "display_title=add+gzip+compression",
    )
    .await;
    assert!(resp.status().is_redirection());
    let title: Option<String> = sqlx::query_scalar("SELECT display_title FROM plans WHERE id = ?")
        .bind(&plan_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(title.as_deref(), Some("add gzip compression"));
}

#[tokio::test]
async fn evict_master_clears_master_agent_and_emits_event() {
    let app = TestApp::spawn().await;
    let (plan_id, _, _) = seed_plan_with_feedback(&app).await;
    let resp = post_form(&app, &format!("/plans/{plan_id}/evict-master"), "").await;
    assert!(resp.status().is_redirection());
    let row: (Option<i64>,) = sqlx::query_as("SELECT master_agent_id FROM plans WHERE id = ?")
        .bind(&plan_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert!(row.0.is_none());

    // Reviewer survives, master is gone.
    let agents: Vec<String> = sqlx::query_scalar("SELECT role FROM agents WHERE plan_id = ?")
        .bind(&plan_id)
        .fetch_all(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(agents, vec!["reviewer".to_string()]);

    let kinds: Vec<String> =
        sqlx::query_scalar("SELECT kind FROM events WHERE plan_id = ? AND kind = 'master_evicted'")
            .bind(&plan_id)
            .fetch_all(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(kinds.len(), 1);
}

#[tokio::test]
async fn delivered_feedback_cannot_be_restaged() {
    let app = TestApp::spawn().await;
    let (plan_id, _, event_id) = seed_plan_with_feedback(&app).await;
    post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/stage"),
        "",
    )
    .await;
    post_form(
        &app,
        &format!("/plans/{plan_id}/deliver"),
        "target_kind=plan_revision",
    )
    .await;

    let resp = post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{event_id}/unstage"),
        "",
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("cannot be modified") || body.contains("delivered"),
        "body: {body}"
    );
}
