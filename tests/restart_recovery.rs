mod common;

use serde_json::json;

use common::{TestApp, make_commit};

/// Planning plan survives restart; watcher continues against the same
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

/// Implementing plan survives restart and a post-impl plan-file edit
/// archives the active plan and starts a new one. Implementation
/// commits are observed by the git logs/HEAD watcher; we make a commit
/// then wait for the watcher to pick it up. The `register_implementation_commit`
/// MCP tool is gone — that flow is fully automatic now.
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

    // First commit. The git-logs watcher transitions plan_a from
    // planning → implementing.
    let head = make_commit(&app.repo, "feature.txt", "x\n");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let impl_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_a)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(impl_count, 1, "first commit observed via watcher");

    let app = app.restart().await;

    // Same HEAD across the restart: recovery's HEAD-drift check must
    // see the SHA is already known and emit no new impl row.
    let impl_count_after_restart: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_a)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(
        impl_count_after_restart, 1,
        "restart at same HEAD must not duplicate impl row"
    );
    let _ = head;

    // Post-impl plan-file edit: archive plan_a + start new plan.
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
    app.archive_via_service("s").await;
    let active: Option<i64> =
        sqlx::query_scalar("SELECT active_plan_id FROM sessions WHERE id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert!(active.is_none());

    let app = app.restart().await;

    std::fs::write(&plan_path, "# new content out of band\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let plan_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM plans WHERE session_id = 's' AND state != 'archived'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(plan_count, 0, "no new active plan from drift alone");

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

/// Feedback files dropped before the daemon was up must be ingested
/// when the daemon comes back: recovery scans the dir and queues an
/// ingest pass for every file whose hash doesn't match the sidecar.
#[tokio::test]
async fn feedback_file_changed_while_daemon_off_is_re_ingested() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": "s", "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    let rev_id = r["revision_id"].as_i64().unwrap();
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    let p = dir.join("rev-a.md");
    std::fs::write(&p, "initial\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let app = app.restart().await;

    // Write while we're "still off" then restart again.
    std::fs::write(&p, "updated while off\n").unwrap();
    let app = app.restart().await;

    let body: String = sqlx::query_scalar(
        "SELECT body FROM feedback WHERE session_id = 's' AND author_label = 'rev-a' AND target_id = ?",
    )
    .bind(rev_id.to_string())
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        body, "updated while off\n",
        "recovery must re-ingest the updated body"
    );
}

/// Commits made while the daemon was off must be observed on restart.
#[tokio::test]
async fn commit_made_while_daemon_off_is_observed_on_restart() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
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

    let app = app.restart().await;

    // While restarted, make a commit. We did it after spawn, so the
    // watcher *can* pick it up too; the point is recovery's HEAD-drift
    // detection alone is enough to observe.
    make_commit(&app.repo, "f.txt", "x\n");
    let app = app.restart().await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "HEAD drift detection on restart records the commit"
    );
}

#[tokio::test]
async fn unchanged_impl_feedback_is_not_retargeted_on_restart_after_amend() {
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

    let sha_a = make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let impl_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("impl");
    std::fs::write(impl_dir.join("rev-a.md"), "same words\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    std::fs::write(app.repo.join("f.txt"), "x\ny\n").unwrap();
    common::run_git(&app.repo, &["add", "f.txt"]);
    common::run_git(&app.repo, &["commit", "-q", "--amend", "--no-edit"]);
    let sha_b = common::run_git(&app.repo, &["rev-parse", "HEAD"]);
    assert_ne!(sha_a, sha_b);
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let app = app.restart().await;

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback \
         WHERE session_id = 's' AND author_label = 'rev-a' AND target_kind = 'implementation_commit'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        total, 1,
        "restart recovery must not duplicate unchanged feedback onto the amended commit"
    );

    let target_id: String = sqlx::query_scalar(
        "SELECT target_id FROM feedback \
         WHERE session_id = 's' AND author_label = 'rev-a' AND target_kind = 'implementation_commit'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        target_id, sha_a,
        "unchanged feedback remains attached to the commit it reviewed"
    );

    let retargeted: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback \
         WHERE session_id = 's' AND author_label = 'rev-a' \
           AND target_kind = 'implementation_commit' AND target_id = ?",
    )
    .bind(&sha_b)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(retargeted, 0);

    let feedback_added: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events \
         WHERE session_id = 's' AND kind = 'feedback_added' AND actor = 'agent:rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        feedback_added, 1,
        "restart must not emit a new feedback_added event"
    );

    let ctx = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(ctx["write_feedback"]["status"], "stale");
    assert_eq!(
        ctx["write_feedback"]["last_ingested_target"]["id"].as_str(),
        Some(sha_a.as_str())
    );
}

#[tokio::test]
async fn agents_last_seen_survives_restart() {
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
    let before: i64 = sqlx::query_scalar(
        "SELECT last_seen FROM agents WHERE session_id = 's' AND label = 'alice'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert!(before > 0);

    let app = app.restart().await;

    let after: i64 = sqlx::query_scalar(
        "SELECT last_seen FROM agents WHERE session_id = 's' AND label = 'alice'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        after, before,
        "agents.last_seen must not be reset on daemon restart"
    );
}
