//! Integration tests for `clank auto on|off|status`.
//!
//! Establishes a session binding via `clank as` first (since
//! `clank auto` is NOT a bootstrap path), then exercises the
//! auto-mode + role transitions.

use std::path::Path;
use std::process::Command;

fn clank_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clank")
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@test"]);
    git(path, &["config", "user.name", "test"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    dir
}

const CLAUDE_SESSION: &str = "742f6a04-f174-409a-ab01-419a16c5f372";

/// Run a clank subcommand with the given env. Always passes
/// `--repo <path>` so cwd doesn't matter; always clears inherited
/// session env so tests are deterministic.
fn run_clank(repo: &Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.args(args)
        .arg("--repo")
        .arg(repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn clank")
}

fn bind_alice(repo: &Path) {
    let out = run_clank(
        repo,
        &["as", "alice"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(
        out.status.success(),
        "clank as alice failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn auto_on_writes_hint_by_default() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    let out = run_clank(
        repo,
        &["auto", "on"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/alice/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["auto_mode"], "hint");
}

#[test]
fn auto_on_mode_wait_writes_wait() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    let out = run_clank(
        repo,
        &["auto", "on", "--mode", "wait"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/alice/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["auto_mode"], "wait");
}

#[test]
fn auto_off_writes_off() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);
    // First turn it on, then off.
    run_clank(
        repo,
        &["auto", "on"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    let out = run_clank(
        repo,
        &["auto", "off"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/alice/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["auto_mode"], "off");
}

#[test]
fn auto_on_role_master_writes_repo_config() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    let out = run_clank(
        repo,
        &["auto", "on", "--role", "master"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(repo.join(".clank/config.json")).unwrap())
            .unwrap();
    assert_eq!(cfg["master"], "alice");
}

#[test]
fn auto_on_role_reviewers_clears_master_when_self() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    // Claim master first.
    run_clank(
        repo,
        &["auto", "on", "--role", "master"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    // Now downgrade to reviewer.
    let out = run_clank(
        repo,
        &["auto", "on", "--role", "reviewers"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(repo.join(".clank/config.json")).unwrap())
            .unwrap();
    assert!(
        cfg.get("master").is_none() || cfg["master"].is_null(),
        "expected master cleared, got {cfg}"
    );
}

#[test]
fn auto_on_role_reviewers_leaves_other_master_alone() {
    let dir = init_repo();
    let repo = dir.path();
    // Seed repo config with bob as master (manually — bob isn't
    // bound to a session here, just designated).
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(repo.join(".clank/config.json"), r#"{"master":"bob"}"#).unwrap();
    bind_alice(repo);

    let out = run_clank(
        repo,
        &["auto", "on", "--role", "reviewers"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(repo.join(".clank/config.json")).unwrap())
            .unwrap();
    // bob should still be master — alice asking to be reviewers
    // doesn't disturb a different agent's master designation.
    assert_eq!(cfg["master"], "bob");
}

#[test]
fn auto_status_shows_resolved_role() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);
    run_clank(
        repo,
        &["auto", "on", "--role", "master"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );

    let out = run_clank(
        repo,
        &["auto", "status", "--json"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(body["label"], "alice");
    assert_eq!(body["auto_mode"], "hint");
    assert_eq!(body["role"], "master");
}

#[test]
fn auto_errors_when_session_not_bound() {
    let dir = init_repo();
    let repo = dir.path();
    // No `clank as` ran; resolver should refuse.
    let out = run_clank(
        repo,
        &["auto", "on"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no agent is bound") || stderr.contains("clank as"),
        "stderr missing bootstrap hint: {stderr}"
    );
}
