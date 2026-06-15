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

/// A repo with a base commit, a team, and an `origin` bare remote
/// carrying `refs/pull/<pr>/head` so `start_with`'s fetch works.
fn env_with_pr(pr: u32) -> (TestEnv, String) {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &["ruthless"]);
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
