//! Command wake sources for `clank wait` (extra-wait-events, M2):
//! spawned when the wait parks, completion is the wake, process-group
//! killed on any other exit. In-process; sh fixtures only.

use crate::common;
use common::TestEnv;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use clank::cli::WaitArgs;

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

fn wait_args(repo: &Path, events: Vec<String>) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some("rev".into()),
        die_with_owner: false,
        deliver_to_session: None,
        json: true,
        events,
        r#for: None,
        peek: false,
        no_cache: false,
        poll: true,
        no_poll: false,
    }
}

/// An idle-reviewer repo: no plans, so the wait parks until a source
/// (or timeout) wakes it.
fn idle_env() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, "seed.txt", "seed");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);
    env
}

fn cmd_event(name: &str, argv: &[&str]) -> String {
    serde_json::json!({ "kind": "command", "name": name, "command": argv }).to_string()
}

fn timer_event(name: &str, due: u64) -> String {
    let instant = time::OffsetDateTime::from_unix_timestamp(due as i64).unwrap();
    serde_json::json!({"kind":"timer", "name":name,
        "due_at":instant.format(&time::format_description::well_known::Rfc3339).unwrap(),
        "content":"check the deployment"})
    .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recurring_timer_catches_up_once_replays_and_ack_covers_offline_ticks() {
    use clank_core::wait::WaitItem;
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let mut timer: serde_json::Value = serde_json::from_str(&timer_event("daily", 1)).unwrap();
    timer["repeat_every"] = "24h".into();
    let config = serde_json::json!({"wait_events":[timer]}).to_string();
    write(repo, ".clank/agents/master/config.json", &config);
    write(repo, ".clank/queue/500-ready.md", "# ready\n");
    let args = |peek| {
        let mut args = wait_args(repo, Vec::new());
        args.author = Some("master".into());
        args.peek = peek;
        args
    };
    let peek = clank::cli::wait::items(args(true)).await.unwrap().unwrap();
    assert!(
        !peek
            .iter()
            .any(|item| matches!(item, WaitItem::TimerEvent { .. }))
    );
    let starts = clank::cli::wait::SourceStarts::default();
    let got = clank::cli::wait::items_observing(args(false), &starts)
        .await
        .unwrap()
        .unwrap();
    let accepted: Vec<_> = got
        .into_iter()
        .filter(|item| matches!(item, WaitItem::TimerEvent { .. }))
        .collect();
    assert_eq!(accepted.len(), 1);
    let WaitItem::TimerEvent {
        recurrence: Some(metadata),
        selector: Some(selector),
        ..
    } = &accepted[0]
    else {
        panic!("missing recurrence custody")
    };
    assert_eq!(metadata.first_slot, 0);
    assert!(
        metadata.count > 10_000,
        "decades offline coalesce into one event"
    );
    assert_eq!(starts.count(), 0, "catch-up needs no producer drivers");
    let key = selector.split_once('@').unwrap().0;
    let path = repo.join(format!(".clank/agents/master/events/{key}.jsonl"));
    let bytes = std::fs::read(&path).unwrap();
    let replay = clank::cli::wait::items(args(false)).await.unwrap().unwrap();
    assert!(replay.contains(&accepted[0]));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "pending ticks do not append"
    );
    write(repo, ".clank/agents/master/config.json", "{}");
    let removed = clank::cli::wait::items(args(true)).await.unwrap().unwrap();
    assert!(
        removed.contains(&accepted[0]),
        "removed definition still owes accepted work"
    );
    clank::cli::events::run(clank::cli::EventsArgs {
        command: clank::cli::EventsCmd::Ack(clank::cli::EventsAckArgs {
            reply: None,
            repo: Some(repo.to_path_buf()),
            author: Some("master".into()),
            ids: vec![selector.clone()],
        }),
    })
    .await
    .unwrap();
    write(repo, ".clank/agents/master/config.json", &config);
    let restored = clank::cli::wait::items(args(false)).await.unwrap().unwrap();
    assert!(
        !restored
            .iter()
            .any(|item| matches!(item, WaitItem::TimerEvent { .. })),
        "restored generation preserves ack coverage"
    );
    assert!(
        !repo
            .join(format!(".clank/agents/master/events/{key}.seen"))
            .exists()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recurring_timer_deadline_wakes_real_local_wait_without_an_extra_producer() {
    let _serial = common::serial();
    let env = idle_env();
    let due = time::OffsetDateTime::now_utc().unix_timestamp() as u64 + 2;
    let mut timer: serde_json::Value =
        serde_json::from_str(&timer_event("recurring-future", due)).unwrap();
    timer["repeat_every"] = "1h".into();
    let mut args = wait_args(env.repo(), vec![timer.to_string()]);
    args.poll = false;
    args.no_poll = true;
    let got = tokio::time::timeout(
        common::race_deadline(env.repo()),
        clank::cli::wait::items(args),
    )
    .await
    .expect("recurring native deadline wakes local wait")
    .unwrap()
    .unwrap();
    assert!(time::OffsetDateTime::now_utc().unix_timestamp() as u64 >= due);
    assert!(
        matches!(got.as_slice(), [clank_core::wait::WaitItem::TimerEvent {
        recurrence: Some(metadata), due_at, selector: Some(_), ..
    }] if *due_at == due && metadata.first_slot == 0 && metadata.last_slot == 0 && metadata.count == 1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timer_peek_is_read_only_and_normal_catchup_precedes_ready_work() {
    use clank_core::wait::WaitItem;
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let timer = timer_event("deploy", 1);
    let config_path = ".clank/agents/master/config.json";
    write(repo, config_path, &format!("{{\"wait_events\":[{timer}]}}"));
    write(repo, ".clank/queue/500-ready.md", "# ready\n");
    let mut args = wait_args(repo, Vec::new());
    args.author = Some("master".into());
    args.peek = true;
    let starts = clank::cli::wait::SourceStarts::default();
    let got = clank::cli::wait::items_observing(args, &starts)
        .await
        .unwrap()
        .unwrap();
    assert!(
        got.iter()
            .any(|item| matches!(item, WaitItem::PromoteFromQueue { .. }))
    );
    assert!(
        !got.iter()
            .any(|item| matches!(item, WaitItem::TimerEvent { .. }))
    );
    let clank_core::agent_config::WaitEventSource::Timer(source) =
        serde_json::from_str(&timer).unwrap()
    else {
        unreachable!()
    };
    let events = repo.join(".clank/agents/master/events");
    assert!(
        !events
            .join(format!("{}.jsonl", source.source_key(None)))
            .exists(),
        "peek cannot append a timer WAL"
    );
    assert!(
        !events
            .join(format!("{}.seen", source.source_key(None)))
            .exists(),
        "peek cannot checkpoint a timer ID"
    );

    let mut args = wait_args(repo, Vec::new());
    args.author = Some("master".into());
    let got = clank::cli::wait::items_observing(args, &starts)
        .await
        .unwrap()
        .unwrap();
    assert!(
        got.iter()
            .any(|item| matches!(item, WaitItem::PromoteFromQueue { .. }))
    );
    let timers: Vec<_> = got
        .iter()
        .filter(|item| matches!(item, WaitItem::TimerEvent { .. }))
        .collect();
    assert_eq!(
        timers.len(),
        1,
        "sink plus scan must not duplicate the accepted row"
    );
    let selector = timers[0].event_view().unwrap().selector.unwrap().to_owned();
    assert_eq!(
        starts.count(),
        0,
        "catchup starts no ordinary source drivers"
    );
    write(repo, config_path, "{}");
    let mut args = wait_args(repo, Vec::new());
    args.author = Some("master".into());
    args.peek = true;
    let got = clank::cli::wait::items(args).await.unwrap().unwrap();
    assert!(
        got.iter().any(|item| item
            .event_view()
            .is_some_and(|view| view.selector == Some(selector.as_str()))),
        "source removal must leave accepted work owed"
    );
    let ack = |reply| clank::cli::EventsArgs {
        command: clank::cli::EventsCmd::Ack(clank::cli::EventsAckArgs {
            reply,
            repo: Some(repo.to_path_buf()),
            author: Some("master".into()),
            ids: vec![selector.clone()],
        }),
    };
    assert!(
        clank::cli::events::run(ack(Some("answer".into())))
            .await
            .is_err(),
        "timer has no reply backend"
    );
    clank::cli::events::run(ack(None)).await.unwrap();
    write(repo, config_path, &format!("{{\"wait_events\":[{timer}]}}"));
    let mut args = wait_args(repo, Vec::new());
    args.author = Some("master".into());
    let got = clank::cli::wait::items(args).await.unwrap().unwrap();
    assert!(
        !got.iter()
            .any(|item| matches!(item, WaitItem::TimerEvent { .. })),
        "acked configured occurrence cannot refire"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timer_deadline_wakes_real_parked_wait_without_a_producer() {
    let _serial = common::serial();
    let env = idle_env();
    let due = time::OffsetDateTime::now_utc().unix_timestamp() as u64 + 2;
    let args = wait_args(env.repo(), vec![timer_event("future", due)]);
    let got = tokio::time::timeout(
        common::race_deadline(env.repo()),
        clank::cli::wait::items(args),
    )
    .await
    .expect("deadline wakes parked wait")
    .unwrap()
    .unwrap();
    assert!(time::OffsetDateTime::now_utc().unix_timestamp() as u64 >= due);
    assert!(
        matches!(got.as_slice(), [clank_core::wait::WaitItem::TimerEvent { due_at, selector: Some(_), .. }] if *due_at == due)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timer_invocations_have_distinct_custody_and_old_calls_do_not_wake_peek() {
    let _serial = common::serial();
    let env = idle_env();
    let timer = timer_event("same", 1);
    let first = clank::cli::wait::items(wait_args(env.repo(), vec![timer.clone()]))
        .await
        .unwrap()
        .unwrap();
    let second = clank::cli::wait::items(wait_args(env.repo(), vec![timer]))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(
        first[0].event_view().unwrap().selector,
        second[0].event_view().unwrap().selector
    );
    let mut args = wait_args(env.repo(), Vec::new());
    args.peek = true;
    assert!(
        clank::cli::wait::items(args)
            .await
            .unwrap()
            .unwrap()
            .is_empty()
    );
}

/// A printed EVENT wakes a parked wait.
///
/// It used to be the command's completion
/// (a-command-is-an-event-source-like-any-other): a source is a
/// long-running producer now, so exiting is not news — a line is.
/// A message keeps its lines from the producer that printed it to the
/// reply that answers it.
///
/// Ingestion used to run the row preview `one_line` on the content, so
/// every line after the first was gone before the event was stored. A
/// later surface could render however it liked; there was nothing left
/// to render. This drives the REAL path: one JSONL record from a real
/// producer, the wait it wakes, the durable log, a replay, and a reply
/// through the source's own `reply_command`.
///
/// The record's newlines are escaped inside its JSON string — it is ONE
/// record, which is the producer contract, not one record per line.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multiline_message_survives_ingestion_storage_replay_and_reply() {
    use clank_core::wait::WaitItem;
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();

    let raw = "Para one.\n\nPara two:\n  indented\n\tTabbed\tcols\n\
               üñí 中文 ✓ 🎙\nColour \u{1b}[31mred\u{1b}[0m done\u{7}\n\
               Title \u{1b}]0;evil\u{7}ok\r\nlast\n\
               Bare esc\u{1b}\nkept\n\
               Open \u{1b}]0;never ends\rnext\rafter";
    // Everything a message is made of, kept; terminal control, gone whole.
    // The last three lines are the losses codex found on 7233636: an ESC
    // that took the newline after it, and an unterminated OSC that ran
    // through CR-separated lines to the end of the message. A BARE CR, on
    // purpose: after a CRLF the LF stops the sequence one byte later and
    // the output is the same, so a CRLF here could not tell the fix from
    // the bug — and a mutation run showed exactly that.
    let expected = "Para one.\n\nPara two:\n  indented\n\tTabbed\tcols\n\
                    üñí 中文 ✓ 🎙\nColour red done\nTitle ok\nlast\n\
                    Bare esc\nkept\n\
                    Open \nnext\nafter";

    let record = serde_json::json!({
        "id": "m1", "kind": "message", "requires_reply": true, "content": raw,
    })
    .to_string();
    assert!(!record.contains('\n'), "the fixture is ONE JSONL record");
    write(repo, "payload.jsonl", &format!("{record}\n"));
    let reply_file = repo.join("reply.json");
    let config = serde_json::json!({"wait_events": [{
        "kind": "command",
        "name": "signal",
        "command": ["sh", "-c", format!(
            "sleep 0.4; cat '{}'; sleep 300", repo.join("payload.jsonl").display()
        )],
        "reply_command": ["sh", "-c", format!("cat > '{}'", reply_file.display())],
    }]})
    .to_string();
    write(repo, ".clank/agents/master/config.json", &config);
    let args = |peek: bool| {
        let mut a = wait_args(repo, Vec::new());
        a.author = Some("master".into());
        a.peek = peek;
        a
    };

    // INGESTION: the wait wakes with the whole message.
    let got = clank::cli::wait::items(args(false)).await.unwrap().unwrap();
    let event = got
        .iter()
        .find(|i| matches!(i, WaitItem::CommandEvent { .. }))
        .expect("woken by the message")
        .clone();
    let WaitItem::CommandEvent {
        content,
        requires_reply,
        selector: Some(selector),
        ..
    } = &event
    else {
        panic!("a command event with a selector: {event:?}")
    };
    assert_eq!(content, expected, "the message as the agent receives it");
    assert!(requires_reply, "the reply requirement rode along unchanged");

    // STORAGE: the durable log holds exactly that body, so nothing that
    // reads it later can be handed less.
    let key = selector.split_once('@').unwrap().0;
    let log = repo.join(format!(".clank/agents/master/events/{key}.jsonl"));
    let stored = std::fs::read_to_string(&log).unwrap();
    let encoded = serde_json::to_string(expected).unwrap();
    assert!(
        stored.contains(&encoded),
        "the stored body is the whole message: {stored}"
    );
    assert!(!stored.contains("[31m"), "and carries no escape syntax");

    // REPLAY: a later wait delivers the same item, byte for byte.
    let replay = clank::cli::wait::items(args(false)).await.unwrap().unwrap();
    assert!(replay.contains(&event), "replayed: {replay:?}");

    // The receipt-aware READ is asserted in `events::read`'s own tests:
    // it records delivery against a bound session, which this harness
    // clears on purpose, and that module's identity seam supplies one
    // without mutating the environment every other test shares.

    // REPLY: still required, still routed — and the reply keeps its
    // lines too, on its way back to the producer.
    let answer = "Got it.\n\nWill do.";
    clank::cli::events::run(clank::cli::EventsArgs {
        command: clank::cli::EventsCmd::Ack(clank::cli::EventsAckArgs {
            reply: Some(answer.into()),
            repo: Some(repo.to_path_buf()),
            author: Some("master".into()),
            ids: vec![selector.clone()],
        }),
    })
    .await
    .expect("the reply is delivered and the event acked");
    let delivered: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&reply_file).unwrap()).unwrap();
    assert_eq!(delivered["reply"], answer, "{delivered}");
    assert_eq!(delivered["id"], "m1");

    let after = clank::cli::wait::items(args(true)).await.unwrap().unwrap();
    assert!(!after.contains(&event), "answered, so no longer owed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_printed_event_wakes_a_parked_wait() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let ev = cmd_event(
        "probe",
        &[
            "sh",
            "-c",
            r#"sleep 0.4; printf '{"id":"1","kind":"probe","content":"the-payload"}\n'; sleep 300"#,
        ],
    );
    clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect("a line the producer printed is the wake");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn returning_kills_the_whole_process_group() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("grandchild.pid");
    // The shell backgrounds a long sleep (a GRANDCHILD) and then waits
    // on it: only a group kill reaps the sleep.
    let script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
    let stuck = cmd_event("stuck", &["sh", "-c", &script]);
    // A second source EMITS to supply the wake. The wait used to be
    // ended by its own timeout; with the flag gone, the teardown path
    // under test is reached the way production reaches it — by a wake
    // (remove-wait-timeout). It emits rather than completing, because
    // completing is no longer a wake.
    let waker = cmd_event(
        "waker",
        &[
            "sh",
            "-c",
            r#"sleep 1.5; printf '{"id":"1","kind":"waker","content":"go"}\n'; sleep 300"#,
        ],
    );
    clank::cli::wait::run(wait_args(repo, vec![stuck, waker]))
        .await
        .expect("the waker's event returns the wait");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("grandchild pidfile written")
        .trim()
        .parse()
        .unwrap();
    // Give the group kill a beat, then the grandchild must be gone.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "grandchild {pid} must die with the wait (group kill)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_source_wait_parks_to_its_timeout() {
    // A wait with NO event sources parks to its deadline and returns
    // parked. (The no-hot-loop fuse itself is pinned
    // deterministically by the `drain_external_fuses_a_closed_channel`
    // unit test — codex 907acd5 — since wall time can't distinguish a
    // spin from a park.)
    let env = idle_env();
    let repo = env.repo();
    common::assert_stays_parked(repo, wait_args(repo, vec![]), "idle with no sources").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_event_during_the_refold_is_not_lost() {
    // codex 907acd5: the single return boundary drains once more after
    // the refold, so an external item that races a repo wake is never
    // aborted away. Arm a command source AND create repo work mid-wait;
    // whichever the beat sees, the other merges into the same result
    // and the wait RETURNS (never hangs).
    let env = idle_env();
    let repo = env.repo();
    let ev = cmd_event("racer", &["sh", "-c", "sleep 0.6; echo hi"]);
    let waiter = tokio::spawn(clank::cli::wait::run(wait_args(repo, vec![ev])));
    tokio::time::sleep(Duration::from_millis(400)).await;
    write(repo, ".clank/queue/500-p.md", "# p\n");
    tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("a beat with repo + external readiness must return, not hang")
        .expect("join")
        .expect("returns a combined result");
}

/// `--peek` returns the DELIBERATE no-work outcome and starts nothing.
///
/// Two claims, and the first is what makes the second mean anything.
/// This test used to assert only that a marker file was absent, after
/// an `.expect("peek returns")` that had already stopped holding —
/// an empty peek is `WaitNoWork` BY DESIGN, so the test failed before
/// reaching the assertion it is named for. Making the verdict "either
/// outcome" would have been worse: a broken fixture, an identity it
/// could not resolve or a fold that failed all leave the marker absent
/// too, so the test would pass without ever entering the peek branch
/// (codex on fbb84e3).
///
/// The outcome is identified by the code it EXITS with rather than by
/// its message: 3 is `WaitNoWork` and every other failure is 1, so
/// this distinguishes the intended answer from an arbitrary one without
/// reading an error chain that carries our own argv.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peek_starts_no_command_source() {
    let env = idle_env();
    let repo = env.repo();
    let marker = repo.join("source_ran.marker");
    let ev = cmd_event("touch", &["touch", marker.to_str().unwrap()]);

    // The projection itself, positively: empty, and reached before any
    // source derivation, monitor attach or watcher setup.
    let starts = clank::cli::wait::SourceStarts::default();
    let mut args = wait_args(repo, vec![ev.clone()]);
    args.peek = true;
    match clank::cli::wait::items_observing(args, &starts)
        .await
        .expect("peek projects rather than failing")
    {
        Some(items) => assert!(
            items.is_empty(),
            "an idle repo has no work to peek at, got {} item(s)",
            items.len()
        ),
        None => panic!("peek must project; `None` is observer mode, which this is not"),
    }

    // And the command's verdict is the one the peek branch produces.
    let mut args = wait_args(repo, vec![ev]);
    args.peek = true;
    let refused = clank::cli::wait::run(args)
        .await
        .expect_err("an empty peek is an error by design, so the hook can branch on it");
    assert_eq!(
        clank::cli::command::exit_code_for(&refused),
        3,
        "the deliberate no-work exit, not an arbitrary failure: {refused:#}"
    );

    // The boundary itself, which is the only thing that can tell
    // "never started" from "started and torn down before its command
    // ran". The marker below cannot: with the peek return moved under
    // the spawn, it caught the detached variant and missed the one
    // that shut the tasks down.
    assert_eq!(
        starts.count(),
        0,
        "--peek must not reach the source-start boundary"
    );

    // And the child's own evidence, which agrees when it can.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!marker.exists(), "--peek must spawn no source");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fast_source_wins_and_the_slow_source_is_group_killed() {
    // Two sources: one completes fast (the wake), one is a stuck shell
    // with a backgrounded grandchild. On return the supervisor aborts
    // AND joins the slow task; its process group — grandchild included
    // — must be dead.
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("slow.pid");
    let slow_script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
    let slow = cmd_event("slow", &["sh", "-c", &slow_script]);
    let fast = cmd_event("fast", &["sh", "-c", "sleep 0.5; echo done"]);
    clank::cli::wait::run(wait_args(repo, vec![slow, fast]))
        .await
        .expect("the fast source wakes the wait");

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("slow source's grandchild pidfile")
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "the slow source's grandchild {pid} must be group-killed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_github_poll_interval_fails_loud_at_arm_time() {
    // codex ef1861a: a malformed poll_interval must error at arm time,
    // not silently fall back to the default deep in the poll loop.
    let env = idle_env();
    let repo = env.repo();
    let ev = serde_json::json!({
        "kind": "github",
        "repo": "o/r",
        "events": ["issue_opened"],
        "poll_interval": "banana"
    })
    .to_string();
    let err = clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect_err("a bad poll_interval must fail loud");
    assert!(
        format!("{err:#}").contains("poll_interval"),
        "error names the bad field: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ci_watch_arms() {
    // The kinds are readable now, so a watch that names them arms
    // like any other. (It replaces the refusal that stood between the
    // vocabulary landing and the reader landing.) No network: the
    // wait parks and the test's timeout ends it.
    let env = idle_env();
    let repo = env.repo();
    let ev = serde_json::json!({
        "kind": "github",
        "repo": "o/r",
        "events": ["ci_failed"],
        "prs": [7],
    })
    .to_string();
    let armed = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        clank::cli::wait::run(wait_args(repo, vec![ev])),
    )
    .await;
    assert!(
        armed.is_err(),
        "arming succeeds and the wait parks; it does not refuse: {armed:?}",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_config_wait_events_is_a_hard_error() {
    // codex 1bbb61d: an unknown tagged kind in the FILE config must
    // propagate, not be silently treated as no sources.
    let env = idle_env();
    let repo = env.repo();
    let cfg_dir = repo.join(".clank/agents/rev");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.json"),
        r#"{"wait_events":[{"kind":"telepathy","repo":"x"}]}"#,
    )
    .unwrap();
    let err = clank::cli::wait::run(wait_args(repo, vec![]))
        .await
        .expect_err("unknown config kind must fail loud");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("config") || msg.contains("wait_events") || msg.contains("kind"),
        "error surfaces the config parse failure: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_event_json_is_an_arm_time_error() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let err = clank::cli::wait::run(wait_args(repo, vec!["{\"kind\":\"nope\"}".into()]))
        .await
        .expect_err("unknown kind fails loud at arm time");
    assert!(
        format!("{err:#}").contains("invalid --event"),
        "error names the offending input: {err:#}"
    );
}

/// A command source that could never work fails the ARM,/// A command source that could never work fails the ARM,/// A command source that could never work fails the ARM, rather than
/// printing from inside its task and leaving the wait running
/// without it.
///
/// Both of these used to arm fine and then quietly return from the
/// source task, so the operator got a wait that looked healthy and
/// watched nothing (codex 072e459). A source the wait cannot run is
/// a configuration error, and the wait refuses.
#[tokio::test]
async fn a_command_source_that_cannot_run_fails_the_arm() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();

    let empty = r#"{"kind":"command","name":"deploys","command":[]}"#;
    let err = clank::cli::wait::run(wait_args(repo, vec![empty.into()]))
        .await
        .expect_err("an empty command has nothing to run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("deploys") && msg.contains("empty command"),
        "the error names the source and what is wrong: {msg}"
    );

    // The companion — that a VALID source is not refused — is a lib
    // test (`cli::wait::tests::arm_time_validation_refuses_only_what_cannot_run`).
    // It cannot live here: arming a good source PARKS, and an idle
    // wait has no timeout to end it (remove-wait-timeout), so the
    // only thing that would return is the backoff's failure event
    // several seconds later. A test that depends on that is timing
    // the constants, not the refusal.
}

/// A producer whose descendant holds the pipe open still wakes the
/// wait, and the descendant dies with it.
///
/// Replaces three tests of the one-shot contract
/// (a-command-is-an-event-source-like-any-other): root exit is no
/// longer a wake, so "root exit wakes despite a pipe-holding /
/// continuously-writing descendant" cannot be asserted here. The
/// hazard those protected — a held pipe stalling the source — is
/// asserted where the restart is observable, in
/// `cli::wait::tests::a_pipe_holding_descendant_does_not_stall_the_restart`.
/// What is integration-level is that the event still arrives and the
/// group is still killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pipe_holding_descendant_neither_blocks_the_wake_nor_survives_it() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("holder.pid");
    // `yes` inherits stdout AND writes to it forever: the pipe never
    // reaches EOF and never goes quiet.
    let script = format!(
        r#"yes > /dev/null & echo $! > '{}'; sleep 300 & printf '{{"id":"1","kind":"k","content":"emitted"}}\n'; sleep 300"#,
        pidfile.display()
    );
    let ev = cmd_event("holder", &["sh", "-c", &script]);
    clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect("the event arrives despite a descendant holding the pipe");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("descendant pidfile")
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "descendant {pid} must die with the wait");
}
