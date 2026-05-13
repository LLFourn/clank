//! Multi-session-in-one-repo behaviour. Sessions share `.git/logs/HEAD`,
//! so only the newest active session in a repo may stay open.

mod common;

use std::path::PathBuf;

use serde_json::json;

use common::{TestApp, make_commit};

const SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

#[tokio::test]
async fn newer_session_in_same_repo_supersedes_older_session_for_commits() {
    let app = TestApp::spawn().await;
    let plan_a = app.repo.join("plan-a.md");
    let plan_b = app.repo.join("plan-b.md");
    std::fs::write(&plan_a, "# a\n").unwrap();
    std::fs::write(&plan_b, "# b\n").unwrap();

    let ra = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "a", "path": &plan_a, "label": "m"}),
        )
        .await
        .unwrap();
    let rb = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "b", "path": &plan_b, "label": "m"}),
        )
        .await
        .unwrap();
    let plan_id_a = ra["plan_id"].as_i64().unwrap();
    let plan_id_b = rb["plan_id"].as_i64().unwrap();

    let a_state: (Option<i64>, String) = sqlx::query_as(
        "SELECT s.active_plan_id, p.state \
         FROM sessions s JOIN plans p ON p.id = ? \
         WHERE s.id = 'a'",
    )
    .bind(plan_id_a)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(a_state, (None, "archived".to_string()));

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    let old_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id_a)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    let new_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id_b)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(old_count, 0, "superseded session must not observe commits");
    assert_eq!(new_count, 1, "current session should observe the commit");
}

#[tokio::test]
async fn feedback_files_for_superseded_session_do_not_target_new_plan() {
    let app = TestApp::spawn().await;
    let plan_a = app.repo.join("plan-a.md");
    let plan_b = app.repo.join("plan-b.md");
    std::fs::write(&plan_a, "# a\n").unwrap();
    std::fs::write(&plan_b, "# b\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "a", "path": &plan_a, "label": "m"}),
    )
    .await
    .unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "b", "path": &plan_b, "label": "m"}),
    )
    .await
    .unwrap();

    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = |sid: &str| -> PathBuf {
        canonical_repo
            .join(".trinity")
            .join("feedback")
            .join(sid)
            .join("plan")
    };

    std::fs::write(plan_dir("a").join("rev.md"), "for-a\n").unwrap();
    std::fs::write(plan_dir("b").join("rev.md"), "for-b\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let count_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback WHERE session_id = 'a' AND author_label = 'rev'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let body_b: String = sqlx::query_scalar(
        "SELECT body FROM feedback WHERE session_id = 'b' AND author_label = 'rev'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count_a, 0, "superseded session has no active target");
    assert_eq!(body_b, "for-b\n");
}

#[tokio::test]
async fn recovery_archives_superseded_active_sessions_in_same_repo() {
    let app = TestApp::spawn().await;
    let plan_a = app.repo.join("plan-a.md");
    let plan_b = app.repo.join("plan-b.md");
    std::fs::write(&plan_a, "# a\n").unwrap();
    std::fs::write(&plan_b, "# b\n").unwrap();

    let ra = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "a", "path": &plan_a, "label": "m"}),
        )
        .await
        .unwrap();
    let rb = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "b", "path": &plan_b, "label": "m"}),
        )
        .await
        .unwrap();
    let plan_id_a = ra["plan_id"].as_i64().unwrap();
    let plan_id_b = rb["plan_id"].as_i64().unwrap();

    sqlx::query("UPDATE plans SET state = 'planning', archived_at = NULL WHERE id = ?")
        .bind(plan_id_a)
        .execute(&app.state.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET active_plan_id = ? WHERE id = 'a'")
        .bind(plan_id_a)
        .execute(&app.state.pool)
        .await
        .unwrap();

    let app = app.restart().await;

    let a_state: (Option<i64>, String) = sqlx::query_as(
        "SELECT s.active_plan_id, p.state \
         FROM sessions s JOIN plans p ON p.id = ? \
         WHERE s.id = 'a'",
    )
    .bind(plan_id_a)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let b_active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 'b'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();

    assert_eq!(a_state, (None, "archived".to_string()));
    assert_eq!(b_active, Some(plan_id_b));
}
