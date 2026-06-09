//! Integration tests for `status-blocks-dominate-gate` (Phase 6).
//!
//! Verifies that `clank status`'s per-plan output reflects the
//! Blocked gate state when a plan has an open block — `gate:
//! BLOCKED`, `waiting on: <creator>`, `reason: blocked: ...` —
//! and that unblocking restores the underlying review-driven gate.

mod common;

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

fn init_repo() -> common::TestEnv {
    let env = common::TestEnv::init();
    // Dogfood: set up the team through the real library cores
    // (declare agents → create team → set master → add members →
    // point repo at it), not hand-rolled JSON.
    env.register_team("claude", &["codex", "ruthless"], &[]);
    write(env.repo(), "README.md", "seed\n");
    git(env.repo(), &["add", "-A"]);
    git(env.repo(), &["commit", "--quiet", "-m", "seed"]);
    env
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

fn run(env: &common::TestEnv, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.args(args)
        .arg("--repo")
        .arg(env.repo())
        .env("HOME", env.home())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT");
    cmd.output().expect("spawn")
}

fn intro_plan(repo: &Path, stem: &str, body: &str) {
    write(repo, &format!(".clank/plans/{stem}.md"), body);
    git(repo, &["add", "-A"]);
    git(
        repo,
        &["commit", "--quiet", "-m", &format!("[{stem}] intro")],
    );
}

fn revise_plan(repo: &Path, stem: &str, body: &str) {
    write(repo, &format!(".clank/plans/{stem}.md"), body);
    git(repo, &["add", "-A"]);
    git(
        repo,
        &["commit", "--quiet", "-m", &format!("[{stem}] revise")],
    );
}

#[test]
fn status_shows_blocked_gate_and_creator_for_plan_block() {
    let env = init_repo();
    let repo = env.repo();
    intro_plan(repo, "foo", "# foo\n");
    revise_plan(repo, "foo", "# foo v2\n");

    // Create a plan-scoped block via the CLI.
    let block_out = run(
        &env,
        &[
            "block",
            "create",
            "--plan",
            "foo",
            "--author",
            "claude",
            "halt",
            "-m",
            "wait — checking the design",
        ],
    );
    assert!(
        block_out.status.success(),
        "block create failed: {}",
        String::from_utf8_lossy(&block_out.stderr)
    );

    let status_out = run(&env, &["status"]);
    assert!(status_out.status.success());
    let stdout = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        stdout.contains("gate:              BLOCKED"),
        "gate should show uppercase BLOCKED; got:\n{stdout}"
    );
    assert!(
        stdout.contains("waiting on:        claude"),
        "waiting on should be block creator (claude), not reviewers; got:\n{stdout}"
    );
    assert!(
        stdout.contains("reason:            blocked: wait — checking the design"),
        "reason should start with `blocked:` + first line of message; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("missing approval"),
        "must NOT show the review-gate reason when blocked; got:\n{stdout}"
    );
    // The footer audit list still includes the block.
    assert!(
        stdout.contains("BLOCKED (claude"),
        "blocks: footer should still list the entry; got:\n{stdout}"
    );
}

#[test]
fn status_blocked_plan_json_emits_blocked_gate_state_and_structured_waiting_on() {
    let env = init_repo();
    let repo = env.repo();
    intro_plan(repo, "foo", "# foo\n");
    revise_plan(repo, "foo", "# foo v2\n");
    run(
        &env,
        &[
            "block", "create", "--plan", "foo", "--author", "claude", "halt", "-m", "checking",
        ],
    );

    let out = run(&env, &["status", "--json"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("status --json parse: {e}\nstdout:\n{stdout}"));
    let plans = parsed["plans"].as_array().expect("plans array");
    assert_eq!(plans.len(), 1, "expected one active plan; got: {plans:?}");
    let p = &plans[0];
    assert_eq!(p["gate_state"], "blocked", "wire form is lowercase");
    // Structured serde — NOT a Debug string.
    let waiting_on = &p["waiting_on"];
    assert_eq!(waiting_on["kind"], "blocked");
    assert_eq!(waiting_on["block"]["creator"], "claude");
    assert_eq!(waiting_on["block"]["name"], "halt");
    assert_eq!(waiting_on["block"]["message"], "checking");
}

#[test]
fn status_unblocked_plan_returns_to_review_gate() {
    let env = init_repo();
    let repo = env.repo();
    intro_plan(repo, "foo", "# foo\n");
    revise_plan(repo, "foo", "# foo v2\n");
    run(
        &env,
        &[
            "block", "create", "--plan", "foo", "--author", "claude", "halt", "-m", "checking",
        ],
    );
    // Block must answer back via `clank unblock <agent> <name> --plan ... -m ...`.
    let unblock = run(
        &env,
        &[
            "unblock", "claude", "halt", "--plan", "foo", "-m", "resolved",
        ],
    );
    assert!(
        unblock.status.success(),
        "unblock failed: {}",
        String::from_utf8_lossy(&unblock.stderr)
    );

    let out = run(&env, &["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // After unblock, the per-plan gate line must NOT be the
    // blocked form. The footer may still say `UNBLOCKED ...`
    // (that's just the audit history), so we check the specific
    // `gate:              BLOCKED` form rather than just `BLOCKED`.
    assert!(
        !stdout.contains("gate:              BLOCKED"),
        "after unblock, gate should NOT be BLOCKED; got:\n{stdout}"
    );
    assert!(
        stdout.contains("gate:              unreviewed"),
        "after unblock, gate should be unreviewed (no reviews posted); got:\n{stdout}"
    );
    assert!(
        stdout.contains("missing approval"),
        "after unblock, reason should be the review-gate text; got:\n{stdout}"
    );
}

#[test]
fn status_two_pending_blocks_on_same_plan_picks_first_lex() {
    // Phase 2 tie-breaker pin: lex-first by (creator, name).
    let env = init_repo();
    let repo = env.repo();
    intro_plan(repo, "foo", "# foo\n");
    revise_plan(repo, "foo", "# foo v2\n");
    // Two blocks on the same plan. Author them in non-lex order
    // so the test fails if `derive_status` accidentally took
    // file-system iteration order.
    run(
        &env,
        &[
            "block", "create", "--plan", "foo", "--author", "claude", "zebra", "-m", "z first",
        ],
    );
    run(
        &env,
        &[
            "block",
            "create",
            "--plan",
            "foo",
            "--author",
            "claude",
            "alpha",
            "-m",
            "a is lex-first",
        ],
    );

    let out = run(&env, &["status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // `alpha` sorts before `zebra` for the same creator.
    assert!(
        stdout.contains("reason:            blocked: a is lex-first"),
        "lex-first block (alpha) should surface as reason; got:\n{stdout}"
    );
}
