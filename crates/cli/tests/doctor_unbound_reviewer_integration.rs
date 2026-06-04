//! Integration tests for `clank doctor`'s unbound-reviewer warning.

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

fn run_doctor(repo: &Path) -> std::process::Output {
    Command::new(clank_bin())
        .arg("doctor")
        .arg("--repo")
        .arg(repo)
        .arg("--json")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .output()
        .expect("spawn clank doctor")
}

#[test]
fn doctor_warns_on_unbound_reviewer() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    // Seeded reviewer with no session.
    write(
        repo,
        ".clank/agents/ruthless/config.json",
        r#"{"auto_mode":"off","role":"reviewers"}"#,
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("doctor --json should be valid JSON");
    let checks = parsed.as_array().expect("doctor --json should be array");
    let ruthless_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: ruthless"))
        .expect("expected an agent: ruthless check entry");
    assert_eq!(
        ruthless_check["status"], "warn",
        "unbound reviewer should be Warn; got {ruthless_check}"
    );
    let msg = ruthless_check["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("unbound"),
        "warning should mention 'unbound'; got `{msg}`"
    );
    assert!(
        msg.contains("clank as ruthless"),
        "warning should suggest the fix command; got `{msg}`"
    );
}

#[test]
fn doctor_does_not_warn_on_bound_reviewer() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    write(
        repo,
        ".clank/agents/codex/config.json",
        r#"{"auto_mode":"on","role":"reviewers","session":{"id":"11111111-1111-1111-1111-111111111111","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}}"#,
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let codex_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: codex"))
        .expect("expected agent: codex check");
    assert_eq!(
        codex_check["status"], "ok",
        "bound reviewer should be Ok; got {codex_check}"
    );
}

#[test]
fn doctor_warns_on_unbound_master_symmetrically() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    write(
        repo,
        ".clank/agents/lloyd/config.json",
        r#"{"auto_mode":"off","role":"master"}"#,
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let lloyd_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: lloyd"))
        .expect("expected agent: lloyd check");
    assert_eq!(
        lloyd_check["status"], "warn",
        "unbound master should be Warn (symmetric); got {lloyd_check}"
    );
    let msg = lloyd_check["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("unbound"),
        "warning should mention 'unbound'; got `{msg}`"
    );
}

#[test]
fn doctor_warn_for_unbound_does_not_introduce_new_fail() {
    // The unbound-reviewer warning must not escalate to Fail status
    // for the agent entry itself. (Doctor's overall exit code may
    // still be 1 due to pre-existing baseline Fail checks like
    // "no session detected" when run outside an agent; that's
    // unchanged by this plan.)
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/.gitignore", ".gitignore\n");
    write(
        repo,
        ".clank/agents/ruthless/config.json",
        r#"{"auto_mode":"off","role":"reviewers"}"#,
    );

    let out = run_doctor(repo);
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("doctor JSON parse");
    let checks = parsed.as_array().expect("array");
    let ruthless_check = checks
        .iter()
        .find(|c| c["name"].as_str() == Some("agent: ruthless"))
        .expect("expected agent: ruthless check");
    assert_eq!(
        ruthless_check["status"], "warn",
        "unbound reviewer is Warn (not Fail); got {ruthless_check}"
    );
}
