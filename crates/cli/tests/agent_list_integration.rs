//! Integration tests for `clank agent list`.

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

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn run_list(repo: &Path, json: bool) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("agent").arg("list").arg("--repo").arg(repo);
    if json {
        cmd.arg("--json");
    }
    cmd.output().expect("spawn clank agent list")
}

#[test]
fn agent_list_empty_repo_succeeds() {
    let dir = init_repo();
    let out = run_list(dir.path(), false);
    assert!(
        out.status.success(),
        "agent list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("no agents") || stdout.trim().is_empty(),
        "expected empty-list message; got `{stdout}`"
    );
}

#[test]
fn agent_list_shows_bound_and_unbound() {
    let dir = init_repo();
    let repo = dir.path();
    // Bound master.
    write(
        repo,
        ".clank/agents/claude/config.json",
        r#"{"auto_mode":"off","role":"master","session":{"id":"11111111-1111-1111-1111-111111111111","tool":"claude","updated_at":"2026-06-04T12:00:00Z"}}"#,
    );
    // Bound reviewer.
    write(
        repo,
        ".clank/agents/codex/config.json",
        r#"{"auto_mode":"on","role":"reviewers","session":{"id":"22222222-2222-2222-2222-222222222222","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}}"#,
    );
    // Unbound seeded reviewer.
    write(
        repo,
        ".clank/agents/ruthless/config.json",
        r#"{"auto_mode":"off","role":"reviewers"}"#,
    );

    let out = run_list(repo, false);
    assert!(out.status.success(), "agent list failed");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    // Header
    assert!(
        stdout.contains("LABEL") && stdout.contains("BOUND"),
        "missing header; got `{stdout}`"
    );
    // All three labels appear.
    for label in &["claude", "codex", "ruthless"] {
        assert!(
            stdout.contains(label),
            "expected `{label}` in output; got `{stdout}`"
        );
    }
    // Unbound marker shows up.
    assert!(
        stdout.contains(" NO "),
        "expected NO marker for unbound reviewer; got `{stdout}`"
    );
    // Bound marker too.
    assert!(
        stdout.contains(" yes "),
        "expected yes marker for bound entries; got `{stdout}`"
    );
}

#[test]
fn agent_list_json_schema() {
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/claude/config.json",
        r#"{"auto_mode":"off","role":"master","session":{"id":"11111111-1111-1111-1111-111111111111","tool":"claude","updated_at":"2026-06-04T12:00:00Z"}}"#,
    );
    write(
        repo,
        ".clank/agents/ruthless/config.json",
        r#"{"auto_mode":"off","role":"reviewers"}"#,
    );

    let out = run_list(repo, true);
    assert!(out.status.success(), "agent list --json failed");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("output should be valid JSON");
    let arr = parsed.as_array().expect("output should be an array");
    assert_eq!(arr.len(), 2);
    // Master first (sort order).
    assert_eq!(arr[0]["label"], "claude");
    assert_eq!(arr[0]["role"], "master");
    assert_eq!(arr[0]["bound"], true);
    assert_eq!(arr[0]["tool"], "claude");
    assert!(arr[0]["session_id"].is_string());
    // Then reviewer.
    assert_eq!(arr[1]["label"], "ruthless");
    assert_eq!(arr[1]["role"], "reviewers");
    assert_eq!(arr[1]["bound"], false);
    assert!(arr[1]["tool"].is_null());
    assert!(arr[1]["session_id"].is_null());
}

#[test]
fn agent_list_fails_on_malformed_agent_config() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/agents/broken/config.json", "{ not json");

    let out = run_list(repo, false);
    assert!(
        !out.status.success(),
        "agent list must fail on malformed config; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
