//! Integration tests for `clank wfw` resolving `--author` and
//! `--role` automatically when omitted.
//!
//! Bootstraps an agent via `clank as`, optionally claims the
//! master role via `clank auto`, then runs `clank wfw` with no
//! flags. We only care that wfw RESOLVES the right identity +
//! role and starts watching — we don't wait for work; passing
//! `--timeout 1s` makes it exit cleanly without producing any
//! items.

mod common;

fn init_repo() -> common::TestEnv {
    common::TestEnv::init()
}

const CLAUDE_SESSION: &str = "742f6a04-f174-409a-ab01-419a16c5f372";

fn run_clank(
    env: &common::TestEnv,
    args: &[&str],
    extra_env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = env.clank();
    cmd.args(args)
        .arg("--repo")
        .arg(env.repo())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn clank")
}

fn bind_alice(env: &common::TestEnv) {
    let out = run_clank(
        env,
        &["as", "alice"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(
        out.status.success(),
        "clank as alice failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// wfw expected exit code 2 for the empty-work timeout path.
const WFW_TIMEOUT_EXIT: i32 = 2;

#[test]
fn wfw_resolves_author_from_session_when_omitted() {
    let env = init_repo();
    // Team: alice is a commit reviewer (boss is master). wfw
    // resolves alice from her session binding.
    env.register_team("boss", &["alice"], &[]);
    bind_alice(&env);

    // No --author / --role passed. Should resolve alice + reviewers.
    // With no work in the repo it'll time out cleanly.
    let out = run_clank(
        &env,
        &["wfw", "--no-poll", "--timeout", "1s"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert_eq!(
        out.status.code(),
        Some(WFW_TIMEOUT_EXIT),
        "expected timeout exit, got status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn wfw_resolves_role_master_from_repo_config() {
    let env = init_repo();
    // alice is the team master (role is team-derived now;
    // `teams-based-agent-registration`). wfw resolves her role
    // as master from the team, with no explicit `--role`.
    env.register_team("alice", &[], &[]);
    bind_alice(&env);

    // Add a queue item so master returns PromoteFromQueue (exit 0).
    // A reviewer would timeout (exit 2), confirming the resolved
    // role was master.
    let queue_dir = env.repo().join(".clank/queue");
    std::fs::create_dir_all(&queue_dir).unwrap();
    std::fs::write(queue_dir.join("500-test.md"), "# test\n").unwrap();

    let out = run_clank(
        &env,
        &["wfw", "--no-poll", "--timeout", "1s", "--json"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(
        out.status.success(),
        "expected master promote (exit 0); got status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("promote_from_queue"),
        "expected promote_from_queue; got: {}",
        String::from_utf8_lossy(&out.stdout),
    );
}

#[test]
fn wfw_explicit_author_overrides_resolver() {
    let env = init_repo();
    env.register_team("boss", &["alice"], &[]);
    bind_alice(&env);

    // Pass --author bob (a label with no binding) — explicit
    // flag wins. Should still succeed: identity is given, no
    // resolver call.
    let out = run_clank(
        &env,
        &[
            "wfw",
            "--no-poll",
            "--timeout",
            "1s",
            "--author",
            "bob",
            "--role",
            "reviewers",
        ],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert_eq!(out.status.code(), Some(WFW_TIMEOUT_EXIT));
}

#[test]
fn wfw_errors_when_no_author_resolvable() {
    let env = init_repo();
    // No `clank as` and no --author flag. Resolver should refuse
    // with the bootstrap hint.
    let out = run_clank(
        &env,
        &["wfw", "--no-poll", "--timeout", "1s"],
        &[("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION)],
    );
    assert!(!out.status.success() && out.status.code() != Some(WFW_TIMEOUT_EXIT));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no agent is bound") || stderr.contains("clank as"),
        "stderr missing bootstrap hint: {stderr}"
    );
}
