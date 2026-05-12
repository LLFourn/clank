//! Auto-observation of git HEAD via `.git/logs/HEAD` watcher.
//!
//! Implementation commits no longer require an explicit MCP call;
//! every commit / amend / reset on a registered session's repo is
//! observed and fed through the lifecycle automatically. The apply
//! layer is the idempotency layer: re-observing a known SHA emits a
//! `head_reset_to_known_sha` audit event without inserting a duplicate
//! `implementation_revisions` row.

mod common;

use serde_json::json;

use common::{TestApp, make_commit};

const SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

async fn register(app: &TestApp, sid: &str) -> i64 {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    let r = app
        .call(
            "register_plan_file",
            &app.repo,
            None,
            json!({"session_id": sid, "path": &plan_path, "label": "m"}),
        )
        .await
        .unwrap();
    r["plan_id"].as_i64().unwrap()
}

#[tokio::test]
async fn commit_after_register_is_auto_observed() {
    let app = TestApp::spawn().await;
    let plan_id = register(&app, "s").await;

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let state: String = sqlx::query_scalar("SELECT state FROM plans WHERE id = ?")
        .bind(plan_id)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(state, "implementing");
}

#[tokio::test]
async fn amend_after_register_records_second_revision() {
    let app = TestApp::spawn().await;
    let plan_id = register(&app, "s").await;

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    std::fs::write(app.repo.join("f.txt"), "x\ny\n").unwrap();
    common::run_git(&app.repo, &["add", "f.txt"]);
    common::run_git(&app.repo, &["commit", "-q", "--amend", "--no-edit"]);
    tokio::time::sleep(SETTLE).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 2, "amend produces a new SHA -> new impl row");
}

/// HEAD bounces away from `latest` and back: the second movement must
/// be recorded as a `head_reset_to_known_sha` audit event, not a new
/// impl row.
#[tokio::test]
async fn reset_to_latest_impl_sha_records_audit_event_only() {
    let app = TestApp::spawn().await;
    let plan_id = register(&app, "s").await;

    let sha_a = make_commit(&app.repo, "a.txt", "a\n");
    tokio::time::sleep(SETTLE).await;
    let sha_b = make_commit(&app.repo, "b.txt", "b\n");
    tokio::time::sleep(SETTLE).await;

    common::run_git(&app.repo, &["reset", "--hard", &sha_a]);
    tokio::time::sleep(SETTLE).await;
    common::run_git(&app.repo, &["reset", "--hard", &sha_b]);
    tokio::time::sleep(SETTLE).await;

    let impl_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(impl_count, 2, "no new impl rows from reset to known SHAs");

    let resets: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE plan_id = ? AND kind = 'head_reset_to_known_sha'",
    )
    .bind(plan_id)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert!(
        resets >= 2,
        "at least two head_reset_to_known_sha events (got {resets})"
    );

    let r = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(
        r["review_target"]["id"].as_str().unwrap(),
        sha_b,
        "active_target should HEAD-derive back to sha_b"
    );
}

/// Reset to an older known SHA: no new impl row, audit-only event,
/// and `get_context.active_target` follows HEAD back to the older SHA.
#[tokio::test]
async fn reset_to_older_impl_sha_records_audit_event_and_active_target_moves() {
    let app = TestApp::spawn().await;
    let plan_id = register(&app, "s").await;

    let sha_a = make_commit(&app.repo, "a.txt", "a\n");
    tokio::time::sleep(SETTLE).await;
    let _sha_b = make_commit(&app.repo, "b.txt", "b\n");
    tokio::time::sleep(SETTLE).await;

    common::run_git(&app.repo, &["reset", "--hard", &sha_a]);
    tokio::time::sleep(SETTLE).await;

    let impl_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE plan_id = ?")
            .bind(plan_id)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(impl_count, 2, "reset to older known SHA must not INSERT");

    let resets: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE plan_id = ? AND kind = 'head_reset_to_known_sha' AND target_id = ?",
    )
    .bind(plan_id)
    .bind(&sha_a)
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert!(resets >= 1, "expected audit event for reset to sha_a");

    let r = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(
        r["review_target"]["id"].as_str().unwrap(),
        sha_a,
        "active_target follows HEAD back to older SHA"
    );
}

/// HEAD at an unobserved SHA: `active_target` falls back to the
/// latest impl row rather than guessing. Setup: observe a commit (so
/// there is a known row), then move HEAD to the repo's initial commit
/// (which predates `register_plan_file` and is therefore NOT in
/// `implementation_revisions`). Disable the watcher's HEAD pickup
/// during the reset by directly poking the DB instead of letting the
/// dispatcher observe — we want to test the read-time HEAD-not-found
/// fallback, not the apply-layer SHA-known event.
#[tokio::test]
async fn head_at_unknown_sha_falls_back_to_latest_impl() {
    let app = TestApp::spawn().await;
    let _plan_id = register(&app, "s").await;

    let sha_known = make_commit(&app.repo, "a.txt", "a\n");
    tokio::time::sleep(SETTLE).await;

    // Confirm sha_known is in the impl table.
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions WHERE commit_sha = ?")
            .bind(&sha_known)
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(exists, 1);

    // Find the initial commit (predates registration). git rev-list
    // --max-parents=0 gives the root.
    let root_sha = common::run_git(&app.repo, &["rev-list", "--max-parents=0", "HEAD"]);
    assert_ne!(root_sha, sha_known);

    // Delete the impl row for sha_known from the DB (so HEAD-match
    // would fail) and the events row referencing it, then reset HEAD
    // back. The point is to construct a state where:
    //   - implementation_revisions has rows (so fallback has something to return)
    //   - HEAD points at a SHA not in implementation_revisions
    // Direct DB poke is the simplest way; in production this can
    // happen e.g. via reflog rewinds we never observed.
    let root_for_observe = root_sha.clone();
    use trinity::lifecycle::{CommitSha, CommitSnapshot, Observation, SessionId};
    let fake = CommitSnapshot {
        sha: CommitSha::from(root_for_observe.clone()),
        parent_sha: None,
        branch: None,
        message: "root".into(),
        diff_stat: String::new(),
        worktree_status: Some("clean".into()),
        is_head: false,
    };
    // Observe so the row exists, then test HEAD that doesn't match
    // either row.
    app.state
        .lifecycle
        .observe(
            &SessionId::from("s"),
            "test",
            Observation::CommitObserved { commit: fake },
        )
        .await
        .unwrap();
    // root_for_observe is now in the table. Move HEAD to a SHA NOT in
    // the table by creating a fresh detached commit and never letting
    // the watcher pick it up. Pause is to drain prior events first.
    tokio::time::sleep(SETTLE).await;
    // Create a stray commit but don't wait for the watcher.
    std::fs::write(app.repo.join("stray.txt"), "stray\n").unwrap();
    common::run_git(&app.repo, &["add", "stray.txt"]);
    let stray_sha = {
        common::run_git(&app.repo, &["commit", "-q", "-m", "stray"]);
        common::run_git(&app.repo, &["rev-parse", "HEAD"])
    };
    // Immediately delete the would-be row (race: the watcher might
    // observe before get_context). Most reliable: do not wait; the
    // dispatcher debounce is ~1500ms.
    sqlx::query("DELETE FROM implementation_revisions WHERE commit_sha = ?")
        .bind(&stray_sha)
        .execute(&app.state.pool)
        .await
        .unwrap();

    let r = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    let target = r["review_target"]["id"].as_str().unwrap();
    let latest_known: String = sqlx::query_scalar(
        "SELECT commit_sha FROM implementation_revisions ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        target, latest_known,
        "HEAD doesn't match any row; active_target falls back to latest impl row"
    );
}

/// After a reset to an older known SHA, the web UI's
/// "watched artifacts" strip and the home page's `current_feedback_files`
/// count must agree with `get_context.active_target`. Locks Codex
/// round-4 P2 #1.
#[tokio::test]
async fn reset_to_older_sha_makes_ui_and_mcp_agree_on_target() {
    let app = TestApp::spawn().await;
    let plan_id = register(&app, "s").await;

    // Need a feedback file ingested against sha_a for the count to be
    // meaningful; we'll then reset to sha_a after committing past it.
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let impl_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("impl");
    let sha_a = make_commit(&app.repo, "a.txt", "a\n");
    tokio::time::sleep(SETTLE).await;
    // Write IMPL feedback (post-commit, impl phase).
    std::fs::write(impl_dir.join("rev-a.md"), "review of sha_a\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    // Confirm impl feedback was ingested against sha_a.
    let ingested_target: String = sqlx::query_scalar(
        "SELECT last_ingested_target_id FROM feedback_files WHERE session_id = 's' AND feedback_kind = 'impl' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(ingested_target, sha_a);

    // Now move past sha_a and reset back.
    let _sha_b = make_commit(&app.repo, "b.txt", "b\n");
    tokio::time::sleep(SETTLE).await;
    common::run_git(&app.repo, &["reset", "--hard", &sha_a]);
    tokio::time::sleep(SETTLE).await;

    // get_context: HEAD-derived target is sha_a.
    let mcp = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(mcp["review_target"]["id"].as_str().unwrap(), sha_a);

    // Web UI's impl table should mark rev-a as `current` because the
    // active impl target HEAD-derives to sha_a, matching the row.
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(
        body.contains("feedback-file-status current"),
        "rev-a impl row should render `current` after reset to its target: body length {}",
        body.len()
    );

    // Home page's "Files" column should also show 1.
    let home = app.get("/").await.text().await.unwrap();
    // The cell is just the integer in a `td.num` — search for the
    // session row and assert "1" appears after the impl-fb cell.
    assert!(
        home.contains(">1<"),
        "home page should report 1 current feedback file: body={home}"
    );

    let _ = plan_id;
}

#[tokio::test]
async fn head_move_without_active_plan_is_dropped() {
    let app = TestApp::spawn().await;
    let _plan_id = register(&app, "s").await;
    app.archive_via_service("s").await;

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM implementation_revisions")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "HEAD movement without active plan must be ignored"
    );
}
