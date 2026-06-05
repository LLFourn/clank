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
        .env_remove("CLANK_AGENT")
        .env("HOME", repo);
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
fn auto_on_writes_on() {
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
    assert_eq!(cfg["auto_mode"], "on");
}

#[test]
fn auto_on_legacy_hint_deserializes_as_on() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    let cfg_path = repo.join(".clank/agents/alice/config.json");
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    cfg["auto_mode"] = serde_json::json!("hint");
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

    let out = run_clank(
        repo,
        &["auto", "status"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("auto_mode:   on"),
        "legacy 'hint' should read as 'on'; got {stdout}"
    );
}

#[test]
fn auto_on_legacy_wait_deserializes_as_on() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    let cfg_path = repo.join(".clank/agents/alice/config.json");
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    cfg["auto_mode"] = serde_json::json!("wait");
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

    let out = run_clank(
        repo,
        &["auto", "status"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("auto_mode:   on"),
        "legacy 'wait' should read as 'on'; got {stdout}"
    );
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
fn auto_on_role_master_writes_agent_config() {
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    let out = run_clank(
        repo,
        &["auto", "on", "--role", "master"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/alice/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["role"], "master");
    // No repo-shared config.json should exist — master is
    // per-user preference, not a repo claim.
    assert!(!repo.join(".clank/config.json").exists());
}

#[test]
fn auto_on_role_reviewers_writes_agent_config() {
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

    let cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/alice/config.json")).unwrap(),
    )
    .unwrap();
    // Canonical singular per role-reviewers-to-reviewer-rename.
    // `--role reviewers` on the CLI still works via the clap
    // alias, but the on-disk value is the canonical "reviewer".
    assert_eq!(cfg["role"], "reviewer");
}

#[test]
fn auto_on_role_only_touches_self() {
    // Alice's --role doesn't affect bob's config at all (each
    // agent's role is independent per-user state).
    let dir = init_repo();
    let repo = dir.path();
    bind_alice(repo);

    // Pre-seed bob with role=master via a direct file write.
    std::fs::create_dir_all(repo.join(".clank/agents/bob")).unwrap();
    std::fs::write(
        repo.join(".clank/agents/bob/config.json"),
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            auto_mode: clank_core::vocab::AutoMode::Off,
            role: clank_core::vocab::Role::Master,
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();

    // Alice claims reviewer.
    let out = run_clank(
        repo,
        &["auto", "on", "--role", "reviewers"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(out.status.success());

    // Bob's config untouched.
    let bob_cfg: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/bob/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bob_cfg["role"], "master");
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
    assert_eq!(body["auto_mode"], "on");
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

#[test]
fn clank_agent_overrides_both_tools_detected() {
    // Per the pure resolver's precedence: CLANK_AGENT > session
    // lookup. The CLI wrapper MUST honor that — it can't fail
    // early on BothToolsDetected when the explicit override is
    // set. (Caught by codex on 99ce55b.)
    let dir = init_repo();
    let repo = dir.path();
    // No `clank as` is needed — the explicit override
    // short-circuits identity resolution before any session env
    // gets parsed.
    let out = run_clank(
        repo,
        &["auto", "status", "--json"],
        &[
            ("CLANK_AGENT", "alice"),
            ("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION),
            ("CODEX_THREAD_ID", "019e5385-ed97-7603-8561-dd9024328ff9"),
        ],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(body["label"], "alice");
}

#[test]
fn clank_agent_overrides_invalid_session_id() {
    // Same precedence rule: a malformed session env should not
    // fail the command when the explicit override is set.
    let dir = init_repo();
    let repo = dir.path();
    let out = run_clank(
        repo,
        &["auto", "status", "--json"],
        &[
            ("CLANK_AGENT", "alice"),
            ("CLAUDE_CODE_SESSION_ID", "not/a/valid/session"),
        ],
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid JSON");
    assert_eq!(body["label"], "alice");
}
