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
        deliver_to_session: None,
        turn_end: false,
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

/// A message sent the way `clank send` sends one: logged as an event.
fn send(repo: &Path, to: &str, body: &str) -> String {
    clank::inbox::deliver(repo, &label(to), None, "2026-09-22T10:15:00Z", body).unwrap()
}

/// A message dropped into the OLD file inbox, as one sent before this
/// existed would be. Migration is what brings it into the log.
fn legacy_file(repo: &Path, to: &str, body: &str) -> std::path::PathBuf {
    let dir = clank::inbox::inbox_dir(repo, &label(to));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("500-00000001-legacy.md");
    std::fs::write(
        &file,
        format!("---\nsent: 2026-09-22T10:15:00Z\n---\n{body}\n"),
    )
    .unwrap();
    file
}

/// The selector a message's wake names — what `clank events ack`
/// takes, and the only handle an agent is given.
fn selector_of(items: &[WaitItem], subject: &str) -> String {
    items
        .iter()
        .find_map(|i| match i {
            WaitItem::Message {
                subject: s,
                selector,
                ..
            } if s == subject => selector.clone(),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no message {subject:?} naming a selector in {items:?}"))
}

/// Handle a message the way an agent is told to: run the ack command
/// with the selector its wake gave it. Not a test-only shortcut --
/// this is the contract, and a selector the command refuses is a wake
/// that cannot be discharged.
async fn ack(repo: &Path, to: &str, selector: &str) {
    clank::cli::events::run(clank::cli::EventsArgs {
        command: clank::cli::EventsCmd::Ack(clank::cli::EventsAckArgs {
            reply: None,
            repo: Some(repo.to_path_buf()),
            author: Some(to.to_string()),
            ids: vec![selector.to_string()],
        }),
    })
    .await
    .expect("the ack command accepts the selector the wake named");
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
async fn a_message_keeps_waking_until_it_is_acked() {
    let env = idle_env();
    let repo = env.repo();
    send(repo, "rev", "rebase please");

    // Reading is not handling: it comes back every time.
    let items = peek(repo, "rev").await;
    assert_eq!(subjects(&items), ["rebase please"]);
    assert_eq!(subjects(&peek(repo, "rev").await), ["rebase please"]);
    ack(repo, "rev", &selector_of(&items, "rebase please")).await;
    assert!(peek(repo, "rev").await.is_empty());
}

/// A message sent BEFORE this existed still wakes its agent: the first
/// read migrates it into the log, and from there it behaves like any
/// other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_legacy_file_still_wakes_and_is_acked_like_an_event() {
    let env = idle_env();
    let repo = env.repo();
    legacy_file(repo, "rev", "from before");

    let items = peek(repo, "rev").await;
    assert_eq!(subjects(&items), ["from before"]);
    assert_eq!(
        subjects(&peek(repo, "rev").await),
        ["from before"],
        "still owed until it is acked",
    );
    // Through the SAME command, with the selector its wake named: a
    // migrated message is not a second kind of thing.
    ack(repo, "rev", &selector_of(&items, "from before")).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_waits_can_park_and_both_see_the_same_pending_message() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    if let Some((tool, session)) = clank::agent_env::detect_session_from_env().unwrap() {
        clank::agent_store::bind_session_to_agent(repo, &label("rev"), tool, &session).unwrap();
    }
    let first = tokio::spawn(clank::cli::wait::items(wait_args(repo, "rev", false)));
    let second = tokio::spawn(clank::cli::wait::items(wait_args(repo, "rev", false)));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    send(repo, "rev", "wake both");
    for waiter in [first, second] {
        let items = tokio::time::timeout(common::race_deadline(repo), waiter)
            .await
            .expect("both waits must wake")
            .expect("join")
            .expect("wait succeeds")
            .expect("items");
        assert_eq!(subjects(&items), ["wake both"]);
    }
}
