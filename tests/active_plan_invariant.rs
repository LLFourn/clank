//! Code-level invariant tests for the one-plan session lifecycle.
//!
//! The lifecycle owner is the `sessions` row. There is no separate
//! `plans` table and no cross-session active-plan pointer to corrupt.

mod common;

use serde_json::json;

use common::TestApp;
use trinity::lifecycle::SessionId;
use trinity::storage::plans;

#[tokio::test]
async fn active_session_loads_from_session_lifecycle_row() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "x").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "alpha", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let plan_id = r["plan_id"].as_i64().unwrap();

    let active = plans::load_active_plan(&app.state.pool, &SessionId::from("alpha"))
        .await
        .unwrap()
        .expect("registered session is active");
    assert_eq!(active.id, plan_id);
    assert_eq!(active.session_id, "alpha");
    assert_eq!(active.state, "planning");
}

#[tokio::test]
async fn terminal_session_state_is_not_active() {
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

    sqlx::query(
        "UPDATE sessions SET state = 'archived', archived_at = 1, updated_at = 1 WHERE id = 's'",
    )
    .execute(&app.state.pool)
    .await
    .unwrap();

    let active = plans::load_active_plan(&app.state.pool, &SessionId::from("s"))
        .await
        .unwrap();
    assert!(active.is_none());
}
