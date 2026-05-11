mod common;

use serde_json::json;

use common::{TestApp, make_commit};

/// Rule: Planning plan survives restart; watcher continues against the same
/// plan_id.
#[tokio::test]
async fn planning_plan_survives_restart() {
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
    let plan_id = r["plan_id"].as_i64().unwrap();
    std::fs::write(&plan_path, "# v2\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let rev_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM plan_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(rev_count, 2);

    let app = app.restart().await;

    std::fs::write(&plan_path, "# v3\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let rev_count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM plan_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(rev_count_after, 3, "third revision on same plan_id");

    let active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(active, Some(plan_id));
}

/// Rule: Implementing plan survives restart; same-SHA commit re-register is
/// idempotent; post-impl plan-file edit archives the active plan and starts a
/// new one based on current HEAD.
#[tokio::test]
async fn implementing_plan_survives_restart_and_amend_works() {
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
    let head = make_commit(&app.repo, "feature.txt", "x\n");
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "commit_sha": &head}),
    )
    .await
    .unwrap();

    let app = app.restart().await;

    // Same SHA re-register: must NOT add a duplicate impl revision.
    app.call(
        "register_implementation_commit",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "commit_sha": &head}),
    )
    .await
    .unwrap();
    let impl_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_a)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(impl_count, 1, "duplicate SHA must be a no-op");

    // Edit the plan file. Post-impl edit: archive plan_a + start new active plan.
    std::fs::write(&plan_path, "# totally new task\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let old_state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
        .bind(plan_a)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(old_state, "archived");
    let new_active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(new_active.is_some());
    assert_ne!(new_active, Some(plan_a));
}

/// Rule: No-active session — file drift alone does not start a plan.
#[tokio::test]
async fn no_active_session_drift_does_not_start_plan() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# original\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();
    // Archive the active plan.
    let resp = app.post_form("/sessions/s/archive", "").await;
    assert!(resp.status().is_redirection());
    let active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(active.is_none());

    let app = app.restart().await;

    // Edit the watched plan file. The watcher should NOT silently start a new plan.
    std::fs::write(&plan_path, "# new content out of band\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let plan_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plans WHERE session_id = 's' AND state != 'archived'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(plan_count, 0, "no new active plan from drift alone");

    // Explicit register_plan_file starts the next lifecycle.
    app.call(
        "register_plan_file",
        &app.repo,
        Some("m"),
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();
    let active_now: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(active_now.is_some());
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM plans WHERE session_id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(total, 2, "1 archived + 1 new active");
}
