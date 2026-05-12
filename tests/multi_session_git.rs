//! Two-session-in-one-repo behaviour. Sessions share `.git/logs/HEAD`
//! (one watch on the notify side, dedupe per-session at the emit
//! layer). Feedback directories are per-session.

mod common;

use std::path::PathBuf;

use serde_json::json;

use common::{TestApp, make_commit};

const SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

#[tokio::test]
async fn two_sessions_one_repo_share_head_watcher_and_both_observe() {
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

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    for plan_id in [plan_id_a, plan_id_b] {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
                .bind(plan_id)
                .fetch_one(&app.state.pool)
                .await
                .unwrap();
        assert_eq!(
            count, 1,
            "both sessions sharing logs/HEAD should each observe the commit (plan {plan_id})"
        );
    }
}

#[tokio::test]
async fn per_session_feedback_files_are_independent() {
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

    let body_a: String = sqlx::query_scalar(
        "SELECT body FROM feedback WHERE session_id = 'a' AND author_label = 'rev'",
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
    assert_eq!(body_a, "for-a\n");
    assert_eq!(body_b, "for-b\n");
}
