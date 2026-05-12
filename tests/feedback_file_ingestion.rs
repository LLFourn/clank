//! Feedback file ingest path: per-author markdown files dropped under
//! `<repo_root>/.trinity/feedback/<session_id>/` are auto-discovered
//! and ingested through `SessionService::put_feedback`. The sidecar
//! `feedback_files` row records observation/ingest state; status is
//! derived from columns (current / stale / missing / parse_error /
//! not_yet_ingested).

mod common;

use std::path::PathBuf;

use serde_json::json;

use common::{TestApp, make_commit};

const SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

async fn register(app: &TestApp, sid: &str) -> (i64, i64) {
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
    (
        r["plan_id"].as_i64().unwrap(),
        r["revision_id"].as_i64().unwrap(),
    )
}

fn plan_feedback_dir(app: &TestApp, sid: &str) -> PathBuf {
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    canonical_repo
        .join(".trinity")
        .join("feedback")
        .join(sid)
        .join("plan")
}

fn impl_feedback_dir(app: &TestApp, sid: &str) -> PathBuf {
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    canonical_repo
        .join(".trinity")
        .join("feedback")
        .join(sid)
        .join("impl")
}

#[tokio::test]
async fn dropping_a_new_md_in_feedback_dir_auto_creates_sidecar_and_ingests() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");

    std::fs::write(dir.join("rev-a.md"), "first feedback body\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback_files WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(count, 1);

    let body: String = sqlx::query_scalar(
        "SELECT body FROM feedback WHERE session_id = 's' AND author_label = 'rev-a' AND target_kind = 'plan_revision' AND target_id = ?",
    )
    .bind(rev_id.to_string())
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(body, "first feedback body\n");
}

#[tokio::test]
async fn subsequent_writes_update_same_feedback_row() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");

    std::fs::write(dir.join("rev-a.md"), "v1\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let id_v1: i64 = sqlx::query_scalar(
        "SELECT id FROM feedback WHERE session_id = 's' AND author_label = 'rev-a' AND target_id = ?",
    )
    .bind(rev_id.to_string())
    .fetch_one(&app.state.pool)
    .await
    .unwrap();

    std::fs::write(dir.join("rev-a.md"), "v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let id_v2: i64 = sqlx::query_scalar(
        "SELECT id FROM feedback WHERE session_id = 's' AND author_label = 'rev-a' AND target_id = ?",
    )
    .bind(rev_id.to_string())
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(id_v1, id_v2, "same row, body updated in place");

    let body: String = sqlx::query_scalar("SELECT body FROM feedback WHERE id = ?")
        .bind(id_v1)
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(body, "v2\n");
}

#[tokio::test]
async fn debounce_collapses_partial_writes() {
    let app = TestApp::spawn().await;
    let (_, _) = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");
    let p = dir.join("rev-a.md");

    for line in ["partial1\n", "partial2\n", "partial3\n", "final\n"] {
        std::fs::write(&p, line).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    tokio::time::sleep(SETTLE).await;

    let audit_writes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM events WHERE session_id = 's' AND kind = 'feedback_added' \
         AND target_kind = 'plan_revision'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        audit_writes, 1,
        "debounce should collapse rapid writes to one insert audit event"
    );

    let body: String = sqlx::query_scalar(
        "SELECT body FROM feedback WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(body, "final\n");
}

#[tokio::test]
async fn file_delete_marks_missing_but_preserves_feedback_row() {
    let app = TestApp::spawn().await;
    let (_, _) = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");
    let p = dir.join("rev-a.md");

    std::fs::write(&p, "before deletion\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    std::fs::remove_file(&p).unwrap();
    tokio::time::sleep(SETTLE).await;

    let observed_hash: Option<String> = sqlx::query_scalar(
        "SELECT last_observed_hash FROM feedback_files WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert!(
        observed_hash.is_none(),
        "deletion clears last_observed_hash"
    );

    let feedback_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feedback WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        feedback_count, 1,
        "deletion must NOT delete the historical feedback row"
    );
}

/// Same author can hold both a `plan/` and an `impl/` row at once;
/// each is keyed independently in `feedback_files`.
#[tokio::test]
async fn same_author_has_independent_plan_and_impl_feedback_rows() {
    let app = TestApp::spawn().await;
    let (_, _rev_id) = register(&app, "s").await;
    let plan_dir = plan_feedback_dir(&app, "s");
    let impl_dir = impl_feedback_dir(&app, "s");

    // Write plan-phase critique.
    std::fs::write(plan_dir.join("rev-a.md"), "plan critique\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    // Trigger planning → implementing.
    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    // Write impl-phase critique.
    std::fs::write(impl_dir.join("rev-a.md"), "impl critique\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT feedback_kind, author_label FROM feedback_files \
         WHERE session_id = 's' AND author_label = 'rev-a' ORDER BY feedback_kind",
    )
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("impl".to_string(), "rev-a".to_string()),
            ("plan".to_string(), "rev-a".to_string()),
        ]
    );

    let feedback_targets: Vec<(String, String)> = sqlx::query_as(
        "SELECT target_kind, body FROM feedback \
         WHERE session_id = 's' AND author_label = 'rev-a' ORDER BY target_kind",
    )
    .fetch_all(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(feedback_targets.len(), 2);
    assert!(
        feedback_targets
            .iter()
            .any(|(k, b)| k == "implementation_commit" && b == "impl critique\n")
    );
    assert!(
        feedback_targets
            .iter()
            .any(|(k, b)| k == "plan_revision" && b == "plan critique\n")
    );
}

/// Writes to `impl/` during planning are rejected with a specific
/// `parse_error` sentinel; the file stays on disk but no feedback row
/// is created.
#[tokio::test]
async fn impl_dir_write_during_planning_records_parse_error_no_active_impl_target() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = impl_feedback_dir(&app, "s");

    std::fs::write(dir.join("rev-a.md"), "impl critique during planning\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let parse_error: Option<String> = sqlx::query_scalar(
        "SELECT parse_error FROM feedback_files WHERE session_id = 's' AND feedback_kind = 'impl' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(
        parse_error.as_deref(),
        Some("no_active_impl_target_for_kind")
    );

    let feedback_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(feedback_count, 0);
}

/// `plan/` writes are still accepted in `implementing` phase and
/// continue to target the latest plan_revision (cross-phase plan
/// critique stays editable).
#[tokio::test]
async fn plan_dir_write_during_implementing_still_ingests_against_plan_revision() {
    let app = TestApp::spawn().await;
    let (_, rev_id) = register(&app, "s").await;
    let plan_dir = plan_feedback_dir(&app, "s");

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    std::fs::write(plan_dir.join("rev-a.md"), "plan critique\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let (target_kind, target_id): (String, String) = sqlx::query_as(
        "SELECT target_kind, target_id FROM feedback WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(target_kind, "plan_revision");
    assert_eq!(target_id, rev_id.to_string());
}

#[tokio::test]
async fn parse_error_no_active_plan() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");

    app.archive_via_service("s").await;

    std::fs::write(dir.join("rev-a.md"), "stale review\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let parse_error: Option<String> = sqlx::query_scalar(
        "SELECT parse_error FROM feedback_files WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(parse_error.as_deref(), Some("no_active_plan"));

    let feedback_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(feedback_count, 0);
}

#[tokio::test]
async fn parse_error_empty_body() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");

    std::fs::write(dir.join("rev-a.md"), "   \n\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let parse_error: Option<String> = sqlx::query_scalar(
        "SELECT parse_error FROM feedback_files WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(parse_error.as_deref(), Some("empty_body"));

    let feedback_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(feedback_count, 0);
}

#[tokio::test]
async fn filename_failing_slug_validation_is_ignored() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");

    std::fs::write(dir.join("has space.md"), "ignored\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let sidecar_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback_files WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(sidecar_count, 0);
    let feedback_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(feedback_count, 0);
}

#[tokio::test]
async fn subdirectory_under_feedback_dir_is_ignored() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");
    let sub = dir.join("subdir");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join("rev-a.md"), "hidden\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback_files WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn non_md_file_under_feedback_dir_is_ignored() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");

    std::fs::write(dir.join("rev-a.txt"), "wrong extension\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback_files WHERE session_id = 's'")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

/// Delete-after-parse-error: the deletion must mark the sidecar
/// `missing`, not leave it stuck at `parse_error` from the now-gone
/// body.
#[tokio::test]
async fn deleting_a_parse_error_file_marks_it_missing_not_parse_error() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");
    let p = dir.join("rev-a.md");

    // Empty body → parse_error="empty_body".
    std::fs::write(&p, "   \n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let parse_error: Option<String> = sqlx::query_scalar(
        "SELECT parse_error FROM feedback_files WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(parse_error.as_deref(), Some("empty_body"));

    // Delete the file.
    std::fs::remove_file(&p).unwrap();
    tokio::time::sleep(SETTLE).await;

    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT parse_error, last_observed_hash FROM feedback_files WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    assert_eq!(row.0, None, "parse_error must clear on deletion");
    assert_eq!(row.1, None, "last_observed_hash must clear on deletion");

    let view = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(view["write_feedback"]["status"], "missing");
}

/// File deleted out-of-band (before watcher processes the delete):
/// `get_context.status` is `missing` at read time even though the
/// sidecar still carries `last_observed_hash`. The derived status
/// helper is the source of truth, not the sidecar columns alone.
#[tokio::test]
async fn status_is_missing_when_file_deleted_before_watcher_settles() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");
    let p = dir.join("rev-a.md");

    std::fs::write(&p, "v1\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    // Confirm the row is current.
    let r1 = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(r1["write_feedback"]["status"], "current");

    // Delete and immediately read — do NOT wait for the watcher debounce.
    std::fs::remove_file(&p).unwrap();
    let r2 = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    // Sidecar's last_observed_hash is still set (watcher hasn't fired),
    // but the file doesn't exist on disk, so the derived status MUST be
    // missing rather than current/stale.
    assert_eq!(r2["write_feedback"]["status"], "missing");
    assert_eq!(r2["write_feedback"]["exists"], false);
}

#[tokio::test]
async fn get_context_reports_feedback_file_status() {
    let app = TestApp::spawn().await;
    let _ = register(&app, "s").await;
    let dir = plan_feedback_dir(&app, "s");

    // before write: not_yet_ingested, file doesn't exist on disk
    let r1 = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(r1["write_feedback"]["status"], "not_yet_ingested");
    assert_eq!(r1["write_feedback"]["exists"], false);

    // after write: current
    std::fs::write(dir.join("rev-a.md"), "the review\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let r2 = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(r2["write_feedback"]["status"], "current");
    assert_eq!(r2["write_feedback"]["exists"], true);

    // Bump the plan revision; the plan-row's last_ingested_target_id
    // no longer matches the new latest plan_revision, so status flips
    // to stale.
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let r3 = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    // Phase stays planning (plan edit doesn't archive); write_feedback
    // is still the plan-kind file, but the latest plan_revision moved
    // forward so the rev-a row goes stale until the file is re-touched.
    assert_eq!(r3["write_feedback"]["status"], "stale");
}
