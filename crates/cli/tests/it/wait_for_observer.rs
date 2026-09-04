//! `clank wait --for` (wait-for-observer-mode): a pure cross-repo
//! observer. No session binding or team is registered in ANY of these
//! fixtures — the observer path must never resolve identity. Poll mode
//! keeps the loop deterministic (500ms tick, no FSEvents dependency),
//! and everything runs in-process via `clank::cli::wait::run`.

use crate::common;
use common::TestEnv;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use clank::cli::{WaitArgs, WaitFor};

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

fn observer_args(repo: &Path, event: WaitFor) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        // The observer never resolves identity: author/role stay None
        // and no session env or binding exists in the fixture.
        author: None,
        die_with_owner: false,
        json: true,
        events: Vec::new(),
        r#for: Some(event),
        peek: false,
        no_cache: false,
        poll: true,
        no_poll: false,
    }
}

/// A pre-existing state is BASELINE, not an event: the observer must
/// stay parked. Sampled rather than timed out (remove-wait-timeout),
/// over a window that outlasts setup.
async fn assert_ignores_preexisting(repo: &Path, event: WaitFor, what: &str) {
    common::assert_stays_parked(repo, observer_args(repo, event), what).await;
}

/// Race the observer against a mutation applied shortly after it
/// starts; the observer must fire (exit Ok) well before its own
/// generous timeout.
async fn fires_after(repo: &Path, event: WaitFor, mutate: impl FnOnce(&Path)) {
    let observer = tokio::spawn(clank::cli::wait::run(observer_args(repo, event)));
    // Give the observer time to capture its baseline and attach.
    tokio::time::sleep(Duration::from_millis(800)).await;
    mutate(repo);
    tokio::time::timeout(common::race_deadline(repo), observer)
        .await
        .expect("observer must fire before the outer deadline")
        .expect("join")
        .expect("observer exits Ok when the event lands");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn for_commit_fires_on_a_new_commit_only() {
    let _serial = common::serial();
    let env = TestEnv::init();
    let repo = env.repo();
    write(repo, "f.txt", "one");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    // Delta semantics: the existing HEAD is the baseline, not an event.
    assert_ignores_preexisting(
        repo,
        WaitFor::Commit,
        "no new commit → observer parks until timeout",
    )
    .await;

    fires_after(repo, WaitFor::Commit, |repo| {
        write(repo, "f.txt", "two");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "the event"]);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn for_finished_ignores_preexisting_and_fires_on_a_new_finalize() {
    let _serial = common::serial();
    let env = TestEnv::init();
    let repo = env.repo();
    // A plan finished BEFORE the observer starts…
    write(repo, ".clank/plans/old.md", "# old\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[old] intro"]);
    std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
    std::fs::rename(
        repo.join(".clank/plans/old.md"),
        repo.join(".clank/finished/old.md"),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[old] finish"]);
    // …and one still active.
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    assert_ignores_preexisting(
        repo,
        WaitFor::Finished,
        "pre-existing finishes are baseline, not events",
    )
    .await;

    fires_after(repo, WaitFor::Finished, |repo| {
        std::fs::rename(
            repo.join(".clank/plans/foo.md"),
            repo.join(".clank/finished/foo.md"),
        )
        .unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "[foo] finish"]);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn for_stopped_fires_on_a_new_block_and_ignores_preexisting() {
    let _serial = common::serial();
    let env = TestEnv::init();
    let repo = env.repo();
    write(repo, "f.txt", "seed");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);
    // An already-pending block is baseline, not an event.
    write(repo, ".clank/agents/claude/blocks/pre.md", "old question\n");

    assert_ignores_preexisting(repo, WaitFor::Stopped, "pre-existing block is baseline").await;

    fires_after(repo, WaitFor::Stopped, |repo| {
        write(repo, ".clank/agents/claude/blocks/new-q.md", "which way?\n");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn for_stopped_also_fires_on_a_finalize() {
    let _serial = common::serial();
    let env = TestEnv::init();
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    fires_after(repo, WaitFor::Stopped, |repo| {
        std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
        std::fs::rename(
            repo.join(".clank/plans/foo.md"),
            repo.join(".clank/finished/foo.md"),
        )
        .unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "--quiet", "-m", "[foo] finish"]);
    })
    .await;
}
