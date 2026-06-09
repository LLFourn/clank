//! Integration tests for `clank finish --amend` on already-finished plans.

mod common;

use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

/// A bare git repo with a separate HOME, NO team registered.
/// Callers register the team they need via `env.register_team`
/// (most want claude-only; one wants a codex reviewer). Keeping
/// init team-less avoids re-registering an existing team.
fn init_repo() -> common::TestEnv {
    common::TestEnv::init()
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn run_clank(env: &common::TestEnv, args: &[&str]) -> std::process::Output {
    env.clank()
        .args(args)
        .arg("--repo")
        .arg(env.repo())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank")
}

/// Set up a repo (claude master team) where plan `foo` is already
/// in `.clank/finished/`. HEAD is a synthetic finalize commit
/// ([foo] finish).
fn repo_with_finished_foo() -> common::TestEnv {
    let env = init_repo();
    env.register_team("claude", &[], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    write(repo, ".clank/finished/foo.md", "# foo\n");
    git(repo, &["rm", "--quiet", ".clank/plans/foo.md"]);
    git(repo, &["add", ".clank/finished/foo.md"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] finish"]);
    env
}

#[test]
fn finish_amend_dry_purge_does_not_mutate_head_when_already_finished() {
    let env = repo_with_finished_foo();
    let repo = env.repo();
    let before = head_sha(repo);

    let out = run_clank(&env, &["finish", "--amend", "--dry", "--purge", "foo"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "clank finish failed: {stderr}");

    let after = head_sha(repo);
    assert_eq!(before, after, "--dry must not move HEAD");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("would amend HEAD finalize commit"),
        "expected amend-aware dry preview; got:\n{stdout}"
    );
}

#[test]
fn finish_fails_closed_on_malformed_reviewer_config() {
    // Regression: a corrupted repo config must NOT silently
    // behave like a zero-reviewer repo (which would auto-Approve
    // and let master finalize without any review). The team
    // resolver is strict; finalize fails closed.
    //
    // Under teams-based-agent-registration the repo's team lives
    // in `<repo>/.clank/config.json` (the `team` field, resolved
    // against user-scope `agents`/`teams`). A malformed file must
    // surface as a parse error, not degrade to "no reviewers".
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    // Malformed JSON in the repo-scope agents declaration — would
    // silently drop reviewers under lossy loading.
    write(repo, ".clank/config.json", "{ this is not valid JSON");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    let out = run_clank(&env, &["finish", "foo"]);
    assert!(
        !out.status.success(),
        "clank finish must fail closed when the declaration is malformed; got stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.to_lowercase().contains("config")
            || combined.to_lowercase().contains("agent")
            || combined.to_lowercase().contains("json")
            || combined.to_lowercase().contains("parsing"),
        "expected error to mention the broken config; got: {combined}"
    );
}

#[test]
fn finish_rejects_approve_without_finished() {
    // Plan intro is APPROVED but not FINISHED. clank finish must
    // refuse — only a FINISHED verdict unlocks finalize when there
    // is at least one registered reviewer.
    let env = init_repo();
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    // Register a master + codex reviewer via the repo-scope team
    // (`teams-based-agent-registration`).
    env.register_team("claude", &["codex"], &[]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let intro_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{intro_sha}.md"),
        "APPROVE\n\nplan looks good\n",
    );
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "approve plan"]);

    let out = run_clank(&env, &["finish", "foo"]);
    assert!(
        !out.status.success(),
        "clank finish should refuse APPROVE-without-FINISHED; got stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("FINISHED"),
        "expected error to mention FINISHED; got: {stderr}"
    );
}

#[test]
fn finish_amend_no_dry_rewrites_head_message_when_already_finished() {
    let env = repo_with_finished_foo();
    let repo = env.repo();
    let before_sha = head_sha(repo);
    let finished_path = repo.join(".clank/finished/foo.md");
    let before_body = std::fs::read_to_string(&finished_path).unwrap();

    let out = run_clank(
        &env,
        &["finish", "--amend", "-m", "[foo] finish (refreshed)", "foo"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "clank finish failed: {stderr}");

    let after_sha = head_sha(repo);
    assert_ne!(before_sha, after_sha, "amend should rewrite HEAD");

    let subject = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "-1", "--format=%s"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(subject.stdout).unwrap().trim(),
        "[foo] finish (refreshed)"
    );

    let after_body = std::fs::read_to_string(&finished_path).unwrap();
    assert_eq!(before_body, after_body, "finished file must not be touched");
}
