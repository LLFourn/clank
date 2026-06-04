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
        .env_remove("CLANK_AGENT")
        .env("HOME", repo);
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

// ─── default_agents seeding ───────────────────────────────────────

fn write_user_config(home: &Path, body: &str) {
    let dir = home.join(".clank");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), body).unwrap();
}

#[test]
fn init_seeds_default_agents_from_user_config() {
    let dir = init_repo();
    let repo = dir.path();
    write_user_config(
        repo,
        r#"{ "default_agents": [
            { "label": "codex" },
            { "label": "ruthless" }
        ] }"#,
    );

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for label in &["codex", "ruthless"] {
        let cfg_path = repo.join(format!(".clank/agents/{label}/config.json"));
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
        assert_eq!(
            cfg["role"], "reviewers",
            "{label} should default to reviewers; got {cfg}"
        );
        assert!(
            cfg.get("session").is_none() || cfg["session"].is_null(),
            "{label} should be unbound (no session); got {cfg}"
        );
    }
}

#[test]
fn init_default_agents_idempotent_preserves_existing_session() {
    // Existing per-agent config (with a session field) must not be
    // overwritten by clank init.
    let dir = init_repo();
    let repo = dir.path();
    let codex_dir = repo.join(".clank/agents/codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let existing = r#"{
        "auto_mode": "on",
        "role": "reviewers",
        "session": {
            "id": "11111111-2222-3333-4444-555555555555",
            "tool": "codex",
            "updated_at": "2026-06-04T12:00:00Z"
        }
    }"#;
    std::fs::write(codex_dir.join("config.json"), existing).unwrap();
    write_user_config(repo, r#"{ "default_agents": [{ "label": "codex" }] }"#);

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(out.status.success(), "init failed");

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(codex_dir.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(
        cfg["session"]["id"], "11111111-2222-3333-4444-555555555555",
        "existing session must be preserved; got {cfg}"
    );
}

#[test]
fn init_no_user_config_no_seeding() {
    // Backward-compat: a user without ~/.clank/config.json sees the
    // same behavior as before this feature shipped.
    let dir = init_repo();
    let repo = dir.path();
    // Deliberately NOT writing ~/.clank/config.json.

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(out.status.success(), "init failed");

    // No codex/ruthless dirs should exist. (The "claude" dir is
    // auto-claimed master by init_auto_claims_master_when_no_existing_agents
    // logic; that's orthogonal.)
    assert!(
        !repo.join(".clank/agents/codex").exists(),
        "no default_agents → no codex dir"
    );
    assert!(
        !repo.join(".clank/agents/ruthless").exists(),
        "no default_agents → no ruthless dir"
    );
}

#[test]
fn init_user_config_without_default_agents_field_no_seeding() {
    let dir = init_repo();
    let repo = dir.path();
    write_user_config(repo, r#"{ "hooks": { "idle": "echo idle" } }"#);

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(out.status.success(), "init failed");

    assert!(
        !repo.join(".clank/agents/codex").exists(),
        "absent default_agents field → no seeding"
    );
}

#[test]
fn init_fails_closed_on_malformed_user_config() {
    let dir = init_repo();
    let repo = dir.path();
    write_user_config(
        repo,
        r#"{ "default_agents": [{ "label": "codex", "role": "not-a-real-role" }] }"#,
    );

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        !out.status.success(),
        "init must fail closed on malformed user config; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn init_seeds_master_role_when_specified() {
    let dir = init_repo();
    let repo = dir.path();
    write_user_config(
        repo,
        r#"{ "default_agents": [
            { "label": "alice", "role": "master" },
            { "label": "bob", "role": "reviewers" }
        ] }"#,
    );

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    let alice: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/alice/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(alice["role"], "master");

    let bob: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/bob/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bob["role"], "reviewers");
}

#[test]
fn init_seeded_master_flips_calling_agent_to_reviewers() {
    // Phase 2 interaction: when default_agents seeds a master entry,
    // bootstrap_agent_identity's has_existing_master check sees it and
    // the calling agent defaults to reviewers (under --yes, no prompt).
    let dir = init_repo();
    let repo = dir.path();
    write_user_config(
        repo,
        r#"{ "default_agents": [{ "label": "lloyd", "role": "master" }] }"#,
    );

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    // Seeded lloyd remains master.
    let lloyd: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/lloyd/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(lloyd["role"], "master");

    // Calling claude session bound as reviewer (not master) because
    // a master already exists from seeding.
    let claude: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/claude/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        claude["role"], "reviewers",
        "claude must default to reviewers when seed already contains a master; got {claude}"
    );
    assert_eq!(
        claude["session"]["id"], CLAUDE_SESSION,
        "claude's session must be bound"
    );
}

#[test]
fn init_calling_agent_label_collides_with_seeded_entry() {
    // Phase 2 interaction: the calling agent's tool-default label
    // (e.g. "codex" for a codex session) collides with a seeded
    // reviewer entry. The calling session binds to the existing
    // skeleton — role preserved, session populated.
    const CODEX_SESSION: &str = "019e54b7-b1c9-7552-8075-69db24499247";
    let dir = init_repo();
    let repo = dir.path();
    write_user_config(
        repo,
        r#"{ "default_agents": [{ "label": "codex", "role": "reviewers" }] }"#,
    );

    let out = run_init(repo, &[("CODEX_THREAD_ID", CODEX_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    let codex: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/agents/codex/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        codex["role"], "reviewers",
        "seeded role must be preserved when the calling session binds; got {codex}"
    );
    assert_eq!(
        codex["session"]["id"], CODEX_SESSION,
        "calling session must be bound to the existing skeleton; got {codex}"
    );
    assert_eq!(codex["session"]["tool"], "codex");
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
