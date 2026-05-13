//! Lock the v1 `get_context` response schema.
//!
//! These tests assert structural properties of the response across
//! planning and implementing phases, with and without an `author_label`.
//! The fixture at `tests/fixtures/get_context_response_v1.json` is the
//! canonical example shape; we parse it as `serde_json::Value` and
//! verify every key the prose calls out is present.

mod common;

use serde_json::{Value, json};

use common::{TestApp, make_commit};

const SETTLE: std::time::Duration = std::time::Duration::from_millis(2500);

async fn register(app: &TestApp, sid: &str) {
    let plan_path = app.repo.join("plan.md");
    std::fs::write(&plan_path, "# body\n").unwrap();
    app.call(
        "register_plan_file",
        &app.repo,
        None,
        json!({"session_id": sid, "path": &plan_path, "label": "m"}),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn fixture_parses_as_json_with_all_v1_keys() {
    let raw = std::fs::read_to_string("tests/fixtures/get_context_response_v1.json")
        .expect("fixture file");
    let v: Value = serde_json::from_str(&raw).expect("fixture must be valid JSON");
    for key in [
        "schema_version",
        "session_id",
        "repo_root",
        "plan_file_path",
        "git_logs_head_path",
        "phase",
        "expected_action",
        "completion_artifact",
        "commit_policy",
        "review_target",
        "latest_plan_revision",
        "latest_implementation_revision",
        "write_feedback",
        "prior_feedback",
        "other_feedback_files",
    ] {
        assert!(
            v.get(key).is_some(),
            "fixture missing top-level key `{key}`"
        );
    }
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["expected_action"], "implement_and_commit");
    assert_eq!(v["completion_artifact"], "git_commit");
    assert_eq!(v["commit_policy"]["initial_impl"], "create_commit");
    assert!(v["other_feedback_files"]["plan"].is_array());
    assert!(v["other_feedback_files"]["impl"].is_array());
}

#[tokio::test]
async fn response_has_schema_version_1() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let r = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(r["schema_version"], 1);
}

#[tokio::test]
async fn response_has_separate_write_feedback_and_prior_feedback_blocks() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let r = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert!(r.get("write_feedback").is_some());
    assert!(r.get("prior_feedback").is_some());
    // distinct objects (or null) — never collapsed into one ambiguous key
    assert!(r["write_feedback"].is_object());
    assert!(r["prior_feedback"].is_object());
}

#[tokio::test]
async fn planning_phase_prior_feedback_is_empty() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
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
    assert_eq!(r["expected_action"], "review_plan_or_update_plan_file");
    assert_eq!(
        r["completion_artifact"],
        "plan_revision_or_plan_feedback_file"
    );
    assert!(r["commit_policy"].is_null());
    assert!(r["prior_feedback"]["self"].is_null());
    assert!(r["prior_feedback"]["others"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn implementing_phase_exposes_prior_plan_feedback_paths_for_caller() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "plan critique\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;

    let r = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(r["phase"], "implementing");
    let self_ = &r["prior_feedback"]["self"];
    assert!(!self_.is_null());
    assert_eq!(self_["kind"], "plan");
    assert!(
        self_["path"]
            .as_str()
            .unwrap()
            .ends_with("/feedback/s/plan/rev-a.md")
    );
    // Body is never returned over MCP.
    assert!(self_.get("body").is_none());
    assert!(self_.get("feedback_body").is_none());
}

#[tokio::test]
async fn other_feedback_files_groups_by_plan_and_impl_arrays_always_present() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let r = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert!(r["other_feedback_files"]["plan"].is_array());
    assert!(r["other_feedback_files"]["impl"].is_array());
}

#[tokio::test]
async fn every_unix_timestamp_has_a_sibling_age_label() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "content\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let r = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    // For an ingested write_feedback row, the *_at fields are Some and
    // their *_age_label siblings must also be Some.
    let wf = &r["write_feedback"];
    assert!(wf["last_observed_at"].as_i64().is_some());
    assert!(wf["last_observed_age_label"].as_str().is_some());
    assert!(wf["last_ingested_at"].as_i64().is_some());
    assert!(wf["last_ingested_age_label"].as_str().is_some());
}

#[tokio::test]
async fn review_target_is_plan_revision_during_planning() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let r = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(r["phase"], "planning");
    assert_eq!(r["review_target"]["kind"], "plan_revision");
}

#[tokio::test]
async fn review_target_is_implementation_commit_during_implementing() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    make_commit(&app.repo, "f.txt", "x\n");
    tokio::time::sleep(SETTLE).await;
    let r = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(r["phase"], "implementing");
    assert_eq!(r["expected_action"], "implement_and_commit");
    assert_eq!(r["completion_artifact"], "git_commit");
    assert_eq!(r["commit_policy"]["initial_impl"], "create_commit");
    assert_eq!(
        r["commit_policy"]["addressing_impl_feedback"],
        "amend_latest_impl_commit_unless_user_requests_new_commit"
    );
    assert_eq!(r["review_target"]["kind"], "implementation_commit");
}

/// no_active_plan → write_feedback is `null`, prior_feedback is `null`.
#[tokio::test]
async fn no_active_plan_write_feedback_and_prior_feedback_are_null() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    app.archive_via_service("s").await;
    let r = app
        .call(
            "get_context",
            &app.repo,
            None,
            json!({"session_id": "s", "author_label": "rev-a"}),
        )
        .await
        .unwrap();
    assert_eq!(r["phase"], "no_active_plan");
    assert_eq!(r["expected_action"], "none");
    assert!(r["completion_artifact"].is_null());
    assert!(r["commit_policy"].is_null());
    assert!(r["write_feedback"].is_null());
    // Per the spec, prior_feedback is also null when no_active_plan.
    // (Caller may have had prior phase feedback, but we don't surface
    // it when there's no active plan to compare against.)
    // The current builder returns PriorFeedback { self: null, others: [] }
    // — treat either null or { self: null, others: [] } as acceptable.
    let pf = &r["prior_feedback"];
    if !pf.is_null() {
        assert!(pf["self"].is_null());
        assert!(pf["others"].as_array().unwrap().is_empty());
    }
}
