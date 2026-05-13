//! Integration tests for the flat newest-first session timeline.
//! Locks the new row anatomy (kind-class on `<article class="entry …">`,
//! action pills with the right labels, amend-aware diff links).

mod common;

use serde_json::json;

use common::{TestApp, make_commit};

const SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

async fn register(app: &TestApp, sid: &str) {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": sid, "path": &plan_path, "label": "claude-main"}),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn session_detail_renders_timeline_newest_first() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let body = app.get("/sessions/s").await.text().await.unwrap();
    // Two plan-revision rows, newer one before the older one.
    let idx_v2 = body.find("event-2").expect("event-2 missing");
    let idx_v1 = body.find("event-1").expect("event-1 missing");
    assert!(
        idx_v2 < idx_v1,
        "newest event should render before the older one (v2 idx {idx_v2}, v1 idx {idx_v1})"
    );
}

#[tokio::test]
async fn timeline_includes_plan_revision_and_impl_commit_rows() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains(r#"class="entry plan-rev""#));
    assert!(body.contains("entry impl-commit"));
}

#[tokio::test]
async fn home_page_renders_cross_session_recent_activity() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let body = app.get("/").await.text().await.unwrap();
    assert!(body.contains("Recent activity"));
    assert!(body.contains("home-timeline-feed"));
    assert!(body.contains("View plan revision #1"));
    assert!(body.contains("# v1"));
    assert!(body.contains(r#"class="action-label">View"#));
}

#[tokio::test]
async fn session_detail_renders_active_plan_preview() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("Active plan revision #1"));
    assert!(body.contains("# v1"));
    assert!(body.contains("active-plan-head"));
    assert!(body.contains("paths ▾"));
    assert!(!body.contains("preview-clipped"));
}

#[tokio::test]
async fn long_active_plan_preview_renders_body_once() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    let mut plan_body = String::from("# long plan\n\nunique-long-marker\n\n");
    for n in 0..40 {
        plan_body.push_str(&format!("line {n}\n\n"));
    }
    std::fs::write(&plan_path, plan_body).unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "claude-main"}),
    )
    .await
    .unwrap();

    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert_eq!(body.matches("unique-long-marker").count(), 1);
    assert!(body.contains(r#"class="active-plan""#));
    assert!(!body.contains("preview-clipped"));
    assert!(!body.contains("active-plan-full"));
}

#[tokio::test]
async fn plan_revision_row_has_view_and_diff_actions_for_rev_2_plus() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("View plan revision #2"));
    assert!(body.contains("Diff to #1"));
}

#[tokio::test]
async fn plan_revision_row_for_rev_1_has_only_view_action() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("View plan revision #1"));
    assert!(!body.contains("Diff to #0"));
}

#[tokio::test]
async fn impl_commit_row_for_normal_commit_renders_diff_parent_commit_action() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("Diff parent..commit"));
    assert!(!body.contains("Diff vs previous amend"));
}

#[tokio::test]
async fn impl_commit_row_for_amend_renders_vs_previous_amend_and_full_diff_since_parent_actions() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;
    common::run_git(&app.repo, &["commit", "-q", "--amend", "--no-edit"]);
    tokio::time::sleep(SETTLE).await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("Diff vs previous amend"));
    assert!(body.contains("Full diff since parent"));
}

#[tokio::test]
async fn amend_chain_of_three_commits_renders_chain_aware_actions() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let sha1 = make_commit(&app.repo, "f.txt", "x\n");
    let parent = common::run_git(&app.repo, &["rev-parse", &format!("{sha1}^")]);
    tokio::time::sleep(SETTLE).await;

    std::fs::write(app.repo.join("f.txt"), "y\n").unwrap();
    common::run_git(&app.repo, &["add", "f.txt"]);
    common::run_git(&app.repo, &["commit", "-q", "--amend", "--no-edit"]);
    let sha2 = common::run_git(&app.repo, &["rev-parse", "HEAD"]);
    tokio::time::sleep(SETTLE).await;

    std::fs::write(app.repo.join("f.txt"), "z\n").unwrap();
    common::run_git(&app.repo, &["add", "f.txt"]);
    common::run_git(&app.repo, &["commit", "-q", "--amend", "--no-edit"]);
    tokio::time::sleep(SETTLE).await;

    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(
        body.contains(&format!("?vs={sha2}")),
        "third amend should diff against the previous amend commit {sha2}; body:\n{body}"
    );
    assert!(
        body.contains(&format!("?vs={parent}")),
        "full amend diff should use the original parent {parent}; body:\n{body}"
    );
}

#[tokio::test]
async fn feedback_row_links_to_target_artifact_anchor_not_to_file_path() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    // Drop a plan-feedback file.
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "consider X\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    // Action is "Open in plan revision #1" with a #feedback-<id> anchor.
    assert!(body.contains("Open in plan revision #1"));
    assert!(body.contains("#feedback-"));
    // Does NOT use a raw .md file link as the primary action.
    assert!(!body.contains(".trinity/feedback/s/plan/rev-a.md"));
}

#[tokio::test]
async fn archived_plan_feedback_row_links_to_original_revision_anchor() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let old_rev_id: i64 =
        sqlx::query_scalar("SELECT id FROM plan_revisions WHERE revision_number = 1")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();

    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "old plan feedback\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let feedback_id: i64 = sqlx::query_scalar(
        "SELECT id FROM feedback WHERE session_id = 's' AND author_label = 'rev-a'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();

    app.archive_via_service("s").await;
    std::fs::write(app.repo.join("plan.md"), "# new plan\n").unwrap();
    register(&app, "s").await;

    let body = app.get("/sessions/s").await.text().await.unwrap();
    let href = format!(r#"/sessions/s/plan_revisions/{old_rev_id}#feedback-{feedback_id}"#);
    assert!(
        body.contains(&href),
        "archived-plan feedback events should keep linking to their original revision anchor; body:\n{body}"
    );
}

#[tokio::test]
async fn timeline_html_uses_kind_accent_class_per_event_type() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    // state_transition planning → implementing fires when the first commit
    // lands.
    assert!(body.contains(r#"class="entry state""#));
    assert!(body.contains(r#"class="entry plan-rev""#));
    assert!(body.contains("entry impl-commit"));
}

#[tokio::test]
async fn stylesheet_defines_timeline_animation_rules() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("timeline-enter"));
    assert!(body.contains("timeline-highlight"));
    assert!(body.contains("prefers-reduced-motion"));
    assert!(body.contains(".home-activity .timeline-wrap"));
    assert!(body.contains("max-height: 70vh"));
    assert!(body.contains("function ping()"));
    assert!(body.contains("data-sound-test"));
    assert!(
        !body.contains("trinity.timeline.sound") && !body.contains("data-sound-toggle"),
        "timeline notification sound should not expose an app-level mute preference"
    );
}
