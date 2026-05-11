//! Code-level invariant tests for `sessions.active_plan_id`.
//!
//! The SQL partial unique index enforces "at most one non-archived plan per
//! session". This file covers the stricter rule the apply layer guarantees in
//! code: `sessions.active_plan_id`, when not NULL, points at a non-archived
//! plan whose `session_id` matches. We corrupt the DB by hand and assert
//! `plans::load_active_plan` returns `ActivePlanInconsistency::*` rather than
//! silently treating the pointer as `None`.

mod common;

use serde_json::json;

use common::TestApp;
use trinity::lifecycle::SessionId;
use trinity::storage::plans;

// Dangling pointer is prevented by the SQL FK (`sessions.active_plan_id
// REFERENCES plans(id)`) before our code-level checks even see the row, so
// no test here — the FK is the strictest defense.
//
// The cross-session and archived cases below exercise the consistency rules
// that the FK does NOT enforce.

#[tokio::test]
async fn cross_session_pointer_returns_inconsistent() {
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
    let plan_alpha = r["plan_id"].as_i64().unwrap();

    // Hand-insert a second session whose active_plan_id points at alpha's plan.
    sqlx::query(
        "INSERT INTO sessions (id, repo_root, plan_file_path, display_title, master_agent_id, active_plan_id, created_at, updated_at, archived_at) VALUES ('beta', '/other', '/p', NULL, NULL, ?, 1, 1, NULL)",
    )
    .bind(plan_alpha)
    .execute(&app.state.pool)
    .await
    .unwrap();

    let err = plans::load_active_plan(&app.state.pool, &SessionId::from("beta"))
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("different session"),
        "msg should mention cross-session: {msg}"
    );
}

#[tokio::test]
async fn pointer_to_archived_plan_returns_inconsistent() {
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
    let plan_id = r["plan_id"].as_i64().unwrap();

    // Hand-archive the plan but leave sessions.active_plan_id dangling.
    sqlx::query("UPDATE plans SET state = 'archived', archived_at = 1 WHERE id = ?")
        .bind(plan_id)
        .execute(&app.state.pool)
        .await
        .unwrap();

    let err = plans::load_active_plan(&app.state.pool, &SessionId::from("s"))
        .await
        .unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("archived"), "msg: {msg}");
}
