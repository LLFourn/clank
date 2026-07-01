//! `clank wait --peek` must be side-effect-free: it reports work-presence
//! without firing any lifecycle hook. `run_hook` (immediate items) and
//! `run_idle_hook` (master no-work) are the ONLY lifecycle firings in
//! wait's initial pass (verified 2026-07-01), so an idle master — which
//! reaches `run_idle_hook` — exercises the guard end to end.

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

fn wait_args(repo: &Path, peek: bool, timeout: &str) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some("master".into()),
        role: Some(WaitRole::Master),
        timeout: timeout.into(),
        json: true,
        peek,
        no_cache: false,
        poll: false,
        no_poll: false,
    }
}

#[tokio::test]
async fn peek_does_not_fire_lifecycle_hooks() {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();

    // An idle hook that touches a marker when it fires.
    let marker = repo.join("idle_fired.marker");
    let cfg_path = repo.join(".clank/config.json");
    let mut cfg: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    cfg["hooks"] = serde_json::json!({ "idle": format!("touch '{}'", marker.display()) });
    std::fs::write(&cfg_path, serde_json::to_string_pretty(&cfg).unwrap()).unwrap();

    // Commit the scaffold so the fold has a HEAD and zero plans (idle master).
    std::fs::write(
        repo.join(".clank/.gitignore"),
        "/cache/\n/agents/\n/shelved/\n",
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);

    // Baseline: a real (non-peek) wait on an idle master DOES fire the idle
    // hook (before it parks). A short timeout keeps the test from blocking.
    let _ = clank::cli::wait::run(wait_args(repo, false, "1s")).await;
    assert!(
        marker.exists(),
        "sanity: a non-peek idle wait fires the idle hook"
    );
    std::fs::remove_file(&marker).unwrap();

    // Peek: same idle master. It must NOT fire the idle hook, and must
    // return immediately (never enter the watcher loop).
    clank::cli::wait::run(wait_args(repo, true, "0"))
        .await
        .expect("peek returns immediately");
    assert!(
        !marker.exists(),
        "peek must be side-effect-free: the idle hook must not fire"
    );
}
