//! Plan revision view + diff routes.

mod common;

use serde_json::json;

use common::TestApp;

use common::SETTLE;

async fn register(app: &TestApp) -> i64 {
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
    r["revision_id"].as_i64().unwrap()
}

#[tokio::test]
async fn get_plan_revision_renders_body_html() {
    let app = TestApp::spawn().await;
    let rev_id = register(&app).await;
    let resp = app
        .get(&format!("/sessions/s/plan_revisions/{rev_id}"))
        .await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // Markdown body rendered.
    assert!(body.contains("v1"));
    // Page chrome
    assert!(body.contains("Plan revision #1"));
}

#[tokio::test]
async fn get_plan_revision_for_unknown_id_returns_404() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let resp = app.get("/sessions/s/plan_revisions/99999").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn get_plan_revision_diff_renders_unified_diff_against_prev() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let rev2: i64 = sqlx::query_scalar(
        "SELECT id FROM plan_revisions WHERE revision_number = 2 ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let resp = app
        .get(&format!("/sessions/s/plan_revisions/{rev2}/diff"))
        .await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    // The unified diff renders ins/del/ctx line classes.
    assert!(body.contains("diff-line"));
    // The page title carries "Plan revision #2"
    assert!(body.contains("Plan revision #2"));
}

#[tokio::test]
async fn get_plan_revision_diff_for_rev_1_redirects_to_view() {
    let app = TestApp::spawn().await;
    let rev_id = register(&app).await;
    let resp = app
        .get(&format!("/sessions/s/plan_revisions/{rev_id}/diff"))
        .await;
    assert_eq!(resp.status(), 303);
    let location = resp.headers().get("location").unwrap().to_str().unwrap();
    assert_eq!(location, format!("/sessions/s/plan_revisions/{rev_id}"));
}

#[tokio::test]
async fn cross_session_rev_id_returns_404() {
    let app = TestApp::spawn().await;
    let _ = register(&app).await;
    // Make a second session by registering against a different plan.
    let plan2 = app.repo.join("plan2.md");
    std::fs::write(&plan2, "# v\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "t", "path": &plan2, "label": "m"}),
    )
    .await
    .unwrap();
    // session `t`'s rev_id 1 doesn't belong to session `s`.
    let t_rev: i64 = sqlx::query_scalar(
        "SELECT pr.id FROM plan_revisions pr JOIN plans p ON p.id = pr.plan_id WHERE p.session_id = 't'",
    )
    .fetch_one(&app.state.pool)
    .await
    .unwrap();
    let resp = app
        .get(&format!("/sessions/s/plan_revisions/{t_rev}"))
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn plan_revision_page_renders_inline_feedback_for_target_revision() {
    let app = TestApp::spawn().await;
    let rev_id = register(&app).await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "the review\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let resp = app
        .get(&format!("/sessions/s/plan_revisions/{rev_id}"))
        .await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("the review"));
    assert!(body.contains("rev-a"));
    assert!(body.contains(">plan<"));
    assert!(body.contains(">current<"));
    assert!(body.contains(".trinity/feedback/s/plan/rev-a.md"));
}

#[tokio::test]
async fn plan_revision_feedback_body_renders_markdown() {
    let app = TestApp::spawn().await;
    let rev_id = register(&app).await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "**bold feedback**\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let resp = app
        .get(&format!("/sessions/s/plan_revisions/{rev_id}"))
        .await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<strong>bold feedback</strong>"),
        "feedback should render as markdown, not raw preformatted text; body:\n{body}"
    );
}

#[tokio::test]
async fn plan_revision_prev_and_next_links_resolve_to_real_revisions() {
    let app = TestApp::spawn().await;
    let rev1 = register(&app).await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# v2\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    std::fs::write(&plan_path, "# v3\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let rev2: i64 =
        sqlx::query_scalar("SELECT id FROM plan_revisions WHERE revision_number = 2 LIMIT 1")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    let rev3: i64 =
        sqlx::query_scalar("SELECT id FROM plan_revisions WHERE revision_number = 3 LIMIT 1")
            .fetch_one(&app.state.pool)
            .await
            .unwrap();
    let resp = app.get(&format!("/sessions/s/plan_revisions/{rev2}")).await;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains(&format!("/sessions/s/plan_revisions/{rev1}")),
        "prev link must point at real prev rev_id {rev1}; body:\n{body}"
    );
    assert!(
        body.contains(&format!("/sessions/s/plan_revisions/{rev3}")),
        "next link must point at real next rev_id {rev3}; body:\n{body}"
    );
    assert!(
        !body.contains("/plan_revisions/0"),
        "no link should resolve to rev_id 0; body:\n{body}"
    );
    // Follow them — neither should 404.
    let resp_prev = app.get(&format!("/sessions/s/plan_revisions/{rev1}")).await;
    assert_eq!(resp_prev.status(), 200);
    let resp_next = app.get(&format!("/sessions/s/plan_revisions/{rev3}")).await;
    assert_eq!(resp_next.status(), 200);
}

#[tokio::test]
async fn plan_revision_feedback_blocks_carry_stable_feedback_id_anchors() {
    let app = TestApp::spawn().await;
    let rev_id = register(&app).await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "x\n").unwrap();
    tokio::time::sleep(SETTLE).await;
    let fid: i64 = sqlx::query_scalar("SELECT id FROM feedback WHERE author_label = 'rev-a'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    let resp = app
        .get(&format!("/sessions/s/plan_revisions/{rev_id}"))
        .await;
    let body = resp.text().await.unwrap();
    assert!(body.contains(&format!(r#"id="feedback-{fid}""#)));
}
