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
    assert!(body.contains(r#"class="entry impl-commit""#));
}

#[tokio::test]
async fn plan_revision_row_has_view_and_diff_actions_for_rev_2_plus() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("View body"));
    assert!(body.contains("Diff to #1"));
}

#[tokio::test]
async fn plan_revision_row_for_rev_1_has_only_view_action() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    assert!(body.contains("View body"));
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
    assert!(body.contains(r#"class="entry impl-commit""#));
}

#[tokio::test]
async fn new_event_row_carries_animation_class_for_live_insertion() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let body = app.get("/sessions/s").await.text().await.unwrap();
    // The initial-render rows reuse the same .entry class which carries
    // the timeline-enter animation; the CSS defines it.
    assert!(body.contains("timeline-enter"));
    assert!(body.contains("timeline-highlight"));
    assert!(body.contains("prefers-reduced-motion"));
}
