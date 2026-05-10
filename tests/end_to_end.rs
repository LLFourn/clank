mod common;

use std::path::Path;
use std::process::Command;

use serde_json::json;

use common::TestApp;

fn run_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        output.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
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

/// Walks the entire spec'd v0 loop:
/// register plan → reviewer joins → reviewer posts → curator stages+delivers
/// → master polls + acks → master edits plan → watcher snapshots →
/// approve plan → master commits + registers impl → reviewer posts impl
/// feedback → curator delivers → master polls + acks → mark done.
#[tokio::test]
async fn end_to_end_v0_spec_walk() {
    let app = TestApp::spawn().await;

    // Master writes plan and registers.
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# proposal v1\n\n- step\n").unwrap();
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

    // Reviewer joins and posts.
    app.call(
        "join_plan",
        &app.repo,
        None,
        json!({"plan_id_or_path": &plan_id, "label": "codex-pragmatist"}),
    )
    .await
    .unwrap();
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("codex-pragmatist"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "plan_revision",
                "target_id": revision_id.to_string(),
                "text": "what about idempotency?",
            }),
        )
        .await
        .unwrap();
    let event_id = post["event_id"].as_i64().unwrap();

    // Curator stages + delivers.
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

    // Master polls and acks.
    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    let batch_id = poll["batch_id"].as_i64().unwrap();
    app.call(
        "ack_directive",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "batch_id": batch_id}),
    )
    .await
    .unwrap();

    // Master edits the plan file → watcher snapshots a new revision.
    std::fs::write(&plan_path, "# proposal v1\n\n- step\n- idempotency note\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let revs: Vec<i64> = sqlx::query_scalar(
        "SELECT revision_number FROM plan_revisions WHERE plan_id = ? ORDER BY revision_number",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(revs, vec![1, 2], "watcher should have created revision 2");

    // Curator approves plan.
    let resp = post_form(&app, &format!("/plans/{plan_id}/approve"), "").await;
    assert!(resp.status().is_redirection());

    // Master implements + commits.
    std::fs::write(app.repo.join("impl.txt"), "implementation\n").unwrap();
    run_git(&app.repo, &["add", "impl.txt"]);
    run_git(&app.repo, &["commit", "-q", "-m", "add implementation"]);

    let reg_impl = app
        .call(
            "register_implementation_commit",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id, "commit_sha": "HEAD"}),
        )
        .await
        .unwrap();
    let commit_sha = reg_impl["commit_sha"].as_str().unwrap().to_string();

    // Reviewer fetches impl context.
    let ctx = app
        .call(
            "get_review_context",
            &app.repo,
            Some("codex-pragmatist"),
            json!({"plan_id": &plan_id, "target": "implementation"}),
        )
        .await
        .unwrap();
    assert_eq!(
        ctx["latest_implementation_revision"]["commit_sha"],
        commit_sha
    );

    // Reviewer posts impl-stage feedback.
    let post = app
        .call(
            "add_feedback",
            &app.repo,
            Some("codex-pragmatist"),
            json!({
                "plan_id": &plan_id,
                "target_kind": "implementation_commit",
                "target_id": &commit_sha,
                "text": "missing test coverage",
            }),
        )
        .await
        .unwrap();
    let impl_event_id = post["event_id"].as_i64().unwrap();
    post_form(
        &app,
        &format!("/plans/{plan_id}/feedback/{impl_event_id}/stage"),
        "",
    )
    .await;
    post_form(
        &app,
        &format!("/plans/{plan_id}/deliver"),
        "target_kind=implementation_commit",
    )
    .await;

    // Master polls impl directive.
    let poll = app
        .call(
            "poll_directive",
            &app.repo,
            Some("claude-main"),
            json!({"plan_id": &plan_id}),
        )
        .await
        .unwrap();
    assert_eq!(poll["target_kind"], "implementation_commit");
    let items = poll["items"].as_array().unwrap();
    assert_eq!(items[0]["target_id"], commit_sha);
    let batch_id = poll["batch_id"].as_i64().unwrap();
    app.call(
        "ack_directive",
        &app.repo,
        Some("claude-main"),
        json!({"plan_id": &plan_id, "batch_id": batch_id}),
    )
    .await
    .unwrap();

    // Curator marks done.
    let resp = post_form(&app, &format!("/plans/{plan_id}/mark-done"), "").await;
    assert!(resp.status().is_redirection());
    let final_state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
        .bind(&plan_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(final_state, "done");

    // Sanity: state transition trail recorded.
    let transitions: Vec<String> = sqlx::query_scalar(
        "SELECT payload FROM events WHERE plan_id = ? AND kind = 'state_transition' ORDER BY id",
    )
    .bind(&plan_id)
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    let joined = transitions.join("\n");
    assert!(
        joined.contains("\"from\":\"planning\""),
        "missing planning transition: {joined}"
    );
    assert!(
        joined.contains("\"to\":\"implementation_review\""),
        "missing impl transition: {joined}"
    );
    assert!(
        joined.contains("\"to\":\"done\""),
        "missing done transition: {joined}"
    );
}

#[tokio::test]
async fn mark_done_rejected_from_planning_state() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
    let reg = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"path": &plan_path, "label": "claude-main"}),
        )
        .await
        .unwrap();
    let plan_id = reg["plan_id"].as_str().unwrap();

    let resp = post_form(&app, &format!("/plans/{plan_id}/mark-done"), "").await;
    assert_eq!(resp.status(), 400);
}
