//! A standby agent is woken for everything but reviews
//! (a-standby-agent-wakes-only-for-messages): no plan or ad-hoc review
//! reaches it, while its messages and its own wake sources do, and a
//! reviewer switched to standby mid-wait stops being owed reviews. All
//! in-process.

use crate::common;
use common::TestEnv;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use clank::cli::WaitArgs;
use clank::cli::teams_config::{AgentDescription, ReviewKind, RosterRole};
use clank_core::ids::AgentLabel;
use clank_core::vocab::Tool;
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

fn label(s: &str) -> AgentLabel {
    AgentLabel::parse(s).unwrap()
}

fn wait_args(repo: &Path, author: &str, peek: bool, events: Vec<String>) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some(author.into()),
        die_with_owner: false,
        deliver_to_session: None,
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
    clank::cli::wait::items(wait_args(repo, author, true, Vec::new()))
        .await
        .unwrap()
        .expect("a wait reports its items")
}

/// `claude` masters, `codex` reviews every commit, `sb` stands by.
/// Ad-hoc review is on, so an untagged commit is owed to reviewers.
async fn team_with_standby() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    clank::cli::agent::add_repo_roster_agent(
        repo,
        &label("sb"),
        AgentDescription {
            tool: Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
        RosterRole::Standby,
    )
    .unwrap();
    use clap::Parser;
    let set = clank::cli::command::Cli::try_parse_from([
        "clank",
        "config",
        "--repo",
        repo.to_str().unwrap(),
        "review.adhoc_feedback",
        "set",
        "true",
    ])
    .unwrap();
    clank::cli::command::dispatch(set).await.unwrap();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);
    env
}

fn is_review(i: &WaitItem) -> bool {
    matches!(
        i,
        WaitItem::Reviewer { .. } | WaitItem::AdHocReview { .. } | WaitItem::PrReviewer { .. }
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plan_commit_is_reviewed_by_the_reviewer_and_not_the_standby_agent() {
    let env = team_with_standby().await;
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);

    assert!(peek(repo, "codex").await.iter().any(is_review));
    assert!(peek(repo, "sb").await.is_empty());
}

/// The case a tierless reviewer gets wrong: an Unreviewed ad-hoc
/// commit is owed to every reviewer by role alone. Ad-hoc commits
/// count only once a plan has been introduced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_adhoc_commit_is_owed_to_reviewers_and_not_to_the_standby_agent() {
    let env = team_with_standby().await;
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    write(repo, "notes.txt", "a tweak\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "tweak"]);

    assert!(
        peek(repo, "codex")
            .await
            .iter()
            .any(|i| matches!(i, WaitItem::AdHocReview { .. }))
    );
    assert!(peek(repo, "sb").await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_agents_messages_and_own_sources_still_wake_it() {
    let _serial = common::serial();
    let env = team_with_standby().await;
    let repo = env.repo();

    clank::inbox::deliver(repo, &label("sb"), None, "2026-09-22T10:15:00Z", "hello").unwrap();
    let items = peek(repo, "sb").await;
    let selector = items
        .iter()
        .find_map(|i| match i {
            WaitItem::Message {
                subject, selector, ..
            } if subject == "hello" => selector.clone(),
            _ => None,
        })
        .expect("the wake names what acks it");
    // The same command an agent is told to run, with the selector it
    // was given.
    clank::cli::events::run(clank::cli::EventsArgs {
        command: clank::cli::EventsCmd::Ack(clank::cli::EventsAckArgs {
            repo: Some(repo.to_path_buf()),
            author: Some("sb".to_string()),
            ids: vec![selector],
        }),
    })
    .await
    .expect("a standby agent's message acks like any other event");

    // A producer that EMITS. `["true"]` used to wake by completing;
    // a source is a long-running producer now and its lines are the
    // events (a-command-is-an-event-source-like-any-other).
    let source = serde_json::json!({
        "kind": "command",
        "name": "done",
        "command": [
            "sh",
            "-c",
            r#"printf '{"id":"1","kind":"done","content":"finished"}\n'; sleep 300"#,
        ],
    })
    .to_string();
    let items = tokio::time::timeout(
        common::race_deadline(repo),
        clank::cli::wait::items(wait_args(repo, "sb", false, vec![source])),
    )
    .await
    .expect("the producer's event wakes the standby agent")
    .unwrap()
    .expect("a wait reports its items");
    assert!(
        items
            .iter()
            .any(|i| matches!(i, WaitItem::CommandEvent { .. })),
        "{items:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reviewer_switched_to_standby_mid_wait_is_owed_no_review_but_still_gets_messages() {
    let _serial = common::serial();
    let env = team_with_standby().await;
    let repo = env.repo();

    let waiter = tokio::spawn(clank::cli::wait::items(wait_args(
        repo,
        "codex",
        false,
        Vec::new(),
    )));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    clank::cli::agent::set_repo_review(repo, &label("codex"), ReviewKind::Standby).unwrap();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    tokio::time::sleep(common::parked_window(repo)).await;
    assert!(
        !waiter.is_finished(),
        "a plan landing after the switch owes the standby agent nothing"
    );

    clank::inbox::deliver(
        repo,
        &label("codex"),
        None,
        "2026-09-22T10:15:00Z",
        "still here?",
    )
    .unwrap();
    let items = tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("a message wakes it")
        .expect("join")
        .expect("the wait returns")
        .expect("a wait reports its items");
    assert!(!items.iter().any(is_review), "{items:?}");
    assert!(
        items
            .iter()
            .any(|i| matches!(i, WaitItem::Message { subject, .. } if subject == "still here?")),
        "{items:?}"
    );
}
