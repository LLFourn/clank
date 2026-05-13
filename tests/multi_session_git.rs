//! Multi-session-in-one-repo behaviour. Sessions share `.git/logs/HEAD`,
//! but plan discovery is passive: the repo's effective session receives
//! commits and other active sessions remain open.

mod common;

use std::path::PathBuf;

use serde_json::json;

use common::{TestApp, make_commit};

use common::SETTLE;

#[tokio::test]
async fn registering_new_session_does_not_supersede_effective_session() {
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

    let a_state: (Option<i64>, String, Option<i64>) = sqlx::query_as(
        "SELECT CASE WHEN s.state IN ('planning', 'implementing') THEN s.rowid ELSE NULL END, \
                s.state, s.archived_at \
         FROM sessions s WHERE s.id = 'a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let b_state: (Option<i64>, String, Option<i64>) = sqlx::query_as(
        "SELECT CASE WHEN s.state IN ('planning', 'implementing') THEN s.rowid ELSE NULL END, \
                s.state, s.archived_at \
         FROM sessions s WHERE s.id = 'b'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(a_state.0, Some(plan_id_a));
    assert_eq!(a_state.1, "planning");
    assert!(a_state.2.is_none());
    assert_eq!(b_state.0, Some(plan_id_b));
    assert_eq!(b_state.1, "planning");
    assert!(b_state.2.is_none());

    let effective: String =
        sqlx::query_scalar("SELECT session_id FROM repo_effective_sessions WHERE repo_root = ?")
            .bind(
                dunce::canonicalize(&app.repo)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(effective, "a", "first session remains in effect");

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    let old_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM implementation_revisions ir \
             JOIN sessions s ON s.id = ir.session_id \
             WHERE s.rowid = ?",
    )
    .bind(plan_id_a)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let new_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM implementation_revisions ir \
             JOIN sessions s ON s.id = ir.session_id \
             WHERE s.rowid = ?",
    )
    .bind(plan_id_b)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(old_count, 1, "effective session should observe the commit");
    assert_eq!(
        new_count, 0,
        "passively registered session must not observe commits"
    );
}

#[tokio::test]
async fn claim_switches_commit_routing_without_archiving_sessions() {
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

    let resp = app.post_form("/sessions/b/claim", "").await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    let count_a: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM implementation_revisions ir \
             JOIN sessions s ON s.id = ir.session_id \
             WHERE s.rowid = ?",
    )
    .bind(plan_id_a)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let count_b: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM implementation_revisions ir \
             JOIN sessions s ON s.id = ir.session_id \
             WHERE s.rowid = ?",
    )
    .bind(plan_id_b)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count_a, 0);
    assert_eq!(count_b, 1);

    let a_archived: Option<i64> =
        sqlx::query_scalar("SELECT archived_at FROM sessions WHERE id = 'a'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(a_archived.is_none(), "claiming b must not archive a");
}

#[tokio::test]
async fn feedback_files_for_parallel_sessions_target_their_own_plans() {
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
    assert_eq!(count_a, 1, "parallel session keeps its active target");
    assert_eq!(body_b, "for-b\n");
}

#[tokio::test]
async fn recovery_preserves_parallel_active_sessions_and_effective_claim() {
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

    let app = app.restart().await;

    let a_state: (Option<i64>, String, Option<i64>) = sqlx::query_as(
        "SELECT CASE WHEN s.state IN ('planning', 'implementing') THEN s.rowid ELSE NULL END, \
                s.state, s.archived_at \
         FROM sessions s WHERE s.id = 'a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let b_active: Option<i64> = sqlx::query_scalar(
        "SELECT CASE WHEN state IN ('planning', 'implementing') THEN rowid ELSE NULL END \
             FROM sessions WHERE id = 'b'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();

    assert_eq!(a_state.0, Some(plan_id_a));
    assert_eq!(a_state.1, "planning");
    assert!(a_state.2.is_none());
    assert_eq!(b_active, Some(plan_id_b));

    let effective: String =
        sqlx::query_scalar("SELECT session_id FROM repo_effective_sessions WHERE repo_root = ?")
            .bind(
                dunce::canonicalize(&app.repo)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            )
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(effective, "a");
}
