//! Integration tests for `clank init` adopting the user's
//! `default` team (`clank-init-defaults-to-default-team`).

mod common;

use common::TestEnv;
use std::path::Path;

fn run_init(env: &TestEnv, extra: &[&str]) -> std::process::Output {
    env.clank()
        .arg("init")
        .arg("--repo")
        .arg(env.repo())
        .args(extra)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank init")
}

/// Set up ONLY a user-scope team `name` (master declared + team
/// created + master set) via the real cores — WITHOUT writing the
/// repo's team field. This leaves the repo teamless so the
/// bare-init default-adoption path is exercised. (`register_team`
/// can't be used here: it also writes the repo team field.)
fn user_team(env: &TestEnv, name: &str, master: &str) {
    use clank::cli::teams_config::AgentDescription;
    use clank_core::ids::AgentLabel;
    use clank_core::vocab::Tool;
    clank::cli::agent::declare_global_agent(
        env.home(),
        &AgentLabel::parse(master).unwrap(),
        AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
    )
    .unwrap();
    clank::cli::team::create_team(env.home(), name).unwrap();
    clank::cli::team::set_master(env.home(), name, master).unwrap();
}

/// The repo's `team` field as a string, or None if unset/no config.
fn repo_team(repo: &Path) -> Option<String> {
    let body = std::fs::read_to_string(repo.join(".clank/config.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v.get("team")?.as_str().map(str::to_string)
}

#[test]
fn bare_init_adopts_user_default_team() {
    let env = TestEnv::init();
    user_team(&env, "default", "claude");

    let out = run_init(&env, &[]);
    assert!(
        out.status.success(),
        "clank init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        repo_team(env.repo()).as_deref(),
        Some("default"),
        "bare init with a user `default` team should set the repo to it"
    );
}

#[test]
fn bare_init_without_default_team_succeeds_and_warns() {
    // Empty HOME (no ~/.clank) — the brand-new-user case. Init must
    // still scaffold (exit 0), set NO team, and warn to STDERR.
    let env = TestEnv::init();

    let out = run_init(&env, &[]);
    assert!(
        out.status.success(),
        "init must succeed even with no default team (scaffold is the core job)"
    );
    assert_eq!(
        repo_team(env.repo()),
        None,
        "no default team → repo gets no team field"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("default") && stderr.contains("clank init --team"),
        "should warn (to stderr) about the missing default team with guidance; got: {stderr}"
    );
    // The scaffold itself still happened.
    assert!(
        env.repo().join(".clank/.gitignore").is_file(),
        "init should still scaffold .clank/.gitignore"
    );
}

#[test]
fn bare_reinit_preserves_existing_team_selection() {
    // Regression guard (ruthless cc4c7c8): a repo already on
    // `team: dev` must NOT be clobbered to `default` by a bare
    // re-init (e.g. run to refresh hooks/perms).
    let env = TestEnv::init();
    user_team(&env, "dev", "claude");
    user_team_extra_default(&env);

    // First: explicitly select `dev`.
    let out = run_init(&env, &["--team", "dev"]);
    assert!(out.status.success(), "init --team dev failed");
    assert_eq!(repo_team(env.repo()).as_deref(), Some("dev"));

    // Bare re-init must preserve `dev`, not revert to `default`.
    let out = run_init(&env, &[]);
    assert!(out.status.success(), "bare re-init failed");
    assert_eq!(
        repo_team(env.repo()).as_deref(),
        Some("dev"),
        "bare re-init must preserve the existing team selection, not clobber to default"
    );
}

/// Add a `default` user team alongside whatever else exists, so the
/// re-init test proves preservation BEATS the default-adoption path
/// (a default IS available, yet `dev` is kept). `claude` is already
/// declared in user-scope by the prior `user_team` call.
fn user_team_extra_default(env: &TestEnv) {
    clank::cli::team::create_team(env.home(), "default").unwrap();
    clank::cli::team::set_master(env.home(), "default", "claude").unwrap();
}
