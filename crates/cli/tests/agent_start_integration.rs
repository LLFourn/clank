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
        .env("HOME", repo) // isolate from user-scope default_agents
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
        initial_prompt: None,
    }
}

const CLAUDE_SESSION: &str = "742f6a04-f174-409a-ab01-419a16c5f372";
const CODEX_SESSION: &str = "019e54b7-b1c9-7552-8075-69db24499247";

#[test]
fn agent_start_no_launch_config_uses_bare_tool_for_claude() {
    let dir = init_repo();
    let repo = dir.path();
    write_skeleton_with_session(
        repo,
        "claude",
        clank_core::vocab::Tool::Claude,
        CLAUDE_SESSION,
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
    write_skeleton_with_session(repo, "codex", clank_core::vocab::Tool::Codex, CODEX_SESSION);

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
fn agent_start_session_none_with_no_tool_errors_with_hint() {
    // Plan: agent-start-bootstraps-missing-skeleton. Renamed from
    // `agent_start_no_bound_session_errors_with_clank_as_hint`.
    //
    // Legacy-skeleton fallback path: `write_unbound_skeleton` writes
    // ONLY an AgentConfig (session=None, launch=None). No explicit
    // declaration. `load_merged_agents` legacy-synthesizes a
    // declaration via `config.rs:502-506` where tool is derived
    // from cfg.session — None here → tool=None. Bootstrap's
    // tool-resolution priority (launch.command > tool > error)
    // surfaces the no-bootstrap-tool error. Pinned per codex a8164a1
    // catch on the prior plan revision.
    let dir = init_repo();
    let repo = dir.path();
    write_unbound_skeleton(repo, "codex");

    let out = run_start(repo, &["codex", "--print"]);
    assert!(
        !out.status.success(),
        "session-none + no-tool must error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("codex"),
        "stderr should name the agent label; got: {stderr}"
    );
    assert!(
        stderr.contains("no bootstrap tool"),
        "stderr should classify the failure as no-bootstrap-tool; got: {stderr}"
    );
    assert!(
        stderr.contains("clank agent remove") || stderr.contains("edit"),
        "stderr should give an actionable path; got: {stderr}"
    );
}

#[test]
fn agent_start_session_none_with_launch_command_bootstraps() {
    // Plan: agent-start-bootstraps-missing-skeleton. Inverts
    // `agent_start_no_session_errors_even_when_launch_command_set`
    // from 12c6c97 — that test locked in the strict "session
    // always required" policy, which the bootstrap plan
    // intentionally supersedes. With launch.command set, tool
    // resolution succeeds; the bootstrap path fires.
    let dir = init_repo();
    let repo = dir.path();
    let cfg = clank_core::agent_config::AgentConfig {
        auto_mode: clank_core::vocab::AutoMode::Off,
        role: clank_core::vocab::Role::Master,
        launch: Some(clank_core::agent_config::LaunchConfig {
            command: Some("my-claude-wrapper".into()),
            args: vec!["--skill".into(), "ruthless".into()],
            env: Default::default(),
        }),
        ..Default::default()
    };
    write(
        repo,
        ".clank/agents/lloyd/config.json",
        &serde_json::to_string_pretty(&cfg).unwrap(),
    );

    let out = run_start(repo, &["lloyd", "--print"]);
    assert!(
        out.status.success(),
        "session-none + launch.command should bootstrap, not error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("my-claude-wrapper"),
        "argv must use launch.command, not the tool fallback; got: {stdout}"
    );
    assert!(
        stdout.contains("Run `clank as lloyd` to bind this session."),
        "argv must include the bootstrap bind prompt; got: {stdout}"
    );
}

#[test]
fn agent_start_bootstraps_when_skeleton_missing() {
    // Plan: agent-start-bootstraps-missing-skeleton. The
    // missing-skeleton path: declaration registers `phantom` with
    // a resolvable tool, but no per-repo skeleton exists. Bootstrap
    // spawns the bare tool with the seed prompt.
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        &[clank::cli::config::DefaultAgent {
            label: clank_core::ids::AgentLabel::parse("phantom").unwrap(),
            role: clank_core::vocab::Role::Reviewer,
            tool: Some(clank_core::vocab::Tool::Claude),
            launch: None,
            initial_prompt: None,
        }],
    );
    // Do NOT write a skeleton.

    let out = run_start(repo, &["phantom", "--print"]);
    assert!(
        out.status.success(),
        "missing-skeleton + declaration with tool should bootstrap; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("claude"),
        "argv must start with the resolved tool `claude`; got: {stdout}"
    );
    assert!(
        stdout.contains("Run `clank as phantom` to bind this session."),
        "argv must include the bootstrap bind prompt; got: {stdout}"
    );
    // Bootstrap path MUST NOT add a session-restore suffix.
    assert!(
        !stdout.contains("--resume"),
        "bootstrap must not pass --resume (no session yet); got: {stdout}"
    );
}

#[test]
fn agent_start_bootstraps_when_skeleton_exists_but_session_none() {
    // Plan: agent-start-bootstraps-missing-skeleton — codex eef6853
    // catch. The fresh-init path: `clank init` seeds default_agents
    // as skeletons-without-session. With a declaration that resolves
    // a tool, bootstrap fires for this case too.
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        &[clank::cli::config::DefaultAgent {
            label: clank_core::ids::AgentLabel::parse("phantom").unwrap(),
            role: clank_core::vocab::Role::Reviewer,
            tool: Some(clank_core::vocab::Tool::Codex),
            launch: None,
            initial_prompt: None,
        }],
    );
    // Skeleton EXISTS but session: None (the fresh-init shape).
    write_unbound_skeleton(repo, "phantom");

    let out = run_start(repo, &["phantom", "--print"]);
    assert!(
        out.status.success(),
        "skeleton + session=None + declaration tool should bootstrap; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("codex"),
        "argv must use the declaration's tool (codex); got: {stdout}"
    );
    assert!(
        stdout.contains("Run `clank as phantom` to bind this session."),
        "argv must include the bootstrap bind prompt; got: {stdout}"
    );
    // No resume — there's no session id yet.
    assert!(
        !stdout.contains("--resume") && !stdout.contains(" resume "),
        "bootstrap must not pass any session-restore suffix; got: {stdout}"
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

#[test]
fn agent_start_initial_prompt_lands_in_composed_print() {
    // Acceptance criterion: `clank agent start <label> --print`
    // for an agent with auto_mode=On (no explicit initial_prompt)
    // shows the default prompt as the trailing positional arg.
    let dir = init_repo();
    let repo = dir.path();
    // Skeleton: auto_mode = On + bound claude session.
    let cfg = clank_core::agent_config::AgentConfig {
        auto_mode: clank_core::vocab::AutoMode::On,
        role: clank_core::vocab::Role::Reviewer,
        wfw_timeout: None,
        session: Some(clank_core::agent_config::Session {
            id: clank_core::ids::SessionId::parse(CLAUDE_SESSION).unwrap(),
            tool: clank_core::vocab::Tool::Claude,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }),
        launch: None,
    };
    clank::agent_store::save_agent_config(
        repo,
        &clank_core::ids::AgentLabel::parse("claude").unwrap(),
        &cfg,
    )
    .unwrap();
    // Declaration: minimal — no explicit initial_prompt.
    write_repo_agents(
        repo,
        &[agent(
            "claude",
            clank_core::vocab::Role::Reviewer,
            Some(clank_core::vocab::Tool::Claude),
            None,
        )],
    );

    let out = run_start(repo, &["claude", "--print"]);
    assert!(
        out.status.success(),
        "--print failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let trimmed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let expected = format!("'claude' '--resume' '{CLAUDE_SESSION}' 'Session resumed.'");
    assert_eq!(
        trimmed, expected,
        "auto_mode=On + no explicit prompt → default 'Session resumed.' as trailing arg; got: `{trimmed}`"
    );
}
