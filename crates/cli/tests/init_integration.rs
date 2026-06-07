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

/// Variant for tests that need a HOME distinct from the repo
/// (e.g., codex 3cb8002 catch: user-scope `default_agents` at
/// `$HOME/.clank/config.json` must NOT be re-seeded into a repo
/// that has its own declaration via the migrated legacy block).
fn run_init_with_home(repo: &Path, home: &Path, env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("init")
        .arg("--yes")
        .arg("--repo")
        .arg(repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .env("HOME", home);
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
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            auto_mode: clank_core::vocab::AutoMode::Off,
            role: clank_core::vocab::Role::Master,
            ..Default::default()
        })
        .unwrap(),
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
        cfg["role"], "reviewer",
        "second agent should default to reviewers when master exists; got config: {cfg}"
    );
}

// ─── default_agents seeding ───────────────────────────────────────

fn write_user_config(home: &Path, body: &str) {
    let dir = home.join(".clank");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.json"), body).unwrap();
}

/// Typed `UserConfigFile` writer for `~/.clank/config.json` per
/// `typed-config-dogfood`. Prefer this for new sites; the
/// string-based `write_user_config` above is kept for tests that
/// deliberately exercise alias / negative-input paths.
fn write_user_config_typed(home: &Path, file: &clank::cli::config::UserConfigFile) {
    write_user_config(home, &serde_json::to_string_pretty(file).unwrap());
}

/// Builder for a `DefaultAgent` with sensible defaults.
fn user_agent(label: &str) -> clank::cli::config::DefaultAgent {
    clank::cli::config::DefaultAgent {
        label: clank_core::ids::AgentLabel::parse(label).unwrap(),
        role: clank_core::vocab::Role::default(),
        tool: None,
        launch: None,
        initial_prompt: None,
    }
}

#[test]
fn init_seeds_default_agents_from_user_config() {
    let dir = init_repo();
    let repo = dir.path();
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![user_agent("codex"), user_agent("ruthless")]),
            ..Default::default()
        },
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
            cfg["role"], "reviewer",
            "{label} should default to reviewers; got {cfg}"
        );
        assert!(
            cfg.get("session").is_none() || cfg["session"].is_null(),
            "{label} should be unbound (no session); got {cfg}"
        );
    }
}

#[test]
fn init_with_repo_scope_agents_seeds_override_set() {
    // Codex review of eef4c49: `clank init` must consume the
    // MERGED declaration (repo-scope `agents` if present, else
    // user-scope `default_agents`). Pre-fix it seeded only from
    // user-scope, so a repo with its own `agents` field still got
    // user-scope skeletons.
    let dir = init_repo();
    let repo = dir.path();
    // User-scope says [codex, ruthless].
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![user_agent("codex"), user_agent("ruthless")]),
            ..Default::default()
        },
    );
    // Repo-scope overrides with just [overlord]. Uses the
    // plural "reviewers" alias deliberately — locks in the
    // serde-alias backwards-compat path. allow-json-literal:
    // exercising the legacy `"reviewers"` alias on input.
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        // allow-json-literal: alias-on-input regression
        r#"{"agents":[{"label":"overlord","role":"reviewers"}]}"#,
    )
    .unwrap();

    let out = run_init(repo, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(out.status.success(), "init failed");

    // Only overlord's skeleton should exist; the user-scope
    // codex/ruthless must NOT be seeded.
    assert!(
        repo.join(".clank/agents/overlord/config.json").exists(),
        "repo-scope agent must be seeded"
    );
    assert!(
        !repo.join(".clank/agents/codex/config.json").exists(),
        "user-scope agent must NOT be seeded when repo-scope is present"
    );
    assert!(
        !repo.join(".clank/agents/ruthless/config.json").exists(),
        "user-scope agent must NOT be seeded when repo-scope is present"
    );
}

#[test]
fn init_migrates_legacy_block_does_not_reseed_user_scope_with_separate_home() {
    // Codex 3cb8002 catch: when HOME is distinct from the repo,
    // pre-fix init ran migrate_legacy_agents_block (alice's
    // legacy entry → .clank/agents/alice/config.json, key
    // removed) and then seed_default_agents fell back to
    // user-scope `default_agents` since the repo key was gone.
    // Result: the repo got BOTH alice (from migration) AND
    // codex (from user-scope), violating the legacy REPLACE
    // semantic that "repo agents block overrides user-scope."
    //
    // Post-fix: seed_default_agents detects that the repo
    // already has a declaration (skeletons exist post-migration,
    // OR sentinel file exists) and skips user-scope seeding.
    let dir = init_repo();
    let repo = dir.path();
    let home_dir = tempfile::tempdir().unwrap();
    let home = home_dir.path();

    // User-scope defaults: codex (would be wrongly seeded pre-fix).
    write_user_config_typed(
        home,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![user_agent("codex")]),
            ..Default::default()
        },
    );
    // Repo-scope legacy block: alice (will be migrated). Exercises
    // the pre-migration legacy `agents` field shape directly.
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    let legacy_block_body =
        // allow-json-literal: pre-fix shape we defend the migration against
        r#"{"agents":[{"label":"alice","role":"master","tool":"claude"}]}"#;
    std::fs::write(repo.join(".clank/config.json"), legacy_block_body).unwrap();

    let out = run_init_with_home(repo, home, &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    // alice's skeleton must exist (migrated from legacy block).
    assert!(
        repo.join(".clank/agents/alice/config.json").exists(),
        "migrated alice must have a skeleton"
    );
    // codex's skeleton must NOT exist — user-scope was suppressed
    // by the migrated repo declaration.
    assert!(
        !repo.join(".clank/agents/codex/config.json").exists(),
        "user-scope codex must NOT be seeded when repo has migrated declaration"
    );
}

#[test]
fn init_default_agents_idempotent_preserves_existing_session() {
    // Existing per-agent config (with a session field) must not be
    // overwritten by clank init.
    let dir = init_repo();
    let repo = dir.path();
    let codex_dir = repo.join(".clank/agents/codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    let existing = clank_core::agent_config::AgentConfig {
        auto_mode: clank_core::vocab::AutoMode::On,
        session: Some(clank_core::agent_config::Session {
            id: clank_core::ids::SessionId::parse("11111111-2222-3333-4444-555555555555").unwrap(),
            tool: clank_core::vocab::Tool::Codex,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }),
        ..Default::default()
    };
    std::fs::write(
        codex_dir.join("config.json"),
        serde_json::to_string_pretty(&existing).unwrap(),
    )
    .unwrap();
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![user_agent("codex")]),
            ..Default::default()
        },
    );

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
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            hooks: Some(clank::cli::config::HooksSection {
                idle: Some(Some("echo idle".to_string())),
                ..Default::default()
            }),
            ..Default::default()
        },
    );

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
        // allow-json-literal: deliberately invalid `role` for fail-closed test (typed construction would refuse to compile).
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
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![
                clank::cli::config::DefaultAgent {
                    label: clank_core::ids::AgentLabel::parse("alice").unwrap(),
                    role: clank_core::vocab::Role::Master,
                    tool: None,
                    launch: None,
                    initial_prompt: None,
                },
                user_agent("bob"),
            ]),
            ..Default::default()
        },
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
    assert_eq!(bob["role"], "reviewer");
}

#[test]
fn init_default_agents_preserve_role_even_when_label_already_bound() {
    // Codex's catch on 5036f18: default_agents declares codex as
    // reviewer; codex already has a bound config (session populated)
    // from a prior `clank as`; no master exists; calling session is
    // codex's. Under the session-absence guard, preserve_existing_role
    // was false because codex had a session — Phase 2 then flipped
    // codex to master, overriding the user's declared default.
    //
    // The fix: scope preserve to "label in default_agents" not
    // "session absent". The user's declared default is the source
    // of truth for the role regardless of bind state.
    const CODEX_SESSION: &str = "019e54b7-b1c9-7552-8075-69db24499247";
    let dir = init_repo();
    let repo = dir.path();
    // User declares codex as reviewer.
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![user_agent("codex")]),
            ..Default::default()
        },
    );
    // codex was previously bound via `clank as`: role reviewer + session populated.
    let codex_dir = repo.join(".clank/agents/codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("config.json"),
        serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            session: Some(clank_core::agent_config::Session {
                id: clank_core::ids::SessionId::parse(CODEX_SESSION).unwrap(),
                tool: clank_core::vocab::Tool::Codex,
                updated_at: "2026-06-04T12:00:00Z".to_string(),
            }),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();

    let out = run_init(repo, &[("CODEX_THREAD_ID", CODEX_SESSION)]);
    assert!(
        out.status.success(),
        "init failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    let codex: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(codex_dir.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(
        codex["role"], "reviewer",
        "user's declared default_agents role must be preserved even when the label is already bound; got {codex}"
    );
    // Session still bound (binding logic ran).
    assert_eq!(
        codex["session"]["id"], CODEX_SESSION,
        "session binding must be preserved"
    );
}

#[test]
fn init_existing_bound_agent_still_claims_master_when_no_master_exists() {
    // Regression for codex's catch on 1b719cc: a pre-existing agent
    // config created by `clank as` (i.e. with a session field) and
    // no master in the repo must still go through the master-claim
    // flow on subsequent `clank init`. The preserve_existing_role
    // guard is scoped to seeded skeletons (no session); a previously
    // bound agent is NOT a skeleton and shouldn't be locked out of
    // role assignment.
    let dir = init_repo();
    let repo = dir.path();
    // No user-scope default_agents at all.
    // Pre-existing codex bound by a prior `clank as`: role=reviewers,
    // session populated. No other agents in the repo.
    let codex_dir = repo.join(".clank/agents/codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("config.json"),
        serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig {
            session: Some(clank_core::agent_config::Session {
                id: clank_core::ids::SessionId::parse("019e54b7-b1c9-7552-8075-69db24499247")
                    .unwrap(),
                tool: clank_core::vocab::Tool::Codex,
                updated_at: "2026-06-04T12:00:00Z".to_string(),
            }),
            ..Default::default()
        })
        .unwrap(),
    )
    .unwrap();

    let out = run_init(
        repo,
        &[("CODEX_THREAD_ID", "019e54b7-b1c9-7552-8075-69db24499247")],
    );
    assert!(
        out.status.success(),
        "init failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    let cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(codex_dir.join("config.json")).unwrap())
            .unwrap();
    assert_eq!(
        cfg["role"], "master",
        "previously bound agent should still be eligible to claim master when no master exists; \
         got {cfg}"
    );
}

#[test]
fn init_seeded_master_flips_calling_agent_to_reviewers() {
    // Phase 2 interaction: when default_agents seeds a master entry,
    // bootstrap_agent_identity's has_existing_master check sees it and
    // the calling agent defaults to reviewers (under --yes, no prompt).
    let dir = init_repo();
    let repo = dir.path();
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![clank::cli::config::DefaultAgent {
                label: clank_core::ids::AgentLabel::parse("lloyd").unwrap(),
                role: clank_core::vocab::Role::Master,
                tool: None,
                launch: None,
                initial_prompt: None,
            }]),
            ..Default::default()
        },
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
        claude["role"], "reviewer",
        "claude must default to reviewer when seed already contains a master; got {claude}"
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
    write_user_config_typed(
        repo,
        &clank::cli::config::UserConfigFile {
            default_agents: Some(vec![user_agent("codex")]),
            ..Default::default()
        },
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
        codex["role"], "reviewer",
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
