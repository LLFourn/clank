//! A pending `clank block` pauses the REVIEW CYCLE, not the agent.
//!
//! A block skipped `items_for` wholesale, and `message_items` is
//! reached only through it — so a human asking a blocked agent a
//! question got silence until the block was answered, including a
//! message that said what the answer was. External events were never
//! affected: they arrive on the parked wait's channel, which no block
//! touched. What this fixes is the class that did.
//!
//! Tested at BOTH routes. The peek is where an empty result becomes
//! "nothing to deliver", and the park is where the channel merges in;
//! a test of either alone leaves the other free to regress.

use crate::common;
use common::TestEnv;
use std::path::Path;
use std::process::Command;

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

fn args_for(repo: &Path, author: &str, peek: bool, events: Vec<String>) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some(author.into()),
        die_with_owner: false,
        deliver_to_session: None,
        turn_end: false,
        json: true,
        events,
        r#for: None,
        peek,
        no_cache: false,
        poll: true,
        no_poll: false,
    }
}

async fn peek(repo: &Path, author: &str) -> Vec<WaitItem> {
    clank::cli::wait::items(args_for(repo, author, true, Vec::new()))
        .await
        .unwrap()
        .expect("a peek reports its items")
}

/// A command source, declared the way an agent's config declares one.
fn cmd_event(name: &str, argv: &[&str]) -> String {
    serde_json::json!({ "kind": "command", "name": name, "command": argv }).to_string()
}

/// A team with nothing to do, so every item a test sees is one it put
/// there.
fn idle_env() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);
    env
}

/// A question for a human, as `clank block` leaves one.
fn block(repo: &Path, agent: &str, name: &str, question: &str) {
    write(
        repo,
        &format!(".clank/agents/{agent}/blocks/{name}.md"),
        question,
    );
}

fn send(repo: &Path, to: &str, body: &str) {
    clank::inbox::deliver(
        repo,
        &AgentLabel::parse(to).unwrap(),
        None,
        "2026-09-30T10:15:00Z",
        body,
    )
    .unwrap();
}

fn kinds(items: &[WaitItem]) -> Vec<&'static str> {
    items
        .iter()
        .map(|i| match i {
            WaitItem::Message { .. } => "message",
            WaitItem::Blocked { .. } => "blocked",
            WaitItem::Unblocked { .. } => "unblocked",
            WaitItem::PromoteFromQueue { .. } => "promote",
            WaitItem::Reviewer { .. } => "reviewer",
            WaitItem::Master { .. } => "master",
            _ => "other",
        })
        .collect()
}

/// The regression. A message reaches a blocked agent, and the block it
/// is blocked on comes with it as context.
#[tokio::test]
async fn a_blocked_peek_still_delivers_a_message() {
    let env = idle_env();
    let repo = env.repo();
    send(repo, "master", "the answer is yes");
    block(repo, "master", "q", "should I?");

    let items = peek(repo, "master").await;

    assert!(
        kinds(&items).contains(&"message"),
        "a message must survive a block — it is the most likely carrier of the \
         answer: {items:?}"
    );
    assert!(
        kinds(&items).contains(&"blocked"),
        "and the block rides along, so the agent knows why its plan work went \
         quiet: {items:?}"
    );
}

/// The other half, and the one that must NOT change: the cycle the
/// block exists to pause stays paused.
///
/// `PromoteFromQueue` is the review-cycle item an idle master produces,
/// so a queue entry plus a block is the smallest case that can tell
/// "delivers events" from "delivers everything".
#[tokio::test]
async fn a_blocked_peek_withholds_the_review_cycle() {
    let env = idle_env();
    let repo = env.repo();
    write(repo, ".clank/queue/500-next-plan.md", "# next-plan\n");
    // Unblocked first: the queue item IS otherwise delivered, so its
    // absence below is suppression and not an empty queue.
    assert!(
        kinds(&peek(repo, "master").await).contains(&"promote"),
        "the queue item must be deliverable in the first place"
    );

    send(repo, "master", "the answer is yes");
    block(repo, "master", "q", "should I?");
    let items = peek(repo, "master").await;

    assert!(
        kinds(&items).contains(&"message"),
        "the event still arrives: {items:?}"
    );
    assert!(
        !kinds(&items).contains(&"promote"),
        "starting a new plan is exactly what a pending question stops: {items:?}"
    );
}

/// A block alone wakes NOTHING.
///
/// The `Blocked` item is context, never a wake of its own: a block
/// pends until a human answers it, so delivering it on its own would
/// wake the agent at every turn end for as long as it pends — a
/// runaway built out of the fix.
#[tokio::test]
async fn a_block_with_no_events_wakes_nothing() {
    let env = idle_env();
    let repo = env.repo();
    write(repo, ".clank/queue/500-next-plan.md", "# next-plan\n");
    block(repo, "master", "q", "should I?");

    let items = peek(repo, "master").await;

    assert!(
        items.is_empty(),
        "nothing to deliver means nothing, not a bare Blocked: {items:?}"
    );
}

/// The answer LEADS, and the events it arrived beside are not dropped.
///
/// An earlier version of this delivered the answer alone. Nothing is
/// gained by that and something is lost: a source running memory-only
/// has no WAL to re-read its item from, so "alone" meant discarded.
#[tokio::test]
async fn an_answered_block_leads_with_the_answer() {
    let env = idle_env();
    let repo = env.repo();
    send(repo, "master", "unrelated");
    block(repo, "master", "q", "should I?");
    // The answer file, where the scanner looks for it: a `<name>.md`
    // under `unblocks/` beside the `blocks/` entry it answers.
    write(repo, ".clank/agents/master/unblocks/q.md", "yes");

    let items = peek(repo, "master").await;

    assert_eq!(
        kinds(&items),
        vec!["unblocked", "message"],
        "the answer first, so nothing buries it; the message still here: {items:?}"
    );
}

/// The CHANNEL route obeys the block decision — in the watcher loop,
/// not only in the helper.
///
/// A parked wait has a second way to receive an external item: the
/// source sends it down a channel as it appends. That drain used to
/// happen AFTER the block decision and its result was appended raw, so
/// a blocked park handed back the event with no `Blocked` beside it
/// (codex on 344280f). The helper composition is tested in `wait.rs`;
/// only a parked wait proves the LOOP composes it that way — a unit
/// test of the helper passes against a loop that calls it and then
/// appends around it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_source_item_reaching_a_blocked_park_arrives_with_its_context() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    block(repo, "rev", "q", "should I?");
    // Emitted after the initial pass has already read the projection,
    // so the item can only arrive on the channel.
    let ev = cmd_event("ticker", &["sh", "-c", "sleep 0.5; echo hi"]);

    let items = tokio::time::timeout(
        common::race_deadline(repo),
        clank::cli::wait::items(args_for(repo, "rev", false, vec![ev])),
    )
    .await
    .expect("the source's line must wake the parked wait")
    .unwrap()
    .expect("a wait reports its items");

    let seen = kinds(&items);
    assert!(
        seen.contains(&"other"),
        "the command event is what woke it: {items:?}"
    );
    assert!(
        seen.contains(&"blocked"),
        "and it must not bypass the block decision that explains the \
         surrounding silence: {items:?}"
    );
    // ONE copy: the channel is the only route an external row takes,
    // so a second would mean the beat appended it twice.
    assert_eq!(
        seen.iter().filter(|k| **k != "blocked").count(),
        1,
        "one row, however many routes carried it: {items:?}"
    );
}
