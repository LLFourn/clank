//! Integration tests for `clank stop-hook`.
//!
//! Spawns the real binary with a controlled hook stdin JSON and
//! verifies per-tool wire output (exit code + stdout/stderr)
//! against the per-tool contract table from
//! `clank-agent-integration` plan + `clank_core::hook_io`.

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo() -> common::TestEnv {
    let env = common::TestEnv::init();
    // Register a master + alice reviewer via the real cores
    // (`dogfood-init-setup-in-tests`) so the gate treats her as
    // a registered reviewer.
    env.register_team("master", &["alice"], &[]);
    env
}

const CLAUDE_SESSION: &str = "742f6a04-f174-409a-ab01-419a16c5f372";
const CODEX_SESSION: &str = "019e5385-ed97-7603-8561-dd9024328ff9";

fn run_clank(
    env: &common::TestEnv,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = env.clank();
    cmd.args(args)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.arg("--repo")
        .arg(env.repo())
        .output()
        .expect("spawn clank")
}

fn bind_alice(env: &common::TestEnv) {
    let out = run_clank(
        env,
        &["as", "alice"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(
        out.status.success(),
        "bind alice failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn turn_auto_on(env: &common::TestEnv) {
    let out = run_clank(
        env,
        &["auto", "on", "--wfw-timeout", "10s"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(
        out.status.success(),
        "auto on failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Spawn `clank stop-hook --tool <tool> --repo <path>` with the
/// given hook stdin JSON. Returns (exit_code, stdout, stderr).
fn run_stop_hook(
    env: &common::TestEnv,
    tool: &str,
    stdin_json: &str,
    extra_env: &[(&str, &str)],
) -> (Option<i32>, String, String) {
    let mut cmd = env.clank();
    cmd.arg("stop-hook")
        .arg("--tool")
        .arg(tool)
        .arg("--repo")
        .arg(env.repo())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn clank stop-hook");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .unwrap();
    let output = child.wait_with_output().expect("wait stop-hook");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn claude_stdin(session: &str, repo: &Path, stop_hook_active: bool) -> String {
    format!(
        r#"{{
            "session_id": "{session}",
            "cwd": "{cwd}",
            "stop_hook_active": {stop_hook_active}
        }}"#,
        cwd = repo.display(),
    )
}

#[test]
fn stop_hook_active_still_fires_continuation() {
    // Regression: the old adapter short-circuited to Silent on
    // `stop_hook_active=true`. The whole point of removing the guard
    // is that `stop_hook_active` is now ignored — hint/wait fire
    // regardless of chain position. This test locks that in.
    let env = init_repo();
    let repo = env.repo();
    std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
    std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    bind_alice(&env);
    turn_auto_on(&env);

    let stdin = claude_stdin(CLAUDE_SESSION, repo, true);
    let (code, stdout, stderr) = run_stop_hook(&env, "claude", &stdin, &[]);
    assert_eq!(
        code,
        Some(2),
        "expected claude continuation even with stop_hook_active=true; stderr={stderr}"
    );
    assert!(
        stdout.is_empty(),
        "claude continuation goes to stderr: {stdout}"
    );
    assert!(
        stderr.contains("clank feedback write"),
        "expected reviewer continuation prompt; got {stderr}"
    );
}

#[test]
fn auto_off_exits_silent() {
    let env = init_repo();
    let repo = env.repo();
    bind_alice(&env);
    // Don't turn auto on — alice's config has auto_mode=off by default
    // (or no config at all → treated as off).

    let stdin = claude_stdin(CLAUDE_SESSION, repo, false);
    let (code, stdout, stderr) = run_stop_hook(&env, "claude", &stdin, &[]);
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty(), "stdout={stdout}");
    assert!(stderr.is_empty(), "stderr={stderr}");
}

#[test]
fn no_agent_bound_emits_diagnostic() {
    let env = init_repo();
    let repo = env.repo();
    // No bind, no auto setup. Identity resolution will fail.

    let stdin = claude_stdin(CLAUDE_SESSION, repo, false);
    let (code, stdout, stderr) = run_stop_hook(&env, "claude", &stdin, &[]);
    // Hook NEVER fails the agent — even on identity failure.
    assert_eq!(code, Some(0));
    assert!(
        stdout.is_empty(),
        "expected no continuation; stdout={stdout}"
    );
    assert!(
        stderr.contains("no agent set up") || stderr.contains("clank as"),
        "expected bootstrap hint in stderr; got {stderr}"
    );
}

#[test]
fn malformed_stdin_emits_diagnostic() {
    let env = init_repo();
    let (code, stdout, stderr) = run_stop_hook(&env, "claude", "not json{", &[]);
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("parsing stdin"),
        "expected parse diag; got {stderr}"
    );
}

#[test]
fn hint_with_reviewable_work_emits_claude_continuation() {
    // Set up: master commits an intro plan → reviewers (alice)
    // have a reviewable commit they haven't acted on.
    let env = init_repo();
    let repo = env.repo();
    // Create the plan + intro commit BEFORE binding so it's part
    // of the repo state.
    std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
    std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    bind_alice(&env);
    turn_auto_on(&env);
    // alice has role=reviewers (no master designated). The intro
    // commit is reviewable; alice hasn't reviewed → wfw should
    // emit a Reviewer item for her.

    // We assert the emitted command is RUNNABLE — full SHA,
    // explicit --author. Short SHAs can collide; missing
    // --author hits the (still-required) CLI flag.
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(head.len(), 40, "expected full sha, got {head}");

    let stdin = claude_stdin(CLAUDE_SESSION, repo, false);
    let (code, stdout, stderr) = run_stop_hook(&env, "claude", &stdin, &[]);
    // Claude continuation = exit 2 + stderr = reason.
    assert_eq!(
        code,
        Some(2),
        "expected claude continuation; stderr={stderr}"
    );
    assert!(
        stdout.is_empty(),
        "claude continuation goes to stderr, not stdout: {stdout}"
    );
    let expected_cmd = format!(
        "clank feedback write --commit {head} \\\n        --author alice --verdict approve|finished|request-changes \\\n        -m"
    );
    assert!(
        stderr.contains(&expected_cmd),
        "stderr missing exact runnable command; expected to contain:\n{expected_cmd}\n\ngot:\n{stderr}"
    );
}

#[test]
fn hint_with_reviewable_work_emits_codex_continuation() {
    let env = init_repo();
    let repo = env.repo();
    std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
    std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    // Bind alice via codex this time.
    let out = run_clank(
        &env,
        &["as", "alice"],
        &[("CODEX_THREAD_ID", CODEX_SESSION)],
    );
    assert!(out.status.success());
    turn_auto_on_codex(&env);

    let stdin = format!(
        r#"{{
            "session_id": "{CODEX_SESSION}",
            "cwd": "{cwd}",
            "stop_hook_active": false
        }}"#,
        cwd = repo.display(),
    );
    let head = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    assert_eq!(head.len(), 40);

    let (code, stdout, stderr) = run_stop_hook(&env, "codex", &stdin, &[]);
    // Codex continuation = exit 0 + stdout JSON.
    assert_eq!(code, Some(0), "expected codex success; stderr={stderr}");
    let decision: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("valid JSON on stdout");
    assert_eq!(decision["decision"], "block");
    let reason = decision["reason"].as_str().expect("reason is string");
    let expected_cmd = format!(
        "clank feedback write --commit {head} \\\n        --author alice --verdict approve|finished|request-changes \\\n        -m"
    );
    assert!(
        reason.contains(&expected_cmd),
        "reason missing exact runnable command; expected to contain:\n{expected_cmd}\n\ngot:\n{reason}"
    );
}

fn turn_auto_on_codex(env: &common::TestEnv) {
    let out = run_clank(
        env,
        &["auto", "on", "--wfw-timeout", "10s"],
        &[("CODEX_THREAD_ID", CODEX_SESSION)],
    );
    assert!(
        out.status.success(),
        "auto on (codex) failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
