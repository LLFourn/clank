//! In-process tests for `clank pr-review` phase 2 (local state +
//! verbs). Drive the cores directly — no binary spawning; git is
//! spawned for the PR-head fetch (allowed).

mod common;

use common::TestEnv;
use std::path::Path;
use std::process::Command;

use clank::cli::pr_review::{abort_with, note_with, start_with, status_with};
use clank_core::ids::AgentLabel;
use clank_core::pr_review::ReviewerVerdict;
use clank_core::vocab::Verdict;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn label(s: &str) -> AgentLabel {
    AgentLabel::parse(s).unwrap()
}

/// Base commit + an `origin` bare remote carrying
/// `refs/pull/<pr>/head` so `start_with`'s fetch works. No team —
/// callers add one when they need it.
fn setup_repo_with_pr(env: &TestEnv, pr: u32) -> String {
    let repo = env.repo();
    std::fs::write(repo.join("file.rs"), "// base\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "base"]);

    let bare = env.home().join("origin.git");
    git(repo, &["init", "--bare", "--quiet", bare.to_str().unwrap()]);
    git(repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(repo, &["checkout", "--quiet", "-b", "pr-branch"]);
    std::fs::write(repo.join("file.rs"), "// pr change\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "pr change"]);
    let pr_sha = git_out(repo, &["rev-parse", "HEAD"]);
    git(
        repo,
        &[
            "push",
            "--quiet",
            "origin",
            &format!("HEAD:refs/pull/{pr}/head"),
        ],
    );
    git(repo, &["checkout", "--quiet", "-"]);
    git(repo, &["branch", "--quiet", "-D", "pr-branch"]);
    pr_sha
}

/// A repo with a base commit, a team, and an `origin` bare remote
/// carrying `refs/pull/<pr>/head` so `start_with`'s fetch works.
fn env_with_pr(pr: u32) -> (TestEnv, String) {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &["ruthless"]);
    let pr_sha = setup_repo_with_pr(&env, pr);
    (env, pr_sha)
}

#[test]
fn start_scaffolds_and_pins_head() {
    let (env, pr_sha) = env_with_pr(123);
    let repo = env.repo();
    let dir = start_with(repo, "LLFourn/clank", 123, None).unwrap();

    assert!(dir.ends_with(".clank/pr-reviews/123"));
    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("pr.json")).unwrap()).unwrap();
    assert_eq!(state["repo"], "LLFourn/clank");
    assert_eq!(state["number"], 123);
    assert_eq!(state["head_sha"], pr_sha, "head pinned to the PR head");
    assert_eq!(state["round"], 0);
    assert!(state["review_id"].is_null(), "no pending review yet");
    assert!(dir.join("master.md").is_file());
    assert!(dir.join("reviews").is_dir());

    // gitignore entry ensured.
    let gi = std::fs::read_to_string(repo.join(".clank/.gitignore")).unwrap();
    assert!(gi.lines().any(|l| l == "/pr-reviews/"), "gitignore: {gi}");
}

#[test]
fn start_refuses_existing() {
    let (env, _) = env_with_pr(7);
    let repo = env.repo();
    start_with(repo, "o/r", 7, None).unwrap();
    let err = start_with(repo, "o/r", 7, None).unwrap_err().to_string();
    assert!(err.contains("already started"), "got: {err}");
}

#[test]
fn note_records_verdict_at_current_round() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None).unwrap();

    let path = note_with(
        repo,
        &label("codex"),
        Some(123),
        Verdict::Finished,
        "lgtm, ship it",
    )
    .unwrap();
    assert!(path.ends_with("reviews/codex.md"));
    let parsed = ReviewerVerdict::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(parsed.verdict, Verdict::Finished);
    assert_eq!(parsed.reviewed_round, 0, "stamped with the current round");
    assert_eq!(parsed.summary, "lgtm, ship it");
}

#[test]
fn note_infers_single_active_pr() {
    let (env, _) = env_with_pr(42);
    let repo = env.repo();
    start_with(repo, "o/r", 42, None).unwrap();
    // No --pr: resolves to the single active review.
    let path = note_with(
        repo,
        &label("codex"),
        None,
        Verdict::RequestChanges,
        "needs work",
    )
    .unwrap();
    assert!(path.ends_with("reviews/codex.md"));
}

#[test]
fn status_reports_pending_then_converged() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None).unwrap();

    let s = status_with(repo, Some(env.home()), Some(123)).unwrap();
    assert!(s.contains("round 0"), "{s}");
    assert!(s.contains("waiting on:"), "{s}");
    assert!(s.contains("codex") && s.contains("ruthless"), "{s}");

    note_with(repo, &label("codex"), Some(123), Verdict::Finished, "ok").unwrap();
    note_with(repo, &label("ruthless"), Some(123), Verdict::Finished, "ok").unwrap();
    let s = status_with(repo, Some(env.home()), Some(123)).unwrap();
    assert!(s.contains("converged"), "{s}");
}

#[test]
fn abort_is_master_only() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None).unwrap();

    // A reviewer cannot abort.
    let err = abort_with(repo, Some(env.home()), &label("codex"), Some(123))
        .unwrap_err()
        .to_string();
    assert!(err.contains("master-only"), "got: {err}");
    assert!(
        repo.join(".clank/pr-reviews/123").exists(),
        "not deleted by reviewer"
    );

    // Master can.
    abort_with(repo, Some(env.home()), &label("claude"), Some(123)).unwrap();
    assert!(
        !repo.join(".clank/pr-reviews/123").exists(),
        "master abort removed it"
    );
}

#[test]
fn note_resolution_errors_when_no_active_review() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let err = note_with(env.repo(), &label("codex"), None, Verdict::Finished, "x")
        .unwrap_err()
        .to_string();
    assert!(err.contains("no active PR review"), "got: {err}");
}

#[test]
fn pr_review_inputs_project_current_round_verdicts() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None).unwrap();

    // Fresh review: one input, round 0, no current verdicts.
    let inputs = clank::cli::pr_review::pr_review_inputs(repo);
    assert_eq!(inputs.len(), 1);
    assert_eq!((inputs[0].pr, inputs[0].round), (123, 0));
    assert!(inputs[0].current_verdicts.is_empty());

    // codex notes FINISHED at round 0 → it shows as a current verdict.
    note_with(repo, &label("codex"), Some(123), Verdict::Finished, "ok").unwrap();
    let inputs = clank::cli::pr_review::pr_review_inputs(repo);
    assert_eq!(inputs[0].current_verdicts.len(), 1);
    assert_eq!(inputs[0].current_verdicts[0].author, label("codex"));
    assert_eq!(inputs[0].current_verdicts[0].verdict, Verdict::Finished);
}

#[test]
fn pr_review_inputs_drop_stale_round_verdicts() {
    let (env, _) = env_with_pr(123);
    let repo = env.repo();
    start_with(repo, "o/r", 123, None).unwrap();
    note_with(repo, &label("codex"), Some(123), Verdict::Finished, "ok").unwrap();

    // Master revises → bump pr.json round to 1 (the round bump that
    // `propose` will do in phase 4; simulated here by editing state).
    let pr_json = repo.join(".clank/pr-reviews/123/pr.json");
    let mut state: clank_core::pr_review::PrReviewState =
        serde_json::from_str(&std::fs::read_to_string(&pr_json).unwrap()).unwrap();
    state.round = 1;
    std::fs::write(&pr_json, serde_json::to_string_pretty(&state).unwrap()).unwrap();

    // codex's round-0 verdict is now STALE → not a current verdict.
    let inputs = clank::cli::pr_review::pr_review_inputs(repo);
    assert_eq!(inputs[0].round, 1);
    assert!(
        inputs[0].current_verdicts.is_empty(),
        "stale round-0 verdict must not count for round 1"
    );
}

#[test]
fn status_fails_closed_without_a_team() {
    // codex da7ab89: a missing/misconfigured team must NOT make
    // status print "converged" off an empty reviewer set. Start a
    // review but register no team → status errors, never "done".
    let env = TestEnv::init();
    setup_repo_with_pr(&env, 123);
    start_with(env.repo(), "o/r", 123, None).unwrap();
    let res = status_with(env.repo(), Some(env.home()), Some(123));
    let err = res.expect_err("status must fail closed without a team");
    assert!(
        !err.to_string().contains("converged"),
        "must not fabricate convergence: {err}"
    );
}
