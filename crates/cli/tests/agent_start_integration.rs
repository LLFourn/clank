//! Integration tests for `clank agent start <name> --print`.
//! Exec replaces the process so we use `--print` to capture the
//! composed command instead. Tests cover bound-session
//! requirements, args composition order, codex subcommand
//! attachment, env override precedence, and error diagnostics.

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
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

fn write_skeleton_with_session(
    repo: &Path,
    label: &str,
    tool: clank_core::vocab::Tool,
    session_id: &str,
) {
    let cfg = clank_core::agent_config::AgentConfig {
        auto_mode: clank_core::vocab::AutoMode::Off,
        role: clank_core::vocab::Role::Reviewer, // unused; declaration owns
        wfw_timeout: None,
        session: Some(clank_core::agent_config::Session {
            id: clank_core::ids::SessionId::parse(session_id).unwrap(),
            tool,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }),
        launch: None,
    };
    clank::agent_store::save_agent_config(
        repo,
        &clank_core::ids::AgentLabel::parse(label).unwrap(),
        &cfg,
    )
    .unwrap();
}

fn write_unbound_skeleton(repo: &Path, label: &str) {
    let cfg = clank_core::agent_config::AgentConfig::default();
    clank::agent_store::save_agent_config(
        repo,
        &clank_core::ids::AgentLabel::parse(label).unwrap(),
        &cfg,
    )
    .unwrap();
}

fn run_start(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("agent")
        .arg("start")
        .arg("--repo")
        .arg(repo)
        .args(args);
    cmd.output().expect("spawn clank agent start")
}

/// Write the repo-scope `agents` declaration via the typed struct
/// so schema changes show up as type errors rather than silent
/// JSON drift.
fn write_repo_agents(repo: &Path, agents: &[clank::cli::config::DefaultAgent]) {
    let file = clank::cli::config::RepoAgentsFile {
        agents: Some(agents.to_vec()),
    };
    let json = serde_json::to_string_pretty(&file).unwrap();
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(repo.join(".clank/config.json"), json).unwrap();
}

fn agent(
    label: &str,
    role: clank_core::vocab::Role,
    tool: Option<clank_core::vocab::Tool>,
    launch: Option<clank_core::agent_config::LaunchConfig>,
) -> clank::cli::config::DefaultAgent {
    clank::cli::config::DefaultAgent {
        label: clank_core::ids::AgentLabel::parse(label).unwrap(),
        role,
        tool,
        launch,
    }
}

const CLAUDE_SESSION: &str = "742f6a04-f174-409a-ab01-419a16c5f372";
const CODEX_SESSION: &str = "019e54b7-b1c9-7552-8075-69db24499247";

#[test]
fn agent_start_no_launch_config_uses_bare_tool_for_claude() {
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/claude/config.json",
        &format!(
            r#"{{"auto_mode":"off","role":"master","session":{{"id":"{CLAUDE_SESSION}","tool":"claude","updated_at":"2026-06-04T12:00:00Z"}}}}"#
        ),
    );

    let out = run_start(repo, &["claude", "--print"]);
    assert!(
        out.status.success(),
        "start --print failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    let expected = format!("'claude' '--resume' '{CLAUDE_SESSION}'");
    assert_eq!(trimmed, expected, "stdout was: `{trimmed}`");
}

#[test]
fn agent_start_no_launch_config_uses_bare_tool_for_codex() {
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/codex/config.json",
        &format!(
            r#"{{"auto_mode":"off","role":"reviewers","session":{{"id":"{CODEX_SESSION}","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}}}}"#
        ),
    );

    let out = run_start(repo, &["codex", "--print"]);
    assert!(out.status.success(), "start --print failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    // clank canonicalizes the --repo path (macOS resolves /tmp →
    // /private/tmp), so the printed --cd argument reflects the
    // canonical form. Use dunce::canonicalize to match.
    let canonical = dunce::canonicalize(repo).unwrap();
    let expected = format!(
        "'codex' 'resume' '{CODEX_SESSION}' '--cd' '{}'",
        canonical.display()
    );
    assert_eq!(trimmed, expected, "stdout was: `{trimmed}`");
}

#[test]
fn agent_start_launch_args_precede_session_restore_for_claude() {
    // Locks in the codex-driven ordering decision: launch args
    // attach to the tool itself, BEFORE the session-restore
    // suffix. For claude (flat flags) this is cosmetic but
    // consistent. Launch lives in the repo-scope declaration
    // per `agent-add-cli-and-repo-scope`.
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/ruthless/config.json",
        &format!(
            r#"{{
                "auto_mode":"off",
                "session":{{"id":"{CLAUDE_SESSION}","tool":"claude","updated_at":"2026-06-04T12:00:00Z"}}
            }}"#
        ),
    );
    // Launch profile lives in the merged declaration.
    write_repo_agents(
        repo,
        &[agent(
            "ruthless",
            clank_core::vocab::Role::Reviewer,
            Some(clank_core::vocab::Tool::Claude),
            Some(clank_core::agent_config::LaunchConfig {
                command: Some("claude".into()),
                args: vec!["--skill".into(), "ruthless".into()],
                env: Default::default(),
            }),
        )],
    );

    let out = run_start(repo, &["ruthless", "--print"]);
    assert!(out.status.success(), "start --print failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    let expected = format!("'claude' '--skill' 'ruthless' '--resume' '{CLAUDE_SESSION}'");
    assert_eq!(trimmed, expected, "stdout was: `{trimmed}`");
}

#[test]
fn agent_start_codex_launch_args_precede_subcommand() {
    // Critical for codex: launch.args must attach to the
    // `codex` binary, NOT to the `resume` subcommand. The fix
    // is that launch.args come BEFORE session-restore. Launch
    // lives in the repo-scope declaration.
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/codex-deep/config.json",
        &format!(
            r#"{{
                "auto_mode":"off",
                "session":{{"id":"{CODEX_SESSION}","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}}
            }}"#
        ),
    );
    write_repo_agents(
        repo,
        &[agent(
            "codex-deep",
            clank_core::vocab::Role::Reviewer,
            Some(clank_core::vocab::Tool::Codex),
            Some(clank_core::agent_config::LaunchConfig {
                command: Some("codex".into()),
                args: vec!["--profile".into(), "deep".into()],
                env: Default::default(),
            }),
        )],
    );

    let out = run_start(repo, &["codex-deep", "--print"]);
    assert!(out.status.success(), "start --print failed");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let trimmed = stdout.trim();
    let canonical = dunce::canonicalize(repo).unwrap();
    let expected = format!(
        "'codex' '--profile' 'deep' 'resume' '{CODEX_SESSION}' '--cd' '{}'",
        canonical.display()
    );
    assert_eq!(trimmed, expected, "stdout was: `{trimmed}`");
}

#[test]
fn agent_start_env_overrides_appear_on_stderr() {
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/codex/config.json",
        &format!(
            r#"{{
                "auto_mode":"off",
                "session":{{"id":"{CODEX_SESSION}","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}}
            }}"#
        ),
    );
    // Env lives in the merged declaration's launch profile.
    let mut env = std::collections::BTreeMap::new();
    env.insert("CODEX_PROFILE".to_string(), "review".to_string());
    env.insert("CODEX_LOG".to_string(), "debug".to_string());
    write_repo_agents(
        repo,
        &[agent(
            "codex",
            clank_core::vocab::Role::Reviewer,
            Some(clank_core::vocab::Tool::Codex),
            Some(clank_core::agent_config::LaunchConfig {
                command: Some("codex".into()),
                args: vec![],
                env,
            }),
        )],
    );

    let out = run_start(repo, &["codex", "--print"]);
    assert!(out.status.success(), "start --print failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("env: CODEX_PROFILE=review"),
        "stderr should list env overrides; got: {stderr}"
    );
    assert!(
        stderr.contains("env: CODEX_LOG=debug"),
        "stderr should list all env overrides; got: {stderr}"
    );
}

#[test]
fn agent_start_unknown_agent_errors() {
    let dir = init_repo();
    let out = run_start(dir.path(), &["nonexistent", "--print"]);
    assert!(
        !out.status.success(),
        "unknown agent must error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no agent"),
        "stderr should explain missing agent; got: {stderr}"
    );
}

#[test]
fn agent_start_no_bound_session_errors_with_clank_as_hint() {
    let dir = init_repo();
    let repo = dir.path();
    // Agent config exists but no `session` field.
    write(
        repo,
        ".clank/agents/codex/config.json",
        r#"{"auto_mode":"off","role":"reviewers"}"#,
    );

    let out = run_start(repo, &["codex", "--print"]);
    assert!(
        !out.status.success(),
        "no-bound-session must error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("clank as codex") || stderr.contains("clank as `codex`"),
        "stderr should suggest `clank as <name>`; got: {stderr}"
    );
}

#[test]
fn agent_start_no_session_errors_even_when_launch_command_set() {
    // Locks in the "one policy: session always required" decision
    // (codex review of 12c6c97). Even with launch.command
    // explicitly set, no-bound-session is an error path — NOT a
    // fallback to running the launch command directly.
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/lloyd/config.json",
        r#"{
            "auto_mode":"off",
            "role":"master",
            "launch":{"command":"claude","args":["--skill","ruthless"]}
        }"#,
    );

    let out = run_start(repo, &["lloyd", "--print"]);
    assert!(
        !out.status.success(),
        "no-bound-session must error even with launch.command set; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn agent_start_propagates_malformed_declaration_error() {
    // Codex review of eef4c49: agent start was swallowing
    // load_merged_agents errors with unwrap_or_default. Violates
    // the strict-fail policy + silently drops the configured
    // launch profile.
    let dir = init_repo();
    let repo = dir.path();
    // Valid skeleton with bound session — start would succeed if
    // we ignored the declaration error.
    write(
        repo,
        ".clank/agents/codex/config.json",
        &format!(
            r#"{{
                "auto_mode":"off",
                "session":{{"id":"{CODEX_SESSION}","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}}
            }}"#
        ),
    );
    // Malformed repo-scope declaration.
    write(repo, ".clank/config.json", "{ not json");

    let out = run_start(repo, &["codex", "--print"]);
    assert!(
        !out.status.success(),
        "agent start must fail closed when declaration is malformed; \
         stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("parsing") || stderr.contains("config") || stderr.contains("json"),
        "stderr should explain the parse failure; got: {stderr}"
    );
}
