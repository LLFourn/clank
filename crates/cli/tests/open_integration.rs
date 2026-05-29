//! Integration tests for `clank open`.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

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

fn run_open(path: &Path, home: &Path) -> Value {
    let out = Command::new(clank_bin())
        .args(["open", "--json"])
        .arg(path)
        .env("HOME", home)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank open");
    assert!(
        out.status.success(),
        "clank open failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

fn run_open_raw(arg: &str, home: &Path) -> Value {
    let out = Command::new(clank_bin())
        .args(["open", "--json", arg])
        .env("HOME", home)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank open");
    assert!(
        out.status.success(),
        "clank open failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON")
}

fn rec_kinds(v: &Value) -> Vec<String> {
    v["recommendations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn path_missing_returns_clean_response() {
    let home = tempfile::tempdir().unwrap();
    let nonexistent = home.path().join("does-not-exist-xyz");
    let v = run_open(&nonexistent, home.path());
    assert_eq!(v["state"], "path_missing");
    assert!(v.get("repo_root").is_none());
    assert!(v.get("git").is_none());
    assert!(v.get("clank").is_none());
    let kinds = rec_kinds(&v);
    assert_eq!(kinds.first().map(String::as_str), Some("init_directory"));
}

#[test]
fn path_not_directory() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("a.txt");
    std::fs::write(&file, "hi").unwrap();
    let v = run_open(&file, home.path());
    assert_eq!(v["state"], "path_not_directory");
    assert!(v["recommendations"].as_array().unwrap().is_empty());
}

#[test]
fn empty_directory() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let v = run_open(target.path(), home.path());
    assert_eq!(v["state"], "empty_directory");
    let kinds = rec_kinds(&v);
    assert_eq!(kinds.first().map(String::as_str), Some("git_init"));
}

#[test]
fn directory_not_git() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    std::fs::write(target.path().join("README.md"), "x").unwrap();
    let v = run_open(target.path(), home.path());
    assert_eq!(v["state"], "directory_not_git");
}

#[test]
fn git_without_clank() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let v = run_open(dir.path(), home.path());
    assert_eq!(v["state"], "git_without_clank");
    assert_eq!(v["git"]["is_repo"], true);
    assert_eq!(v["git"]["head_branch"], "main");
    assert_eq!(v["git"]["dirty"], false);
    let kinds = rec_kinds(&v);
    assert_eq!(kinds, vec!["clank_init", "bind_agent"]);
}

#[test]
fn clank_initialized_with_master_and_reviewer() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(
        repo,
        ".clank/agents/claude/config.json",
        r#"{"role":"master","session":{"id":"00000000-0000-4000-8000-000000000001","tool":"claude","updated_at":"2026-01-01T00:00:00Z"}}"#,
    );
    write(
        repo,
        ".clank/agents/codex/config.json",
        r#"{"role":"reviewers","session":{"id":"00000000-0000-4000-8000-000000000002","tool":"codex","updated_at":"2026-01-01T00:00:00Z"}}"#,
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    assert_eq!(v["state"], "clank_initialized");
    assert_eq!(v["clank"]["master_agents"], serde_json::json!(["claude"]));
    assert_eq!(v["clank"]["agents"].as_array().unwrap().len(), 2);
    // No JSONL on disk under fake HOME → not resumable, so bind_agent
    // recommendations rather than resume_agent.
    let kinds = rec_kinds(&v);
    assert!(kinds.iter().all(|k| k == "bind_agent"));
    assert_eq!(kinds.len(), 2);
}

#[test]
fn clank_initialized_subdir_resolves_to_repo_root() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(repo, "src/lib.rs", "fn main() {}\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(&repo.join("src"), home.path());
    assert_eq!(v["state"], "clank_initialized");
    let opened = v["opened_path"].as_str().unwrap();
    let root = v["repo_root"].as_str().unwrap();
    assert!(
        opened.ends_with("src"),
        "opened_path should be subdir; got {opened}"
    );
    assert_ne!(opened, root);
    // No clank_init recommendation when already initialized.
    let kinds = rec_kinds(&v);
    assert!(!kinds.iter().any(|k| k == "clank_init"));
}

#[test]
fn master_agents_empty_when_all_reviewers() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(
        repo,
        ".clank/agents/a/config.json",
        r#"{"role":"reviewers"}"#,
    );
    write(
        repo,
        ".clank/agents/b/config.json",
        r#"{"role":"reviewers"}"#,
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    assert_eq!(v["clank"]["master_agents"], serde_json::json!([]));
}

#[test]
fn master_agents_lists_multiple_masters() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(repo, ".clank/agents/a/config.json", r#"{"role":"master"}"#);
    write(repo, ".clank/agents/b/config.json", r#"{"role":"master"}"#);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    let masters = v["clank"]["master_agents"].as_array().unwrap();
    assert_eq!(masters.len(), 2);
    let mut labels: Vec<&str> = masters.iter().map(|x| x.as_str().unwrap()).collect();
    labels.sort();
    assert_eq!(labels, vec!["a", "b"]);
}

#[test]
fn git_dir_normalized_from_repo_root() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let v = run_open(dir.path(), home.path());
    let git_dir = v["git"]["git_dir"].as_str().unwrap();
    assert!(
        Path::new(git_dir).is_absolute(),
        "git_dir should be absolute: {git_dir}"
    );
    assert!(
        git_dir.ends_with(".git"),
        "git_dir should end with .git: {git_dir}"
    );
}

#[test]
fn resume_recommended_when_session_jsonl_exists() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    let uuid = "00000000-0000-4000-8000-000000000aaa";
    write(
        repo,
        ".clank/agents/claude/config.json",
        &format!(r#"{{"session":{{"id":"{uuid}","tool":"claude","updated_at":"2026-01-01T00:00:00Z"}}}}"#),
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    // Fake the Claude session file under the test HOME. The
    // inspector accepts ANY project directory under
    // ~/.claude/projects containing <UUID>.jsonl.
    let projects = home.path().join(".claude/projects/somewhere");
    std::fs::create_dir_all(&projects).unwrap();
    std::fs::write(projects.join(format!("{uuid}.jsonl")), "{}\n").unwrap();

    let v = run_open(repo, home.path());
    let recs = v["recommendations"].as_array().unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["kind"], "resume_agent");
    assert_eq!(recs[0]["command_hint"], format!("claude --resume {uuid}"));
    assert_eq!(v["clank"]["agents"][0]["session_resumable"], true);
}

#[test]
fn stale_session_id_falls_back_to_bind_agent() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    write(
        repo,
        ".clank/agents/claude/config.json",
        r#"{"session":{"id":"00000000-0000-4000-8000-deadbeefcafe","tool":"claude","updated_at":"2026-01-01T00:00:00Z"}}"#,
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let v = run_open(repo, home.path());
    let recs = v["recommendations"].as_array().unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["kind"], "bind_agent");
    assert_eq!(v["clank"]["agents"][0]["session_resumable"], false);
}

#[test]
fn codex_session_recommended_when_rollout_jsonl_exists() {
    let home = tempfile::tempdir().unwrap();
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{}");
    let uuid = "019e73af-9204-7790-a997-b2e80ee0a6d1";
    write(
        repo,
        ".clank/agents/codex/config.json",
        &format!(r#"{{"session":{{"id":"{uuid}","tool":"codex","updated_at":"2026-01-01T00:00:00Z"}}}}"#),
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let day_dir = home.path().join(".codex/sessions/2026/05/29");
    std::fs::create_dir_all(&day_dir).unwrap();
    std::fs::write(
        day_dir.join(format!("rollout-2026-05-29T22-22-26-{uuid}.jsonl")),
        "{}\n",
    )
    .unwrap();

    let v = run_open(repo, home.path());
    let recs = v["recommendations"].as_array().unwrap();
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0]["kind"], "resume_agent");
    assert_eq!(recs[0]["command_hint"], format!("codex resume {uuid}"));
}

#[test]
fn warnings_field_omitted_when_empty() {
    let home = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let v = run_open(target.path(), home.path());
    assert!(v.get("warnings").is_none());
}

#[test]
fn lex_clean_for_missing_relative_path() {
    let home = tempfile::tempdir().unwrap();
    // Relative path; should be lex-cleaned to an absolute one
    // under cwd. The inspector does NOT canonicalize missing
    // paths, so this exercises the lex-clean branch.
    let v = run_open_raw("./missing-relative-xyz", home.path());
    assert_eq!(v["state"], "path_missing");
    assert!(
        Path::new(v["opened_path"].as_str().unwrap()).is_absolute(),
        "opened_path should be absolute for missing relative input"
    );
}
