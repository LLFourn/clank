//! Commit diff page + ?vs= override.

mod common;

use serde_json::json;

use common::{TestApp, make_commit};

const SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

async fn register_then_commit(app: &TestApp) -> String {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v1\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();
    let sha = make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;
    sha
}

#[tokio::test]
async fn commit_diff_default_uses_parent_sha() {
    let app = TestApp::spawn().await;
    let sha = register_then_commit(&app).await;
    let resp = app.get(&format!("/sessions/s/commits/{sha}")).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Sticky header chips up the diff base. Default = parent.
    assert!(body.contains("parent "));
    assert!(body.contains("base-chip") || body.contains("Commit"));
    assert!(body.contains("No reviews yet · 0"));
}

#[tokio::test]
async fn commit_diff_page_renders_structured_file_diff() {
    let app = TestApp::spawn().await;
    let sha = register_then_commit(&app).await;
    let resp = app.get(&format!("/sessions/s/commits/{sha}")).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains(r#"class="file-index""#), "body:\n{body}");
    assert!(body.contains(r#"class="structured-diff""#), "body:\n{body}");
    assert!(body.contains(r#"class="diff-row ins""#), "body:\n{body}");
    assert!(body.contains("f.txt"), "body:\n{body}");
}

#[tokio::test]
async fn commit_diff_with_vs_param_uses_arbitrary_base() {
    let app = TestApp::spawn().await;
    let sha_a = register_then_commit(&app).await;
    // Second commit; we can diff sha_b against sha_a explicitly.
    let sha_b = make_commit(&app.repo, "g.txt", "y\n");
    tokio::time::sleep(SETTLE).await;
    let resp = app
        .get(&format!("/sessions/s/commits/{sha_b}?vs={sha_a}"))
        .await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("base override"));
}

#[tokio::test]
async fn commit_diff_with_invalid_vs_returns_400() {
    let app = TestApp::spawn().await;
    let sha = register_then_commit(&app).await;
    let resp = app
        .get(&format!(
            "/sessions/s/commits/{sha}?vs=deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        ))
        .await;
    assert_eq!(resp.status(), 400);
}

#[tokio::test]
async fn commit_diff_page_renders_inline_feedback_for_target_commit() {
    let app = TestApp::spawn().await;
    let sha = register_then_commit(&app).await;
    // Drop impl-kind feedback at the commit.
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let impl_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("impl");
    std::fs::write(impl_dir.join("rev-a.md"), "looks good but...\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let resp = app.get(&format!("/sessions/s/commits/{sha}")).await;
    let body = resp.text().await.unwrap();
    assert!(body.contains("looks good but..."));
    assert!(body.contains("rev-a"));
    assert!(body.contains(r##"href="#reviews""##));
    assert!(body.contains("Reviews · 1"));
}

#[tokio::test]
async fn commit_diff_feedback_blocks_carry_stable_feedback_id_anchors() {
    let app = TestApp::spawn().await;
    let sha = register_then_commit(&app).await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let impl_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("impl");
    std::fs::write(impl_dir.join("rev-a.md"), "x\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let fid: i64 = sqlx::query_scalar(
        "SELECT id FROM feedback WHERE author_label = 'rev-a' AND target_kind = 'implementation_commit'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let resp = app.get(&format!("/sessions/s/commits/{sha}")).await;
    let body = resp.text().await.unwrap();
    assert!(body.contains(&format!(r#"id="feedback-{fid}""#)));
}
