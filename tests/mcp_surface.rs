//! Lock the agent-facing MCP surface area. Under the watcher-coordinator
//! model the catalog is exactly three agent-facing tools (plus the
//! `echo_cwd` diagnostic stub). Removed tools must not be in
//! `catalog()` or dispatchable through `dispatch()`.

mod common;

use serde_json::json;

use common::TestApp;

fn catalog_names() -> Vec<String> {
    trinity::tools::catalog()
        .into_iter()
        .map(|d| d.name)
        .collect()
}

#[tokio::test]
async fn catalog_lists_exactly_the_three_normal_tools() {
    let names = catalog_names();
    let normal: Vec<&str> = names
        .iter()
        .filter(|n| *n != "echo_cwd")
        .map(|s| s.as_str())
        .collect();
    assert_eq!(
        normal,
        vec!["list_sessions", "register_plan_file", "get_context"],
        "catalog must contain exactly the three agent-facing tools (plus echo_cwd diagnostic), got {names:?}"
    );
}

#[tokio::test]
async fn put_feedback_is_not_in_catalog() {
    assert!(!catalog_names().iter().any(|n| n == "put_feedback"));
}

#[tokio::test]
async fn register_feedback_file_is_not_in_catalog() {
    assert!(
        !catalog_names()
            .iter()
            .any(|n| n == "register_feedback_file")
    );
}

#[tokio::test]
async fn register_implementation_commit_is_not_in_catalog() {
    assert!(
        !catalog_names()
            .iter()
            .any(|n| n == "register_implementation_commit")
    );
}

#[tokio::test]
async fn join_session_is_not_in_catalog() {
    assert!(!catalog_names().iter().any(|n| n == "join_session"));
}

#[tokio::test]
async fn get_current_feedback_is_not_in_catalog() {
    assert!(!catalog_names().iter().any(|n| n == "get_current_feedback"));
}

#[tokio::test]
async fn get_review_context_is_not_in_catalog() {
    assert!(!catalog_names().iter().any(|n| n == "get_review_context"));
}

/// `/sessions/{id}/register-head` was the curator-route fallback for
/// the removed `register_implementation_commit` MCP tool. The
/// watcher-coordinator model observes commits via `.git/logs/HEAD`
/// only — no UI escape hatch.
#[tokio::test]
async fn register_head_http_route_is_404() {
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
    let resp = app.post_form("/sessions/s/register-head", "").await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

/// The session detail page must not advertise `register_implementation_commit`
/// or the deleted "Start implementation review from current HEAD" form.
#[tokio::test]
async fn session_detail_does_not_mention_removed_register_impl_workflow() {
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
    let body = app.get("/sessions/s").await.text().await.unwrap();
    for banned in [
        "register_implementation_commit",
        "register-head",
        "Start implementation review from current HEAD",
    ] {
        assert!(
            !body.contains(banned),
            "session detail still mentions removed `{banned}`"
        );
    }
}

#[tokio::test]
async fn removed_tools_404_through_dispatch() {
    let app = TestApp::spawn().await;
    for tool in [
        "put_feedback",
        "register_feedback_file",
        "register_implementation_commit",
        "join_session",
        "get_current_feedback",
        "get_review_context",
    ] {
        let (status, body) = app
            .call(tool, &app.repo, None, json!({}))
            .await
            .expect_err();
        assert_eq!(
            status,
            reqwest::StatusCode::NOT_FOUND,
            "{tool}: body={body}"
        );
    }
}

#[tokio::test]
async fn get_context_returns_active_target_and_feedback_status() {
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

    let r = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(r["phase"], "planning");
    assert_eq!(r["review_target"]["kind"], "plan_revision");
    assert!(r["latest_plan_revision"]["id"].is_i64());
    assert_eq!(r["schema_version"], 1);
    let wf = &r["write_feedback"];
    assert_eq!(wf["kind"], "plan");
    assert_eq!(wf["status"], "not_yet_ingested");
    assert!(
        wf["path"]
            .as_str()
            .unwrap()
            .ends_with("/feedback/s/plan/rev-a.md"),
        "got {wf:?}"
    );
    // prior_feedback during planning has null self + empty others.
    let prior = &r["prior_feedback"];
    assert!(prior["self"].is_null());
    assert!(prior["others"].as_array().unwrap().is_empty());
    // other_feedback_files: both arrays always present.
    assert!(r["other_feedback_files"]["plan"].is_array());
    assert!(r["other_feedback_files"]["impl"].is_array());
}

#[tokio::test]
async fn get_context_does_not_bump_sessions_updated_at() {
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
    let before: i64 = sqlx::query_scalar("SELECT updated_at FROM sessions WHERE id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    for _ in 0..3 {
        app.call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    }
    let after: i64 = sqlx::query_scalar("SELECT updated_at FROM sessions WHERE id = 's'")
        .fetch_one(&app.state.pool)
        .await
        .unwrap();
    assert_eq!(before, after);
}

/// Recursive JSON key walk: `get_context` must never carry artifact
/// content. Structural — substring grep would false-match
/// `worktree_status_hash` against the disallowed `worktree_status`.
#[tokio::test]
async fn get_context_response_carries_no_artifact_content() {
    let app = TestApp::spawn().await;
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# initial body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": "s", "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();

    // Commit something so phase flips to implementing.
    std::fs::write(app.repo.join("a.txt"), "dirty\n").unwrap();
    common::run_git(&app.repo, &["add", "a.txt"]);
    common::run_git(&app.repo, &["commit", "-q", "-m", "impl"]);
    // Leave a dirty worktree on top so worktree_dirty == true.
    std::fs::write(app.repo.join("b.txt"), "uncommitted\n").unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;

    let resp = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(resp["phase"], "implementing");

    let banned: &[&str] = &[
        "body",
        "diff_text",
        "diff_stat",
        "commit_message",
        "feedback_body",
        "worktree_status",
    ];
    let allowed: &[&str] = &["worktree_status_hash", "worktree_dirty"];

    let mut seen_allowed = std::collections::HashSet::new();
    walk_keys(&resp, &mut |key| {
        for b in banned {
            assert_ne!(
                key, *b,
                "banned key `{b}` appeared in get_context response: {resp}"
            );
        }
        if allowed.contains(&key) {
            seen_allowed.insert(key.to_string());
        }
    });
    assert!(
        seen_allowed.contains("worktree_dirty"),
        "expected worktree_dirty in response"
    );
    assert!(
        seen_allowed.contains("worktree_status_hash"),
        "expected worktree_status_hash in response"
    );
}

fn walk_keys<F: FnMut(&str)>(v: &serde_json::Value, visit: &mut F) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, vv) in map {
                visit(k.as_str());
                walk_keys(vv, visit);
            }
        }
        serde_json::Value::Array(arr) => {
            for vv in arr {
                walk_keys(vv, visit);
            }
        }
        _ => {}
    }
}
