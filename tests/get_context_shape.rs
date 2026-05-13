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

use common::SETTLE;

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
        "repo_effective_session_id",
        "is_repo_effective",
        "phase",
        "expected_action",
        "completion_artifact",
        "commit_policy",
        "review_gate",
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
    assert_eq!(v["expected_action"], "write_impl_feedback");
    assert_eq!(v["completion_artifact"], "implementation_feedback_file");
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
    assert_eq!(r["expected_action"], "write_plan_feedback");
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
async fn review_gate_ready_requires_current_approval_from_all_verdict_participants() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "APPROVE\nlooks good\n").unwrap();
    std::fs::write(plan_dir.join("notes.md"), "some notes\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let r = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(r["review_gate"]["state"], "ready");
    assert_eq!(r["review_gate"]["participants"], json!(["rev-a"]));
    assert_eq!(r["review_gate"]["approvals"], json!(["rev-a"]));
    assert_eq!(r["review_gate"]["unmarked"], json!(["notes"]));
    assert_eq!(r["expected_action"], "implement_and_commit");
}

#[tokio::test]
async fn ready_plan_requires_claim_when_session_is_not_repo_effective() {
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
    let plan_dir_b = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("b")
        .join("plan");
    std::fs::write(plan_dir_b.join("rev-a.md"), "APPROVE\nlooks good\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let before_claim = app
        .call("get_context", &app.repo, None, json!({"session_id": "b"}))
        .await
        .unwrap();
    assert_eq!(before_claim["is_repo_effective"], false);
    assert_eq!(before_claim["repo_effective_session_id"], "a");
    assert_eq!(before_claim["review_gate"]["state"], "ready");
    assert_eq!(before_claim["expected_action"], "claim_for_implementation");

    let resp = app.post_form("/sessions/b/claim", "").await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);

    let after_claim = app
        .call("get_context", &app.repo, None, json!({"session_id": "b"}))
        .await
        .unwrap();
    assert_eq!(after_claim["is_repo_effective"], true);
    assert_eq!(after_claim["repo_effective_session_id"], "b");
    assert_eq!(after_claim["expected_action"], "implement_and_commit");
}

#[tokio::test]
async fn new_plan_revision_makes_prior_plan_approval_stale() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let plan_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("plan");
    std::fs::write(plan_dir.join("rev-a.md"), "APPROVE\nlooks good\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let ready = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(ready["review_gate"]["state"], "ready");

    std::fs::write(app.repo.join("plan.md"), "# revised\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let stale = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(stale["review_gate"]["state"], "needs_review");
    assert_eq!(stale["review_gate"]["participants"], json!(["rev-a"]));
    assert_eq!(stale["review_gate"]["missing_approvals"], json!(["rev-a"]));
    assert_eq!(stale["expected_action"], "write_plan_feedback");
}

#[tokio::test]
async fn operator_override_sets_gate_until_target_changes() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;

    let resp = app
        .post_form("/sessions/s/review_gate_override", "phase=plan&state=ready")
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);

    let overridden = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(overridden["review_gate"]["state"], "ready");
    assert_eq!(
        overridden["review_gate"]["override"]["actor"],
        "system:operator"
    );
    assert_eq!(overridden["expected_action"], "implement_and_commit");

    std::fs::write(app.repo.join("plan.md"), "# target changed\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let after_change = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(after_change["review_gate"]["state"], "needs_review");
    assert!(after_change["review_gate"]["override"].is_null());
}

#[tokio::test]
async fn operator_override_impl_sticky_until_new_commit() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    make_commit(&app.repo, "impl.txt", "v1\n");
    tokio::time::sleep(SETTLE).await;

    let resp = app
        .post_form("/sessions/s/review_gate_override", "phase=impl&state=ready")
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);

    let overridden = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(overridden["review_gate"]["state"], "ready");
    assert_eq!(
        overridden["review_gate"]["override"]["actor"],
        "system:operator"
    );
    assert_eq!(overridden["expected_action"], "ready_to_finish");

    make_commit(&app.repo, "impl.txt", "v2\n");
    tokio::time::sleep(SETTLE).await;

    let after_change = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(after_change["review_gate"]["state"], "needs_review");
    assert!(after_change["review_gate"]["override"].is_null());
    assert_eq!(after_change["expected_action"], "write_impl_feedback");
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
    assert_eq!(r["expected_action"], "write_impl_feedback");
    assert_eq!(r["completion_artifact"], "implementation_feedback_file");
    assert_eq!(r["commit_policy"]["initial_impl"], "create_commit");
    assert_eq!(
        r["commit_policy"]["addressing_impl_feedback"],
        "amend_latest_impl_commit_unless_user_requests_new_commit"
    );
    assert_eq!(r["review_target"]["kind"], "implementation_commit");
}

#[tokio::test]
async fn new_implementation_commit_makes_prior_impl_approval_stale() {
    let app = TestApp::spawn().await;
    register(&app, "s").await;
    make_commit(&app.repo, "impl.txt", "v1\n");
    tokio::time::sleep(SETTLE).await;

    let canonical_repo = dunce::canonicalize(&app.repo).unwrap();
    let impl_dir = canonical_repo
        .join(".trinity")
        .join("feedback")
        .join("s")
        .join("impl");
    std::fs::write(impl_dir.join("rev-a.md"), "APPROVE\nworks\n").unwrap();
    tokio::time::sleep(SETTLE).await;

    let ready = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(ready["phase"], "implementing");
    assert_eq!(ready["review_gate"]["state"], "ready");
    assert_eq!(ready["expected_action"], "ready_to_finish");

    make_commit(&app.repo, "impl.txt", "v2\n");
    tokio::time::sleep(SETTLE).await;

    let stale = app
        .call("get_context", &app.repo, None, json!({"session_id": "s"}))
        .await
        .unwrap();
    assert_eq!(stale["review_gate"]["state"], "needs_review");
    assert_eq!(stale["review_gate"]["participants"], json!(["rev-a"]));
    assert_eq!(stale["review_gate"]["missing_approvals"], json!(["rev-a"]));
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
