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

fn run_start(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("agent")
        .arg("start")
        .arg("--repo")
        .arg(repo)
        .args(args);
    cmd.output().expect("spawn clank agent start")
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
    // consistent.
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/ruthless/config.json",
        &format!(
            r#"{{
                "auto_mode":"off",
                "role":"reviewers",
                "session":{{"id":"{CLAUDE_SESSION}","tool":"claude","updated_at":"2026-06-04T12:00:00Z"}},
                "launch":{{"command":"claude","args":["--skill","ruthless"]}}
            }}"#
        ),
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
    // is that launch.args come BEFORE session-restore.
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/agents/codex-deep/config.json",
        &format!(
            r#"{{
                "auto_mode":"off",
                "role":"reviewers",
                "session":{{"id":"{CODEX_SESSION}","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}},
                "launch":{{"command":"codex","args":["--profile","deep"]}}
            }}"#
        ),
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
                "role":"reviewers",
                "session":{{"id":"{CODEX_SESSION}","tool":"codex","updated_at":"2026-06-04T12:00:00Z"}},
                "launch":{{
                    "command":"codex",
                    "env":{{"CODEX_PROFILE":"review","CODEX_LOG":"debug"}}
                }}
            }}"#
        ),
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
