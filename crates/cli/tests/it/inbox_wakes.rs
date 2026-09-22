//! A pending message is part of what a wait wakes with on every path —
//! `--peek` and the first pass, a refold, with repo work or without —
//! and it keeps waking until it is handled
//! (a-message-wakes-its-agent-until-it-is-handled). All in-process.

use crate::common;
use common::TestEnv;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use clank::cli::WaitArgs;
use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

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

fn wait_args(repo: &Path, author: &str, peek: bool) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some(author.into()),
        die_with_owner: false,
        json: true,
        events: Vec::new(),
        r#for: None,
        peek,
        no_cache: false,
        poll: true,
        no_poll: false,
    }
}

fn label(s: &str) -> AgentLabel {
    AgentLabel::parse(s).unwrap()
}

fn send(repo: &Path, to: &str, body: &str) -> std::path::PathBuf {
    clank::inbox::send(repo, &label(to), 500, None, "2026-09-22T10:15:00Z", body).unwrap()
}

/// A team with nothing to do: `master` has no plans or queue, `rev`
/// has nothing to review.
fn idle_env() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);
    env
}

async fn peek(repo: &Path, author: &str) -> Vec<WaitItem> {
    clank::cli::wait::items(wait_args(repo, author, true))
        .await
        .unwrap()
        .expect("a wait reports its items")
}

fn subjects(items: &[WaitItem]) -> Vec<&str> {
    items
        .iter()
        .filter_map(|i| match i {
            WaitItem::Message { subject, .. } => Some(subject.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peek_carries_a_pending_message() {
    let env = idle_env();
    let repo = env.repo();
    send(repo, "rev", "rebase please");
    assert_eq!(subjects(&peek(repo, "rev").await), ["rebase please"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_rides_with_repo_work() {
    let env = idle_env();
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    send(repo, "rev", "also this");

    let items = peek(repo, "rev").await;

    assert!(
        items.iter().any(|i| matches!(i, WaitItem::Reviewer { .. })),
        "the review is still there: {items:?}"
    );
    assert_eq!(subjects(&items), ["also this"], "{items:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_does_not_hide_the_queue_from_the_master() {
    let env = idle_env();
    let repo = env.repo();
    write(repo, ".clank/queue/500-someplan.md", "# someplan\n");
    send(repo, "master", "hello");

    let items = peek(repo, "master").await;

    assert!(
        items
            .iter()
            .any(|i| matches!(i, WaitItem::PromoteFromQueue { name, .. } if name == "someplan")),
        "{items:?}"
    );
    assert_eq!(subjects(&items), ["hello"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_keeps_waking_until_it_is_handled() {
    let env = idle_env();
    let repo = env.repo();
    let path = send(repo, "rev", "rebase please");
    let handle = path.file_stem().unwrap().to_string_lossy().into_owned();

    assert_eq!(subjects(&peek(repo, "rev").await), ["rebase please"]);
    assert_eq!(subjects(&peek(repo, "rev").await), ["rebase please"]);
    clank::inbox::done(repo, &label("rev"), &handle).unwrap();
    assert!(peek(repo, "rev").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_landing_message_wakes_a_parked_wait() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    common::assert_stays_parked(
        repo,
        wait_args(repo, "rev", false),
        "rev with nothing to do",
    )
    .await;

    let waiter = tokio::spawn(clank::cli::wait::items(wait_args(repo, "rev", false)));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    send(repo, "rev", "wake up");

    let items = tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("a message landing must wake the parked wait")
        .expect("join")
        .expect("the wait returns")
        .expect("a wait reports its items");
    assert_eq!(subjects(&items), ["wake up"]);
}
