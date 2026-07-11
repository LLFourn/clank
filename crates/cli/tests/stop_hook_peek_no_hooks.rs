//! `clank wait --peek` must be side-effect-free: it reports work-presence
//! without firing any lifecycle hook. `run_hook` (immediate work items) and
//! `run_idle_hook` (master no-work) are the ONLY lifecycle firings in
//! wait's initial pass (verified 2026-07-01), so covering BOTH — a reviewer
//! with a commit to review (`run_hook`) and an idle master
//! (`run_idle_hook`) — exercises the guard completely.

mod common;
use common::TestEnv;

use std::path::Path;
use std::process::Command;

use clank::cli::{WaitArgs, WaitRole};

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed"
    );
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

/// Set a single `hooks.<event>` command in the repo config, preserving the
/// roster written by `register_team`.
fn set_hook(repo: &Path, event: &str, cmd: &str) {
    let cfg_path = repo.join(".clank/config.json");
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    cfg["hooks"] = serde_json::json!({ event: cmd });
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();
}

fn wait_args(repo: &Path, author: &str, role: WaitRole, peek: bool, timeout: &str) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some(author.into()),
        role: Some(role),
        timeout: timeout.into(),
        json: true,
        events: Vec::new(),
        r#for: None,
        peek,
        no_cache: false,
        poll: false,
        no_poll: false,
    }
}

/// A reviewer with a commit to review: a real (non-peek) wait fires the
/// `reviewer_work` hook on the immediate item; `--peek` must not.
#[tokio::test]
async fn peek_does_not_fire_the_work_hook() {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();

    let marker = repo.join("work_fired.marker");
    set_hook(
        repo,
        "reviewer_work",
        &format!("touch '{}'", marker.display()),
    );

    // A plan intro commit → the commit reviewer `rev` has a review item.
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    // Work IS present, so both calls return immediately (no loop entered).
    clank::cli::wait::run(wait_args(repo, "rev", WaitRole::Reviewer, false, "0"))
        .await
        .expect("non-peek returns immediately when work is present");
    assert!(
        marker.exists(),
        "sanity: a non-peek reviewer wait fires the reviewer_work hook"
    );
    std::fs::remove_file(&marker).unwrap();

    clank::cli::wait::run(wait_args(repo, "rev", WaitRole::Reviewer, true, "0"))
        .await
        .expect("peek returns immediately");
    assert!(
        !marker.exists(),
        "peek must be side-effect-free: the reviewer_work hook must not fire"
    );
}

/// An idle master reaches `run_idle_hook`: a real (non-peek) wait fires it;
/// `--peek` must not (and must return instead of parking).
#[tokio::test]
async fn peek_does_not_fire_the_idle_hook() {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();

    let marker = repo.join("idle_fired.marker");
    set_hook(repo, "idle", &format!("touch '{}'", marker.display()));

    // Commit the scaffold so the fold has a HEAD and zero plans (idle master).
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);

    // Baseline: a non-peek idle wait fires the idle hook (before it parks);
    // a short timeout keeps the test from blocking.
    let _ = clank::cli::wait::run(wait_args(repo, "master", WaitRole::Master, false, "1s")).await;
    assert!(
        marker.exists(),
        "sanity: a non-peek idle wait fires the idle hook"
    );
    std::fs::remove_file(&marker).unwrap();

    // Peek: same idle master, must NOT fire the idle hook, and must return.
    clank::cli::wait::run(wait_args(repo, "master", WaitRole::Master, true, "0"))
        .await
        .expect("peek returns immediately");
    assert!(
        !marker.exists(),
        "peek must be side-effect-free: the idle hook must not fire"
    );
}
