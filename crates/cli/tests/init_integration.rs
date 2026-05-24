//! Integration tests for `clank init` auto-claim-master behavior.

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
    git(path, &["init", "--quiet", "--initial-branch=work"]);
    git(path, &["config", "user.email", "test@test"]);
    git(path, &["config", "user.name", "test"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    std::fs::write(path.join("README.md"), "# test\n").unwrap();
    git(path, &["add", "-A"]);
    git(path, &["commit", "--quiet", "-m", "init"]);
    dir
}

const CLAUDE_SESSION: &str = "742f6a04-f174-409a-ab01-419a16c5f372";

fn run_init(repo: &Path, env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("init")
        .arg("--yes")
        .arg("--repo")
        .arg(repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn clank init")
}

#[test]
fn init_auto_claims_master_when_no_existing_agents() {
    let dir = init_repo();
    let repo = dir.path();

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/claude/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cfg["role"], "master",
        "first agent should auto-claim master; got config: {cfg}"
    );
}

#[test]
fn init_defaults_to_reviewers_when_master_exists() {
    let dir = init_repo();
    let repo = dir.path();

    let bob_dir = repo.join(".clank/agents/bob");
    std::fs::create_dir_all(&bob_dir).unwrap();
    std::fs::write(
        bob_dir.join("config.json"),
        r#"{"auto_mode":"off","role":"master"}"#,
    )
    .unwrap();

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/claude/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cfg["role"], "reviewers",
        "second agent should default to reviewers when master exists; got config: {cfg}"
    );
}

#[test]
fn init_fails_on_corrupt_existing_agent_config() {
    let dir = init_repo();
    let repo = dir.path();

    let bob_dir = repo.join(".clank/agents/bob");
    std::fs::create_dir_all(&bob_dir).unwrap();
    std::fs::write(bob_dir.join("config.json"), b"not json").unwrap();

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        !out.status.success(),
        "init should fail on corrupt config; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
