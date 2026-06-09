//! Integration tests for `clank as <label>`.
//!
//! Spawns the real `clank` binary with CLAUDE_CODE_SESSION_ID set
//! (faking a claude session) and verifies the binding lands at
//! `.clank/agents/<label>/config.json` and clears stale bindings
//! from any other agent.

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

/// Run `clank as` with a controlled env. Always passes
/// `--repo <path>` so cwd doesn't matter.
fn run_as(repo: &Path, label: &str, env: &[(&str, &str)]) -> std::process::Output {
    let home = tempfile::tempdir().expect("isolated test HOME");
    let mut cmd = Command::new(clank_bin());
    cmd.arg("as")
        .arg(label)
        .arg("--repo")
        .arg(repo)
        // Clear inherited session env so the test only sees what
        // we set explicitly.
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .env("HOME", home.path());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn clank as")
}

#[test]
fn binds_session_via_claude_env() {
    let dir = init_repo();
    let repo = dir.path();
    let session = "742f6a04-f174-409a-ab01-419a16c5f372";

    let out = run_as(repo, "alice", &[("CLAUDE_CODE_SESSION_ID", session)]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg_path = repo.join(".clank/agents/alice/config.json");
    let body = std::fs::read_to_string(&cfg_path).expect("file exists");
    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed["session"]["id"], session);
    assert_eq!(parsed["session"]["tool"], "claude");
    assert!(parsed["session"]["updated_at"].is_string());
}

#[test]
fn binds_session_via_codex_env() {
    let dir = init_repo();
    let repo = dir.path();
    let session = "019e5385-ed97-7603-8561-dd9024328ff9";

    let out = run_as(repo, "bob", &[("CODEX_THREAD_ID", session)]);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/bob/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["session"]["tool"], "codex");
}

#[test]
fn rebind_clears_stale_session_from_other_agent() {
    let dir = init_repo();
    let repo = dir.path();
    let session = "742f6a04-f174-409a-ab01-419a16c5f372";
    let env = [("CLAUDE_CODE_SESSION_ID", session)];

    // Bind to alice first.
    let out1 = run_as(repo, "alice", &env);
    assert!(out1.status.success());

    // Now bind THIS SESSION to bob. Alice should lose her session.
    let out2 = run_as(repo, "bob", &env);
    assert!(out2.status.success());
    let stderr = String::from_utf8_lossy(&out2.stderr);
    let stdout = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout.contains("cleared stale binding on `alice`"),
        "expected cleared-binding notice, stdout={stdout} stderr={stderr}"
    );

    let alice_cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/alice/config.json")).unwrap(),
    )
    .unwrap();
    assert!(
        alice_cfg.get("session").is_none() || alice_cfg["session"].is_null(),
        "alice's session field should be cleared, got: {alice_cfg}",
    );

    let bob_cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/bob/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bob_cfg["session"]["id"], session);
}

#[test]
fn errors_when_no_session_env_set() {
    let dir = init_repo();
    let repo = dir.path();

    let out = run_as(repo, "alice", &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no session detected"),
        "stderr missing diag: {stderr}"
    );
}

#[test]
fn errors_when_both_session_envs_set() {
    let dir = init_repo();
    let repo = dir.path();
    let out = run_as(
        repo,
        "alice",
        &[
            (
                "CLAUDE_CODE_SESSION_ID",
                "742f6a04-f174-409a-ab01-419a16c5f372",
            ),
            ("CODEX_THREAD_ID", "019e5385-ed97-7603-8561-dd9024328ff9"),
        ],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ambiguous") || stderr.contains("both"),
        "stderr missing ambiguous diag: {stderr}"
    );
}
