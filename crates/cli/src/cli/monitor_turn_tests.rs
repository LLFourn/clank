//! A report's whole exchange through the monitor's own admission, reader,
//! custody and serving, and the child that carries it: the transport the
//! turn policy's terminal-failure contract rests on.

use super::*;
use crate::cli::turn_custody::Turns;
use crate::cli::turn_wire::{Frame, Report, TurnResult, WireVerdict};

const SESSION: &str = "session-handshake";

/// A Claude pane whose input line is empty.
const EMPTY_INPUT: &str = "────────────────\n❯ \n────────────────\n";

/// A timer event no log keeps: the monitor holds it for its author until
/// a presentation of it is delivered.
fn transient(author: &AgentLabel) -> Vec<(AgentLabel, WaitItem)> {
    vec![(
        author.clone(),
        WaitItem::TimerEvent {
            presentation: None,
            name: "transient-probe".into(),
            recurrence: None,
            id: "transient-probe".into(),
            due_at: 100,
            content: "UNLOGGED-PAYLOAD".into(),
            selector: None,
        },
    )]
}

fn gathered_with(
    repo: &std::path::Path,
    author: &AgentLabel,
    state: &crate::repo_state::RepoState,
    pending: &[(AgentLabel, WaitItem)],
) -> crate::cli::turn_custody::Gathered {
    gather(
        repo,
        Some(state),
        author,
        &Default::default(),
        &mut Default::default(),
        pending,
    )
}

/// What the next Stop of another connection is presented.
fn presented_next(
    turns: &mut Turns,
    author: &AgentLabel,
    g: &crate::cli::turn_custody::Gathered,
) -> String {
    let next = turns.answer(author, 999, &stop_report(), true, g).unwrap();
    assert!(next.presentation().is_some(), "presented afresh: {next:?}");
    next.rendered.unwrap_or_default()
}

/// A presentation that is not delivered, because the hook's emit failed
/// or its connection closed without a result, leaves the transient work
/// it carried held and owed: the next Stop is presented it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_emit_keeps_unlogged_work_owed() {
    for ending in ["emit failed", "closed"] {
        let (repo, author, state) = owed_repo().await;
        let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let mut hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
        let client = admitted(theirs, repo.path(), &heard_tx).await;
        let conn = client.conn;
        let mut clients = vec![client];
        let mut turns = Turns::new(repo.path().to_path_buf());
        let mut pending = transient(&author);
        let leaving = serve_reports(
            repo.path(),
            &mut clients,
            Some(&state),
            None,
            &pending,
            &mut Default::default(),
            &Default::default(),
            &mut turns,
        );
        let (frame, mut hook) = tokio::task::spawn_blocking(move || (hook.frame(), hook))
            .await
            .unwrap();
        assert!(
            frame
                .rendered
                .as_ref()
                .unwrap()
                .contains("UNLOGGED-PAYLOAD")
        );
        match ending {
            "emit failed" => {
                hook.answer(&TurnResult::EmitFailed {
                    presentation: frame.presentation().unwrap().clone(),
                    why: "write failed".into(),
                });
                let (_, Heard::Line(line)) = next_heard(&mut heard).await else {
                    panic!("expected a result");
                };
                let settled = settle_result(
                    &mut turns,
                    &mut pending,
                    &author,
                    conn,
                    serde_json::from_str(&line).unwrap(),
                    |pending| gathered_with(repo.path(), &author, &state, pending),
                );
                assert!(settled.unwrap().is_none(), "nothing commits");
            }
            _ => turns.closed(
                &author,
                conn,
                &gathered_with(repo.path(), &author, &state, &pending),
            ),
        }
        drop(hook.stdin);
        hook.child.join().unwrap();
        for writer in leaving {
            let _ = writer.await;
        }
        assert_eq!(pending, transient(&author), "{ending}: still held");
        let g = gathered_with(repo.path(), &author, &state, &pending);
        assert!(
            presented_next(&mut turns, &author, &g).contains("UNLOGGED-PAYLOAD"),
            "{ending}: the failed presentation must leave transient work owed"
        );
    }
}

/// A presentation whose verdict could not even be queued to its carrier
/// fails at once: nobody can answer for it, so it is not left in flight,
/// and what it carried is presented to the next Stop.
#[tokio::test]
async fn a_presentation_its_carrier_never_received_fails_at_once() {
    let (repo, author, state) = owed_repo().await;
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    drop(queued);
    let mut clients = vec![Client {
        who: Served::Agent {
            author: author.clone(),
        },
        holder: None,
        out,
        writer: None,
        farewell: None,
        shown: Default::default(),
        conn: 7,
        report: Some(Reporting {
            report: stop_report(),
            stepped: false,
            reader: None,
        }),
    }];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let pending = transient(&author);
    serve_reports(
        repo.path(),
        &mut clients,
        Some(&state),
        None,
        &pending,
        &mut Default::default(),
        &Default::default(),
        &mut turns,
    );
    assert!(clients.is_empty(), "the carrier is retired");
    assert_eq!(turns.state(&author).unwrap().outstanding, None);
    let g = gathered_with(repo.path(), &author, &state, &pending);
    assert!(presented_next(&mut turns, &author, &g).contains("UNLOGGED-PAYLOAD"));
}

/// A delivered presentation discharges the transient work it carried, and
/// only that: it is not presented again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_discharges_the_transient_work_it_presented() {
    let (repo, author, state) = owed_repo().await;
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
    let client = admitted(theirs, repo.path(), &heard_tx).await;
    let conn = client.conn;
    let mut clients = vec![client];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let other = AgentLabel::parse("other").unwrap();
    let mut pending = transient(&author);
    pending.extend(transient(&other));
    let leaving = serve_reports(
        repo.path(),
        &mut clients,
        Some(&state),
        None,
        &pending,
        &mut Default::default(),
        &Default::default(),
        &mut turns,
    );
    let (frame, mut hook) = tokio::task::spawn_blocking(move || (hook.frame(), hook))
        .await
        .unwrap();
    assert!(
        frame
            .rendered
            .as_ref()
            .unwrap()
            .contains("UNLOGGED-PAYLOAD")
    );
    let arrived = (
        author.clone(),
        WaitItem::TimerEvent {
            presentation: None,
            name: "arrived-in-flight".into(),
            recurrence: None,
            id: "arrived-in-flight".into(),
            due_at: 200,
            content: "LATER-PAYLOAD".into(),
            selector: None,
        },
    );
    pending.push(arrived.clone());
    hook.answer(&TurnResult::Delivered {
        presentation: frame.presentation().unwrap().clone(),
    });
    let (_, Heard::Line(line)) = next_heard(&mut heard).await else {
        panic!("expected a result");
    };
    let settled = settle_result(
        &mut turns,
        &mut pending,
        &author,
        conn,
        serde_json::from_str(&line).unwrap(),
        |pending| gathered_with(repo.path(), &author, &state, pending),
    );
    assert!(settled.unwrap().is_some(), "the delivery commits");
    let mut kept = transient(&other);
    kept.push(arrived);
    assert_eq!(pending, kept, "only what it presented is discharged");
    hook.child.join().unwrap();
    for writer in leaving {
        let _ = writer.await;
    }
    let g = gathered_with(repo.path(), &author, &state, &pending);
    let next = presented_next(&mut turns, &author, &g);
    assert!(
        !next.contains("UNLOGGED-PAYLOAD") && next.contains("LATER-PAYLOAD"),
        "{next}"
    );
}

/// An agent whose binding goes is retired from custody at the fold that
/// sees it, before its check-in is due: the nearest deadline is gone, so
/// the monitor's loop has nothing to wake for.
#[tokio::test]
async fn an_unbinding_seen_by_a_fold_retires_the_check_in_it_owed() {
    let (repo, author, state) = owed_repo().await;
    let (mut turns, due_at) = attended(repo.path(), &author, &state);
    assert_eq!(turns.next_deadline(), Some(due_at));
    crate::agent_store::release_binding(
        repo.path(),
        &author,
        &clank_core::SessionId::parse(SESSION).unwrap(),
    )
    .unwrap();
    recovery_facts(
        repo.path(),
        &state,
        &[],
        &Default::default(),
        &mut Default::default(),
        &[],
        &mut turns,
    );
    assert_eq!(turns.next_deadline(), None);
    assert!(
        !crate::agent_store::agents_root(repo.path())
            .join("claude/turn.json")
            .exists(),
        "retired on disk too"
    );
}

/// The loop's step at a due deadline leaves nothing due behind it, even
/// for an agent unbound since its check-in was armed: the next deadline
/// is never one already passed, so the loop cannot spin on it.
#[tokio::test]
async fn a_due_check_in_of_an_unbound_agent_is_not_due_again() {
    let (repo, author, state) = owed_repo().await;
    let (mut turns, due_at) = attended(repo.path(), &author, &state);
    crate::agent_store::release_binding(
        repo.path(),
        &author,
        &clank_core::SessionId::parse(SESSION).unwrap(),
    )
    .unwrap();
    let stepped = due_check_ins(&mut turns, due_at, |author| {
        gathered(repo.path(), author, &state).at(due_at)
    });
    assert!(
        stepped.iter().all(|(_, _, due)| due.is_none()),
        "nothing typed"
    );
    assert!(turns.next_deadline().is_none_or(|at| at > due_at));
    assert!(turns.due_authors(due_at).is_empty());
}

/// Recovery holding `author` as bound now: a Claude, resumed by typing.
fn recovering(repo: &std::path::Path, author: &AgentLabel) -> crate::cli::recovery::Driver {
    let binding = crate::agent_store::load_agent_config(repo, author)
        .unwrap()
        .unwrap()
        .session
        .unwrap();
    let mut recovery = crate::cli::recovery::Driver::new(repo.to_path_buf(), "/nonexistent".into());
    recovery.reconcile(
        vec![crate::cli::recovery::Facts {
            agent_dir: crate::agent_store::agents_root(repo).join(author.as_str()),
            author: author.clone(),
            session: binding.clone(),
            auto: true,
            work: vec![],
            delivering: false,
            obligated: false,
            role: "master".into(),
            blind_nudge: None,
            stall: None,
            attending: false,
            asking: false,
            projection: Default::default(),
        }],
        0,
    );
    recovery
}

/// Register "sleeper" as a `clank run` of this test's own process, from no
/// turn state at all: the custody, and when its check-in is due. A run, so
/// it attends across turns.
fn attended(
    repo: &std::path::Path,
    author: &AgentLabel,
    state: &crate::repo_state::RepoState,
) -> (Turns, u64) {
    let _ = std::fs::remove_file(
        crate::agent_store::agents_root(repo)
            .join(author.as_str())
            .join("turn.json"),
    );
    let mut turns = Turns::new(repo.to_path_buf());
    let g = gather(
        repo,
        Some(state),
        author,
        &Default::default(),
        &mut Default::default(),
        &[],
    );
    attend_run(&mut turns, author, "sleeper", &g);
    let due = turns.next_deadline().expect("a check-in is owed");
    (turns, due)
}

fn attend_run(
    turns: &mut Turns,
    author: &AgentLabel,
    desc: &str,
    g: &crate::cli::turn_custody::Gathered,
) {
    let pid = std::process::id() as i32;
    turns
        .attend(
            author,
            clank_core::turn_policy::AttendRequest::Run {
                session: clank_core::SessionId::parse(SESSION).unwrap(),
                proof: crate::cli::turn_custody::shell_proof(
                    pid,
                    &crate::proc_identity::token_for(pid).expect("our own token"),
                ),
                desc: desc.into(),
                expect: 60_000,
            },
            g,
        )
        .unwrap();
}

fn attend_sleeper(turns: &mut Turns, author: &AgentLabel, g: &crate::cli::turn_custody::Gathered) {
    attend_named(turns, author, "sleeper", g);
}

fn attend_named(
    turns: &mut Turns,
    author: &AgentLabel,
    name: &str,
    g: &crate::cli::turn_custody::Gathered,
) {
    turns
        .attend(
            author,
            clank_core::turn_policy::AttendRequest::Declare {
                session: clank_core::SessionId::parse(SESSION).unwrap(),
                name: name.into(),
                desc: None,
                expect: 60_000,
            },
            g,
        )
        .unwrap();
}

fn ask_for(due: &crate::cli::turn_custody::CheckInDue) -> CheckInAsk {
    let (reply, _answer) = tokio::sync::oneshot::channel();
    CheckInAsk {
        author: due.author.clone(),
        session: due.session.clone(),
        check_ins: due.check_ins.clone(),
        screen: (clank_core::Tool::Claude, Some(EMPTY_INPUT.into())),
        reply,
    }
}

/// The last look reads the custody and the gates as they are when it is
/// asked, not as they were when the check-in fired: auto turned off, a
/// question or a block opened, the attendance cleared or renewed, each
/// refuses the typing that would otherwise go ahead. Attending something
/// else beside it does not.
#[tokio::test]
async fn the_last_look_rechecks_the_attendance_and_every_gate_before_typing() {
    let (repo, author, state) = owed_repo().await;
    let recovery = recovering(repo.path(), &author);
    let dir = crate::agent_store::agents_root(repo.path()).join(author.as_str());
    let now = || {
        gather(
            repo.path(),
            Some(&state),
            &author,
            &Default::default(),
            &mut Default::default(),
            &[],
        )
    };
    let cases: [(&str, Option<&str>); 7] = [
        ("nothing", None),
        ("another entry beside it", None),
        ("auto off", Some("auto turned off")),
        ("question", Some("a question is pending")),
        ("block", Some("a block is pending")),
        (
            "clear",
            Some("the attendance it was for was cleared or renewed"),
        ),
        (
            "renew",
            Some("the attendance it was for was cleared or renewed"),
        ),
    ];
    for (change, refused) in cases {
        let (mut turns, due_at) = attended(repo.path(), &author, &state);
        let due = turns
            .check_in(&author, &now().at(due_at))
            .expect("it fires");
        match change {
            "auto off" => {
                crate::agent_store::set_auto_mode(repo.path(), &author, clank_core::AutoMode::Off)
                    .unwrap()
            }
            "question" => crate::cli::question_state::observe(
                &dir,
                [crate::cli::question_state::Observation::Asked {
                    key: crate::cli::question_state::Key {
                        session: SESSION.into(),
                        prompt: "p".into(),
                        tool_use: "t".into(),
                    },
                    header: "which one".into(),
                    input: "h".into(),
                }],
            ),
            "block" => {
                std::fs::create_dir_all(dir.join("blocks")).unwrap();
                std::fs::write(dir.join("blocks/decide.md"), "which one?").unwrap();
            }
            "clear" => {
                turns
                    .attend(
                        &author,
                        clank_core::turn_policy::AttendRequest::Clear {
                            session: clank_core::SessionId::parse(SESSION).unwrap(),
                            which: clank_core::turn_policy::Clearing::All,
                        },
                        &now(),
                    )
                    .unwrap();
            }
            "another entry beside it" => {
                attend_named(&mut turns, &author, "other helper", &now().at(due_at + 1))
            }
            "renew" => attend_sleeper(&mut turns, &author, &now().at(due_at + 1)),
            _ => {}
        }
        let looked = may_check_in_now(repo.path(), &recovery, &mut turns, &now(), &ask_for(&due));
        assert_eq!(
            looked.map(|text| text.contains("\"sleeper\" has been going")),
            refused.map_or(Ok(true), |why| Err(why.to_string())),
            "{change}"
        );
        crate::agent_store::set_auto_mode(repo.path(), &author, clank_core::AutoMode::On).unwrap();
        let _ = std::fs::remove_file(crate::cli::question_state::path(&dir));
        let _ = std::fs::remove_dir_all(dir.join("blocks"));
    }
}

/// A check-in whose attendance is replaced while it is being typed is
/// refused at its last look, types nothing, and its loss is not recorded
/// against the replacement, which keeps its own check-in.
#[tokio::test]
async fn a_check_in_overtaken_during_dispatch_leaves_its_replacement_alone() {
    let (repo, author, state) = owed_repo().await;
    let recovery = recovering(repo.path(), &author);
    let (mut turns, due_at) = attended(repo.path(), &author, &state);
    let now = |at: u64| {
        gather(
            repo.path(),
            Some(&state),
            &author,
            &Default::default(),
            &mut Default::default(),
            &[],
        )
        .at(at)
    };
    let old = turns.check_in(&author, &now(due_at)).expect("it fires");
    attend_sleeper(&mut turns, &author, &now(due_at + 1));
    let replacement = turns.projection(&author, &now(due_at + 2));
    let typist = std::sync::Arc::new(Recorded {
        typed: Default::default(),
        lost: false,
    });
    let g = now(due_at + 3);
    let done = check_in_through(repo.path(), old, typist.clone(), |ask| {
        may_check_in_now(repo.path(), &recovery, &mut turns, &g, ask)
    })
    .await;
    assert!(typist.typed.lock().unwrap().is_empty(), "nothing is typed");
    let why = done.outcome.expect_err("refused at its last look");
    assert!(why.contains("cleared or renewed"), "{why}");
    turns.check_in_failed(&author, done.check_ins, why, &g);
    assert_eq!(
        turns.projection(&author, &g),
        replacement,
        "the replacement keeps its own check-in"
    );
    assert!(turns.next_deadline().is_some_and(|at| at > due_at));
}

/// Two entries fall due together; one is renewed or cleared while the
/// check-in is being typed. What is typed is the other's line alone: the
/// stale one is not sent, and the one still owed is not lost with it.
#[tokio::test]
async fn a_grouped_check_in_types_only_the_entries_still_owed_it() {
    let (repo, author, state) = owed_repo().await;
    let recovery = recovering(repo.path(), &author);
    let now = |at: u64| {
        gather(
            repo.path(),
            Some(&state),
            &author,
            &Default::default(),
            &mut Default::default(),
            &[],
        )
        .at(at)
    };
    for change in ["renew", "clear"] {
        let _ = std::fs::remove_file(
            crate::agent_store::agents_root(repo.path())
                .join(author.as_str())
                .join("turn.json"),
        );
        let mut turns = Turns::new(repo.path().to_path_buf());
        let start = crate::cli::turn_custody::now_ms();
        attend_sleeper(&mut turns, &author, &now(start));
        attend_named(&mut turns, &author, "helper", &now(start));
        let due_at = turns.next_deadline().expect("both armed");
        let due = turns.check_in(&author, &now(due_at)).expect("it fires");
        assert_eq!(due.check_ins.len(), 2, "one check-in for both");
        match change {
            "renew" => attend_sleeper(&mut turns, &author, &now(due_at + 1)),
            _ => {
                turns
                    .attend(
                        &author,
                        clank_core::turn_policy::AttendRequest::Clear {
                            session: clank_core::SessionId::parse(SESSION).unwrap(),
                            which: clank_core::turn_policy::Clearing::Named("sleeper".into()),
                        },
                        &now(due_at + 1),
                    )
                    .unwrap();
            }
        }
        let typist = std::sync::Arc::new(Recorded {
            typed: Default::default(),
            lost: false,
        });
        let g = now(due_at + 2);
        let done = check_in_through(repo.path(), due, typist.clone(), |ask| {
            may_check_in_now(repo.path(), &recovery, &mut turns, &g, ask)
        })
        .await;
        assert!(done.outcome.is_ok(), "{change}: {:?}", done.outcome);
        let typed = typist.typed.lock().unwrap().clone();
        let [text] = typed.as_slice() else {
            panic!("{change}: typed once: {typed:?}");
        };
        assert!(
            text.contains("\"helper\" has been going"),
            "{change}: {text}"
        );
        assert!(!text.contains("sleeper"), "{change}: {text}");
    }
}

/// A repo whose master `claude` is bound to [`SESSION`] and owed one timer
/// event, folded.
async fn owed_repo() -> (TempRepo, AgentLabel, crate::repo_state::RepoState) {
    let (repo, author, timer) = timer_repo();
    log_timer_event(repo.path(), &timer);
    let state = folded(repo.path()).await.unwrap();
    (repo, author, state)
}

/// A repo whose master `claude` is bound to [`SESSION`] with a timer source
/// whose event is not logged yet.
fn timer_repo() -> (TempRepo, AgentLabel, clank_core::agent_config::TimerSource) {
    let repo = TempRepo::new();
    let author = AgentLabel::parse("claude").unwrap();
    crate::cli::agent::add_repo_roster_agent(
        repo.path(),
        &author,
        crate::cli::teams_config::AgentDescription {
            tool: clank_core::Tool::Claude,
            launch: None,
            initial_prompt: None,
        },
        crate::cli::teams_config::RosterRole::Commit,
    )
    .unwrap();
    crate::cli::agent::set_repo_master(repo.path(), &author).unwrap();
    let timer = clank_core::agent_config::TimerSource {
        interrupt: false,
        interrupt_message: None,
        name: "check".into(),
        repeat_every: None,
        due_at: "1970-01-01T00:01:40Z".into(),
        content: Some("scheduled work".into()),
    };
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        config.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
            timer.clone(),
        )];
    })
    .unwrap();
    crate::agent_store::bind_session_to_agent(
        repo.path(),
        &author,
        clank_core::Tool::Claude,
        &clank_core::SessionId::parse(SESSION).unwrap(),
    )
    .unwrap();
    crate::agent_store::set_auto_mode(repo.path(), &author, clank_core::AutoMode::On).unwrap();
    (repo, author, timer)
}

fn log_timer_event(repo: &std::path::Path, timer: &clank_core::agent_config::TimerSource) {
    log_timer_occurrence(repo, timer, 100, "scheduled work");
}

fn log_timer_occurrence(
    repo: &std::path::Path,
    timer: &clank_core::agent_config::TimerSource,
    due_at: u64,
    content: &str,
) {
    use crate::cli::github::log::{EventLog, Transport as Wal};
    let events = crate::agent_store::agents_root(repo).join("claude/events");
    std::fs::create_dir_all(&events).unwrap();
    let id = format!("timer/{due_at}");
    EventLog::open(&events, &timer.source_key(None))
        .unwrap()
        .append_inbox(
            Wal::Timer,
            Some(&id),
            None,
            Some(due_at),
            &WaitItem::TimerEvent {
                presentation: None,
                name: "check".into(),
                recurrence: None,
                id: id.clone(),
                due_at,
                content: content.into(),
                selector: None,
            },
        )
        .unwrap();
}

fn stop_report() -> Report {
    Report::TurnEnd {
        stop: clank_core::turn_policy::TurnEndReport {
            tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
            tool: clank_core::Tool::Claude,
            session: clank_core::SessionId::parse(SESSION).unwrap(),
            place: Default::default(),
        },
        deadline: u64::MAX,
        questions: Vec::new(),
    }
}

/// The monitor's side of a connection: admitted by the real handshake,
/// with its reader started as the owner loop starts it.
async fn admitted(
    theirs: std::os::unix::net::UnixStream,
    repo: &std::path::Path,
    heard: &tokio::sync::mpsc::UnboundedSender<(u64, Heard)>,
) -> Client {
    theirs.set_nonblocking(true).unwrap();
    let mut client = greet(tokio::net::UnixStream::from_std(theirs).unwrap(), repo)
        .await
        .expect("admitted")
        .client();
    let reader = client
        .report
        .as_mut()
        .and_then(|r| r.reader.take())
        .expect("a report's read side");
    tokio::spawn(hear(client.conn, reader, heard.clone()));
    client
}

/// The hook's side of the child: its stdin pipe and the frames it prints.
struct Hook {
    stdin: std::io::PipeWriter,
    frames: std::io::BufReader<std::io::PipeReader>,
    child: std::thread::JoinHandle<()>,
}

/// The `clank wait --turn-end` child's own carry, on a thread of this
/// process: it reads its owner from `owner`, prints frames to `frames`, and
/// reaches the test's monitors over `ours`, one connection per attempt in
/// order, and nothing once they are spent.
fn carrier(
    repo: &std::path::Path,
    author: &AgentLabel,
    owner: std::io::PipeReader,
    mut frames: std::io::PipeWriter,
    ours: Vec<std::os::unix::net::UnixStream>,
) -> std::thread::JoinHandle<()> {
    let (repo, author) = (repo.to_path_buf(), author.clone());
    for stream in &ours {
        stream.set_nonblocking(true).unwrap();
    }
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let streams = std::sync::Mutex::new(std::collections::VecDeque::from(ours));
                let attach = |who: Who| -> std::pin::Pin<
                    Box<dyn std::future::Future<Output = Attempt> + Send + '_>,
                > {
                    let stream = streams.lock().unwrap().pop_front();
                    let repo = repo.clone();
                    Box::pin(async move {
                        let Some(stream) = stream else {
                            return Attempt::Unproven("its monitors are spent".into());
                        };
                        settled(greet_at(
                            "test-socket",
                            &repo,
                            who,
                            std::time::Instant::now() + ATTACH_BUDGET,
                            move |_, _| stream.try_clone(),
                        ))
                        .unwrap_or_else(Attempt::Unproven)
                    })
                };
                crate::cli::turn_child::carry(
                    &repo,
                    &author,
                    Some(SESSION.into()),
                    crate::cli::turn_child::Owner::reading(owner),
                    &mut frames,
                    &attach,
                )
                .await;
            })
    })
}

impl Hook {
    /// Start the child carrying `report` over `ours`, the client end of a
    /// connection the test's monitor greets.
    fn carrying(
        repo: &std::path::Path,
        author: &AgentLabel,
        report: &Report,
        ours: std::os::unix::net::UnixStream,
    ) -> Self {
        Self::following(repo, author, report, vec![ours])
    }

    /// [`Hook::carrying`], over one connection per monitor the child is to
    /// find, in the order it finds them.
    fn following(
        repo: &std::path::Path,
        author: &AgentLabel,
        report: &Report,
        ours: Vec<std::os::unix::net::UnixStream>,
    ) -> Self {
        use std::io::Write;
        let (owner_read, mut stdin) = std::io::pipe().unwrap();
        let (frames_read, frames_write) = std::io::pipe().unwrap();
        writeln!(stdin, "{}", serde_json::to_string(report).unwrap()).unwrap();
        let child = carrier(repo, author, owner_read, frames_write, ours);
        Hook {
            stdin,
            frames: std::io::BufReader::new(frames_read),
            child,
        }
    }

    fn frame(&mut self) -> Frame {
        use std::io::BufRead;
        let mut line = String::new();
        self.frames.read_line(&mut line).unwrap();
        serde_json::from_str(line.trim_end()).unwrap_or_else(|e| panic!("{e}: {line:?}"))
    }

    fn answer(&mut self, result: &TurnResult) {
        use std::io::Write;
        writeln!(self.stdin, "{}", serde_json::to_string(result).unwrap()).unwrap();
    }
}

async fn next_heard(
    heard: &mut tokio::sync::mpsc::UnboundedReceiver<(u64, Heard)>,
) -> (u64, Heard) {
    tokio::time::timeout(std::time::Duration::from_secs(10), heard.recv())
        .await
        .expect("the reader reports")
        .expect("the reader is alive")
}

fn serve(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: &crate::repo_state::RepoState,
    turns: &mut Turns,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    serve_reports(
        repo,
        clients,
        Some(state),
        None,
        &[],
        &mut Default::default(),
        &Default::default(),
        turns,
    )
}

fn gathered(
    repo: &std::path::Path,
    author: &AgentLabel,
    state: &crate::repo_state::RepoState,
) -> crate::cli::turn_custody::Gathered {
    gather(
        repo,
        Some(state),
        author,
        &Default::default(),
        &mut Default::default(),
        &[],
    )
}

/// The handshake, end to end: the verdict reaches the hook while the
/// child is alive and waiting; the hook's one result is forwarded on the
/// same connection that carried the presentation, ahead of its close; and
/// the monitor credits it to that connection and commits once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verdict_and_its_result_travel_on_one_connection() {
    let (repo, author, state) = owed_repo().await;
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
    let client = admitted(theirs, repo.path(), &heard_tx).await;
    let conn = client.conn;
    let mut clients = vec![client];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let leaving = serve(repo.path(), &mut clients, &state, &mut turns);
    assert!(
        clients.is_empty(),
        "a delivered report is retired from serving"
    );

    let frame = tokio::task::spawn_blocking(move || {
        let frame = hook.frame();
        (frame, hook)
    })
    .await
    .unwrap();
    let (frame, mut hook) = frame;
    let presentation = frame.presentation().expect("delivered").clone();
    assert!(
        frame
            .rendered
            .as_deref()
            .is_some_and(|t| t.contains("scheduled work")),
        "{frame:?}"
    );
    assert!(
        !hook.child.is_finished(),
        "the child is alive, waiting on its result"
    );
    assert_eq!(turns.carrier(conn), Some(&author));

    hook.answer(&TurnResult::Delivered {
        presentation: presentation.clone(),
    });
    let (from, said) = next_heard(&mut heard).await;
    assert_eq!(
        from, conn,
        "the result came back on the carrying connection"
    );
    let Heard::Line(line) = said else {
        panic!("the result comes before the close");
    };
    let result: TurnResult = serde_json::from_str(&line).unwrap();
    assert_eq!(result.presentation(), &presentation);
    let g = gathered(repo.path(), &author, &state);
    assert!(turns.result(&author, from, result, &g).unwrap().is_some());
    let (_, closed) = next_heard(&mut heard).await;
    assert!(matches!(closed, Heard::Closed));
    hook.child.join().unwrap();
    for writer in leaving {
        let _ = writer.await;
    }
}

/// An attending Claude agent's turn-end parks its hook's wait rather than
/// ending it silently, so an event logged afterwards still wakes it, on
/// the same parked connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attending_agents_turn_end_parks_and_a_later_event_wakes_it() {
    let (repo, author, timer) = timer_repo();
    let state = folded(repo.path()).await.unwrap();
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gather(
        repo.path(),
        Some(&state),
        &author,
        &Default::default(),
        &mut Default::default(),
        &[],
    );
    attend_sleeper(&mut turns, &author, &g);
    let serve_now = |clients: &mut Vec<Client>, state, turns: &mut Turns| {
        serve_reports(
            repo.path(),
            clients,
            Some(state),
            None,
            &[],
            &mut Default::default(),
            &Default::default(),
            turns,
        )
    };

    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
    let client = admitted(theirs, repo.path(), &heard_tx).await;
    let conn = client.conn;
    let mut clients = vec![client];
    let _ = serve_now(&mut clients, &state, &mut turns);
    let (parked, mut hook) = tokio::task::spawn_blocking(move || (hook.frame(), hook))
        .await
        .unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park);
    assert_eq!(clients.len(), 1, "the parked report is held");
    assert!(turns.next_deadline().is_some(), "the attendance qualified");

    log_timer_event(repo.path(), &timer);
    let state = folded(repo.path()).await.unwrap();
    let leaving = serve_now(&mut clients, &state, &mut turns);
    assert!(clients.is_empty(), "the wake retires the report");
    let (woke, mut hook) = tokio::task::spawn_blocking(move || (hook.frame(), hook))
        .await
        .unwrap();
    let presentation = woke.presentation().expect("the event is presented").clone();
    assert!(
        woke.rendered
            .as_deref()
            .is_some_and(|t| t.contains("scheduled work")),
        "{woke:?}"
    );
    hook.answer(&TurnResult::Delivered { presentation });
    let (from, _) = next_heard(&mut heard).await;
    assert_eq!(from, conn);
    hook.child.join().unwrap();
    for writer in leaving {
        let _ = writer.await;
    }
}

/// How long a step the child owes may take before the test calls it lost.
const FOLLOWED_WITHIN: std::time::Duration = std::time::Duration::from_secs(15);

/// The artist4 case. A parked agent's monitor goes away twice under it, a
/// message for it lands while it has none, and another agent's block is
/// pending throughout. The child decides nothing about any of that: it
/// follows to the monitor that replaces them, and that monitor's fold
/// presents the message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_owed_behind_anothers_block_is_delivered_by_the_monitor_replacing_two_lost() {
    let (repo, author, _timer) = timer_repo();
    let blocks = repo.path().join(".clank/agents/codex/blocks");
    std::fs::create_dir_all(&blocks).unwrap();
    std::fs::write(blocks.join("which-way.md"), "which way?").unwrap();
    assert!(
        crate::cli::wait::blocked(repo.path(), &author),
        "the block is what a fold of the child's own would have parked on"
    );

    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs): (Vec<_>, Vec<_>) = (0..3)
        .map(|_| std::os::unix::net::UnixStream::pair().unwrap())
        .unzip();
    let mut theirs = theirs.into_iter();
    let hook = Hook::following(repo.path(), &author, &stop_report(), ours);
    let frame_of = |mut hook: Hook| async move {
        tokio::time::timeout(
            FOLLOWED_WITHIN,
            tokio::task::spawn_blocking(move || (hook.frame(), hook)),
        )
        .await
        .expect("the child prints what its monitor said")
        .unwrap()
    };
    let mut next_monitor = async || {
        tokio::time::timeout(
            FOLLOWED_WITHIN,
            admitted(theirs.next().unwrap(), repo.path(), &heard_tx),
        )
        .await
        .expect("the child follows to the next monitor")
    };

    let state = folded(repo.path()).await.unwrap();
    let mut clients = vec![next_monitor().await];
    let _ = serve(
        repo.path(),
        &mut clients,
        &state,
        &mut Turns::new(repo.path().to_path_buf()),
    );
    let (parked, hook) = frame_of(hook).await;
    assert_eq!(parked.verdict, WireVerdict::Park, "nothing is owed yet");
    clients.clear();

    drop(next_monitor().await);

    crate::inbox::deliver(
        repo.path(),
        &author,
        None,
        "2026-11-10T11:10:00Z",
        "the retakes are in",
    )
    .unwrap();
    let client = next_monitor().await;
    let conn = client.conn;
    let mut clients = vec![client];
    let state = folded(repo.path()).await.unwrap();
    let leaving = serve(
        repo.path(),
        &mut clients,
        &state,
        &mut Turns::new(repo.path().to_path_buf()),
    );
    let (woke, mut hook) = frame_of(hook).await;
    let presentation = woke
        .presentation()
        .expect("the replacement presents the message")
        .clone();
    assert!(
        woke.rendered
            .as_deref()
            .is_some_and(|t| t.contains("the retakes are in")),
        "{woke:?}"
    );
    hook.answer(&TurnResult::Delivered { presentation });
    let said = loop {
        match next_heard(&mut heard).await {
            (from, said) if from == conn => break said,
            (_, lost) => assert!(matches!(lost, Heard::Closed), "{lost:?}"),
        }
    };
    assert!(
        matches!(said, Heard::Line(_)),
        "the result comes back on the replacement's connection: {said:?}"
    );
    hook.child.join().unwrap();
    for writer in leaving {
        let _ = writer.await;
    }
}

/// Where work arriving after the park comes from.
#[derive(Clone, Copy, Debug)]
enum Later {
    /// An event appended to the agent's log.
    Logged,
    /// An emission no log keeps, held by the monitor until delivered.
    Pending,
}

/// Attendance holds recovery only over work the session received, from
/// either source of later work. Work it received stays held; a healthy
/// parked hook still owns the wake of work arriving after it parked; and
/// once that hook is lost, the new work leaves the agent obligated and
/// unquieted, so recovery's facts, the deadline's gate and the last look
/// before a send all let recovery wake it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attendance_holds_recovery_only_over_work_the_session_received() {
    recovery_after_a_lost_park(Later::Logged).await;
    recovery_after_a_lost_park(Later::Pending).await;
}

async fn recovery_after_a_lost_park(later: Later) {
    use crate::cli::recovery::Gate;
    let (repo, author, timer) = timer_repo();
    log_timer_event(repo.path(), &timer);
    let state = folded(repo.path()).await.unwrap();
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gather(
        repo.path(),
        Some(&state),
        &author,
        &Default::default(),
        &mut Default::default(),
        &[],
    );
    let shown = turns.answer(&author, 1, &stop_report(), true, &g).unwrap();
    let presentation = shown.presentation().expect("presented").clone();
    turns
        .result(&author, 1, TurnResult::Delivered { presentation }, &g)
        .unwrap();
    attend_run(&mut turns, &author, "sleeper", &g);
    let parked = turns.answer(&author, 2, &stop_report(), true, &g).unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park);

    type Pending = [(AgentLabel, WaitItem)];
    let facts = |state: &crate::repo_state::RepoState,
                 pending: &Pending,
                 clients: &[Client],
                 turns: &mut Turns| {
        let [facts] = recovery_facts(
            repo.path(),
            state,
            clients,
            &Default::default(),
            &mut Default::default(),
            pending,
            turns,
        )
        .try_into()
        .unwrap();
        facts
    };
    let look = |state: &crate::repo_state::RepoState,
                pending: &Pending,
                clients: &[Client],
                turns: &mut Turns| {
        let (reply, _) = tokio::sync::oneshot::channel();
        let ask = Authorize {
            author: author.clone(),
            attempt: crate::cli::recovery::AttemptRef {
                session: SESSION.into(),
                opened_by: "t1".into(),
                n: 1,
            },
            screen: None,
            transport: crate::cli::recovery::Transport::Typed,
            state: Some(state.clone()),
            reply,
        };
        let last = last_look(
            repo.path(),
            clients,
            &Default::default(),
            pending,
            turns,
            &ask,
        )
        .map(|(gate, _)| gate);
        let due = due_gate(
            repo.path(),
            clients,
            turns,
            Some(state),
            &Default::default(),
            &mut Default::default(),
            pending,
            &author,
            SESSION,
        );
        assert_eq!(
            last,
            Some(due),
            "{later:?}: the last look agrees with the deadline's gate"
        );
        last
    };
    let held = facts(&state, &[], &[], &mut turns);
    assert!(held.obligated && held.attending, "received work: {held:?}");
    assert_eq!(look(&state, &[], &[], &mut turns), Some(Gate::Hold));

    let (heard_tx, _heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
    let mut clients = vec![admitted(theirs, repo.path(), &heard_tx).await];
    let _ = serve_reports(
        repo.path(),
        &mut clients,
        Some(&state),
        None,
        &[],
        &mut Default::default(),
        &Default::default(),
        &mut turns,
    );
    let (frame, hook) = tokio::task::spawn_blocking(move || {
        let mut hook = hook;
        (hook.frame(), hook)
    })
    .await
    .unwrap();
    assert_eq!(frame.verdict, WireVerdict::Park);

    let (now, pending) = match later {
        Later::Logged => {
            log_timer_occurrence(repo.path(), &timer, 200, "later work");
            (folded(repo.path()).await.unwrap(), Vec::new())
        }
        Later::Pending => (state.clone(), transient(&author)),
    };
    let owned = facts(&now, &pending, &clients, &mut turns);
    assert!(
        owned.delivering && !owned.obligated,
        "{later:?}: the parked hook owns it: {owned:?}"
    );
    assert_eq!(
        look(&now, &pending, &clients, &mut turns),
        Some(Gate::Deliverer)
    );

    clients.clear();
    drop(hook.stdin);
    hook.child.join().unwrap();
    let lost = facts(&now, &pending, &[], &mut turns);
    assert!(
        lost.obligated && !lost.attending,
        "{later:?}: unseen work is recoverable: {lost:?}"
    );
    assert_eq!(look(&now, &pending, &[], &mut turns), Some(Gate::Go));
}

/// EOF on the hook's pipe before a result is a failed emit: the child
/// says so on the carrying connection, and the presentation fails
/// terminally with nothing committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pipe_closed_before_a_result_is_a_failed_emit() {
    let (repo, author, state) = owed_repo().await;
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
    let client = admitted(theirs, repo.path(), &heard_tx).await;
    let conn = client.conn;
    let mut clients = vec![client];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let _leaving = serve(repo.path(), &mut clients, &state, &mut turns);
    let (mut hook, frame) = tokio::task::spawn_blocking(move || {
        let mut hook = hook;
        let frame = hook.frame();
        (hook, frame)
    })
    .await
    .unwrap();
    let presentation = frame.presentation().unwrap().clone();
    let Hook { stdin, child, .. } = std::mem::replace(
        &mut hook,
        Hook {
            stdin: std::io::pipe().unwrap().1,
            frames: std::io::BufReader::new(std::io::pipe().unwrap().0),
            child: std::thread::spawn(|| {}),
        },
    );
    drop(stdin);
    let (from, said) = next_heard(&mut heard).await;
    assert_eq!(from, conn);
    let Heard::Line(line) = said else {
        panic!("an explicit failure before the close");
    };
    let result: TurnResult = serde_json::from_str(&line).unwrap();
    assert!(
        matches!(&result, TurnResult::EmitFailed { presentation: p, .. } if p == &presentation)
    );
    let g = gathered(repo.path(), &author, &state);
    assert!(turns.result(&author, from, result, &g).unwrap().is_none());
    child.join().unwrap();
}

/// A carrier that disappears — the hook and its child killed — closes its
/// connection with no result: the monitor reads that as a failed emit,
/// and commits nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_killed_carrier_fails_its_presentation_and_commits_nothing() {
    let (repo, author, state) = owed_repo().await;
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let hello = Hello {
        proto: PROTO,
        repo: repo.path().to_string_lossy().to_string(),
        who: Who::Agent {
            author: author.to_string(),
            holder: Some(Holder {
                tool: clank_core::Tool::Claude,
                session: SESSION.into(),
            }),
            has_cli_sources: false,
            turn_end: Some(stop_report()),
        },
    };
    {
        use std::io::Write;
        writeln!(&ours, "{}", serde_json::to_string(&hello).unwrap()).unwrap();
    }
    let client = admitted(theirs, repo.path(), &heard_tx).await;
    let conn = client.conn;
    let mut clients = vec![client];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let _leaving = serve(repo.path(), &mut clients, &state, &mut turns);
    assert_eq!(turns.carrier(conn), Some(&author));
    drop(ours);
    let (from, said) = next_heard(&mut heard).await;
    assert_eq!((from, matches!(said, Heard::Closed)), (conn, true));
    let g = gathered(repo.path(), &author, &state);
    turns.closed(&author, from, &g);
    assert_eq!(turns.carrier(conn), None, "failed, terminally");
    let row = crate::cli::github::log::EventLog::open(
        &crate::agent_store::agents_root(repo.path()).join("claude/events"),
        &clank_core::agent_config::TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: "check".into(),
            repeat_every: None,
            due_at: "1970-01-01T00:01:40Z".into(),
            content: None,
        }
        .source_key(None),
    )
    .unwrap()
    .read_rows()
    .unwrap()
    .unwrap()
    .rows
    .remove(0);
    assert!(row.woken_at.is_none(), "nothing was committed");
}

/// A result that arrives on any other connection — another report's, a
/// stale hook's — is not the carrier's and cannot settle the
/// presentation; the carrier's own still can.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_on_a_foreign_connection_is_never_credited() {
    let (repo, author, state) = owed_repo().await;
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let hello = |report: Report| Hello {
        proto: PROTO,
        repo: repo.path().to_string_lossy().to_string(),
        who: Who::Agent {
            author: author.to_string(),
            holder: Some(Holder {
                tool: clank_core::Tool::Claude,
                session: SESSION.into(),
            }),
            has_cli_sources: false,
            turn_end: Some(report),
        },
    };
    let connect = |report: Report| {
        use std::io::Write;
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        writeln!(&ours, "{}", serde_json::to_string(&hello(report)).unwrap()).unwrap();
        (ours, theirs)
    };
    let (carrier_end, theirs) = connect(stop_report());
    let carrier = admitted(theirs, repo.path(), &heard_tx).await;
    let carrier_conn = carrier.conn;
    let mut clients = vec![carrier];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let _leaving = serve(repo.path(), &mut clients, &state, &mut turns);
    let presentation = {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(carrier_end.try_clone().unwrap());
        let mut welcome = String::new();
        reader.read_line(&mut welcome).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let Note::Verdict(frame) = serde_json::from_str(line.trim_end()).unwrap() else {
            panic!("{line}");
        };
        frame.presentation().unwrap().clone()
    };

    let (foreign_end, theirs) = connect(stop_report());
    let foreign = admitted(theirs, repo.path(), &heard_tx).await;
    let foreign_conn = foreign.conn;
    assert_ne!(foreign_conn, carrier_conn);
    {
        use std::io::Write;
        let result = TurnResult::Delivered {
            presentation: presentation.clone(),
        };
        writeln!(&foreign_end, "{}", serde_json::to_string(&result).unwrap()).unwrap();
    }
    let (from, said) = next_heard(&mut heard).await;
    assert_eq!(from, foreign_conn);
    let Heard::Line(line) = said else {
        panic!("a line");
    };
    assert_eq!(
        turns.carrier(from),
        None,
        "the foreign connection carries nothing"
    );
    let g = gathered(repo.path(), &author, &state);
    assert!(
        turns
            .result(&author, from, serde_json::from_str(&line).unwrap(), &g)
            .is_err(),
        "a result from another connection is refused"
    );
    assert_eq!(
        turns.carrier(carrier_conn),
        Some(&author),
        "still in flight"
    );
    assert!(
        turns
            .result(
                &author,
                carrier_conn,
                TurnResult::Delivered { presentation },
                &g
            )
            .unwrap()
            .is_some()
    );
}

/// A typist that records what it was asked to type, or cannot find the
/// pane at all.
struct Recorded {
    typed: std::sync::Mutex<Vec<String>>,
    lost: bool,
}

impl Typist for Recorded {
    fn locate(
        &self,
        _repo: std::path::PathBuf,
        _author: AgentLabel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Located, String>> + Send>> {
        let lost = self.lost;
        Box::pin(async move {
            if lost {
                return Err("its pane was not found".into());
            }
            Ok((
                crate::cli::pane_target::Target {
                    session: "test".into(),
                    pane: "terminal_1".into(),
                    tool: clank_core::Tool::Claude,
                    repo: "/test".into(),
                    label: AgentLabel::parse("test").unwrap(),
                },
                Some(EMPTY_INPUT.into()),
            ))
        })
    }

    fn submit(
        &self,
        _target: crate::cli::pane_target::Target,
        text: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::cli::monitor::Submitted> + Send>>
    {
        self.typed.lock().unwrap().push(text);
        Box::pin(async { Ok(crate::cli::pane_input::Delivered::Whole) })
    }
}

/// Deliver `due` through `typist`, answering the last look with `look`.
async fn check_in_through(
    repo: &std::path::Path,
    due: crate::cli::turn_custody::CheckInDue,
    typist: std::sync::Arc<Recorded>,
    look: impl FnOnce(&CheckInAsk) -> Result<String, String>,
) -> CheckedIn {
    let (asks, mut asked) = tokio::sync::mpsc::unbounded_channel();
    let (done, mut finished) = tokio::sync::mpsc::unbounded_channel();
    let delivering = tokio::spawn(deliver_check_in(
        repo.to_path_buf(),
        due,
        typist,
        asks,
        done,
    ));
    if let Some(ask) = asked.recv().await {
        let looked = look(&ask);
        let _ = ask.reply.send(looked);
    }
    delivering.await.unwrap();
    finished.recv().await.unwrap()
}

/// A declaration owes one typed check-in at its deadline; with the route
/// lost by then, the check-in is recorded undeliverable and recovery takes
/// the agent back.
#[tokio::test]
async fn a_declaration_is_checked_in_on_by_typing_once() {
    let (repo, author, state) = owed_repo().await;
    let mut turns = Turns::new(repo.path().to_path_buf());
    let g = gather(
        repo.path(),
        Some(&state),
        &author,
        &Default::default(),
        &mut Default::default(),
        &[],
    );
    attend_sleeper(&mut turns, &author, &g);
    let listed = stop_report();
    let frame = turns.answer(&author, 1, &listed, true, &g).unwrap();
    assert!(
        frame.presentation().is_some(),
        "owed work it never received is delivered, attending or not: {frame:?}"
    );
    let due_at = turns.next_deadline().expect("a check-in is owed");

    for lost in [false, true] {
        let mut turns = Turns::new(repo.path().to_path_buf());
        let at_due = gather(
            repo.path(),
            Some(&state),
            &author,
            &Default::default(),
            &mut Default::default(),
            &[],
        )
        .at(due_at);
        let due = turns.check_in(&author, &at_due).expect("due");
        let typist = std::sync::Arc::new(Recorded {
            typed: Default::default(),
            lost,
        });
        let done = check_in_through(repo.path(), due.clone(), typist.clone(), |_| {
            Ok("clank check-in: as the last look says".into())
        })
        .await;
        let typed = typist.typed.lock().unwrap().clone();
        match lost {
            false => {
                assert!(done.outcome.is_ok());
                assert_eq!(
                    typed,
                    vec!["clank check-in: as the last look says".to_string()],
                    "typed once, as the last look answered"
                );
            }
            true => {
                assert!(typed.is_empty());
                let why = done.outcome.expect_err("undeliverable");
                turns.check_in_failed(&author, done.check_ins, why, &at_due);
                let shown = turns.projection(&author, &at_due).attended;
                assert!(
                    matches!(shown.as_slice(), [a] if a.check_in.as_deref().is_some_and(|c| c.starts_with("check-in undeliverable"))),
                    "{shown:?}"
                );
                assert_eq!(
                    turns.recovery_quiet(&author, &at_due),
                    None,
                    "recovery takes over"
                );
            }
        }
        // The next run of this loop restarts from the persisted attendance,
        // whose check-in is still armed: rewind it.
        if !lost {
            let path = crate::agent_store::agents_root(repo.path()).join("claude/turn.json");
            let mut p: clank_core::turn_policy::Persisted =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            for entry in &mut p.attendances {
                entry.check_in = clank_core::turn_policy::CheckIn::Armed;
            }
            std::fs::write(&path, serde_json::to_vec(&p).unwrap()).unwrap();
        }
    }
}

/// A check-in refused at its last look is not typed.
#[tokio::test]
async fn a_check_in_its_last_look_refuses_is_not_typed() {
    let repo = TempRepo::new();
    let typist = std::sync::Arc::new(Recorded {
        typed: Default::default(),
        lost: false,
    });
    let due = crate::cli::turn_custody::CheckInDue {
        author: AgentLabel::parse("claude").unwrap(),
        session: clank_core::SessionId::parse(SESSION).unwrap(),
        check_ins: vec![clank_core::turn_policy::CheckInRef {
            key: clank_core::turn_policy::EntryKey::Name("sleeper".into()),
            since: 0,
        }],
    };
    let done = check_in_through(repo.path(), due, typist.clone(), |_| Err("refused".into())).await;
    assert!(done.outcome.is_err());
    assert!(typist.typed.lock().unwrap().is_empty());
}

/// The last look needs recovery to hold the agent as resumed by typing: an
/// agent recovery does not hold is never typed into.
#[test]
fn the_last_look_needs_a_harness_resumed_by_typing() {
    let repo = TempRepo::new();
    let driver =
        crate::cli::recovery::Driver::new(repo.path().to_path_buf(), "/nonexistent".into());
    let (reply, _answer) = tokio::sync::oneshot::channel();
    let author = AgentLabel::parse("claude").unwrap();
    let ask = CheckInAsk {
        author: author.clone(),
        session: clank_core::SessionId::parse(SESSION).unwrap(),
        check_ins: vec![clank_core::turn_policy::CheckInRef {
            key: clank_core::turn_policy::EntryKey::Name("sleeper".into()),
            since: 0,
        }],
        screen: (clank_core::Tool::Claude, Some(String::new())),
        reply,
    };
    let g = crate::cli::turn_custody::Gathered::of(repo.path(), &author, None);
    assert!(
        may_check_in_now(
            repo.path(),
            &driver,
            &mut Turns::new(repo.path().to_path_buf()),
            &g,
            &ask
        )
        .is_err()
    );
}

/// Recovery's transport rides the monitor's connection: a turn the harness
/// reported is recorded by the monitor, which answers how a retry would
/// reach the harness, and a helper's answer is taken; either from a
/// session no longer bound is refused and records nothing.
#[tokio::test]
async fn recovery_requests_are_carried_out_for_a_bound_session_only() {
    let (repo, author, _) = owed_repo().await;
    let mut recovery =
        crate::cli::recovery::Driver::new(repo.path().to_path_buf(), "/nonexistent".into());
    let record = crate::cli::recovery::observed_path(
        &crate::agent_store::agents_root(repo.path()).join(author.as_str()),
    );
    let turn = serde_json::json!({
        "session": SESSION, "turn": "started", "id": "u1", "class": "unknown", "at": 1,
    });
    let observe = |session: &str| Control::Observe {
        author: author.to_string(),
        tool: clank_core::Tool::Claude,
        session: session.into(),
        turn: turn.clone(),
    };
    let wire: Control =
        serde_json::from_str(&serde_json::to_string(&observe(SESSION)).unwrap()).unwrap();
    assert_eq!(wire, observe(SESSION), "it travels whole");
    let (said, acts) = recovery_control(repo.path(), &mut recovery, &wire, 0).unwrap();
    assert_eq!(
        said,
        crate::cli::recovery::retry_announcement(clank_core::Tool::Claude)
    );
    assert!(acts.is_empty());
    let recorded = || std::fs::read_to_string(&record).unwrap_or_default();
    assert_eq!(recorded(), format!("{turn}\n"));
    assert_eq!(
        recovery_control(repo.path(), &mut recovery, &observe("session-gone"), 0).map(|(s, _)| s),
        Err("this session no longer holds the label".to_string())
    );
    assert_eq!(recorded(), format!("{turn}\n"), "nothing more recorded");

    let reply = |session: &str| Control::RecoveryReply {
        author: author.to_string(),
        tool: clank_core::Tool::Claude,
        reply: crate::cli::recovery::Reply {
            session: session.into(),
            opened_by: "u1".into(),
            n: 1,
            answer: crate::cli::recovery::ReplyAnswer::Submitted,
            started: None,
        },
    };
    let wire: Control =
        serde_json::from_str(&serde_json::to_string(&reply(SESSION)).unwrap()).unwrap();
    assert_eq!(wire, reply(SESSION));
    assert_eq!(
        recovery_control(repo.path(), &mut recovery, &wire, 0).map(|(s, _)| s),
        Ok("recorded".to_string())
    );
    assert_eq!(
        recovery_control(repo.path(), &mut recovery, &reply("session-gone"), 0).map(|(s, _)| s),
        Err("this session no longer holds the label".to_string())
    );
}

/// Attend and clear requests at the monitor's control seam travel whole,
/// two runs with one `--desc` are two entries told apart by their
/// processes, a declaration under a run's `--desc` renews that run, and a
/// run's rollback clears only its own entry.
#[tokio::test]
async fn attend_and_clear_requests_are_scoped_to_their_entry() {
    let (repo, author, state) = owed_repo().await;
    let mut turns = Turns::new(repo.path().to_path_buf());
    let now = || {
        gather(
            repo.path(),
            Some(&state),
            &author,
            &Default::default(),
            &mut Default::default(),
            &[],
        )
    };
    let session = clank_core::SessionId::parse(SESSION).unwrap();
    let proof = |pid: i32| {
        crate::cli::turn_custody::shell_proof(
            pid,
            &crate::proc_identity::token_for(pid).expect("a live process"),
        )
    };
    let (me, parent) = (
        std::process::id() as i32,
        std::os::unix::process::parent_id() as i32,
    );
    let run = |pid: i32| Control::Run {
        author: author.to_string(),
        session: session.clone(),
        desc: "test run".into(),
        expect_ms: 600_000,
        proof: proof(pid),
    };
    let clear = |which| Control::Clear {
        author: author.to_string(),
        session: session.clone(),
        which,
    };
    let control = |turns: &mut Turns, c: &Control| {
        let wire: Control = serde_json::from_str(&serde_json::to_string(c).unwrap()).unwrap();
        assert_eq!(&wire, c, "it travels whole");
        turn_control(repo.path(), turns, &wire, |_| now()).map(|(_, said)| said)
    };
    let said = control(&mut turns, &run(me)).unwrap();
    assert!(said.contains("until its process ends"), "{said}");
    assert!(control(&mut turns, &run(parent)).is_ok());
    let entries = |turns: &Turns| turns.state(&author).unwrap().attendances.len();
    assert_eq!(entries(&turns), 2, "one entry per registered process");
    let said = control(
        &mut turns,
        &Control::Attend {
            author: author.to_string(),
            session: session.clone(),
            name: "test run".into(),
            desc: None,
            expect_ms: 1_200_000,
        },
    )
    .unwrap();
    assert!(said.starts_with("renewed"), "{said}");
    assert_eq!(entries(&turns), 2, "the runs were renewed, nothing added");
    attend_named(&mut turns, &author, "sleeper", &now());
    assert_eq!(entries(&turns), 3);

    assert_eq!(
        control(
            &mut turns,
            &clear(clank_core::turn_policy::Clearing::Run(proof(parent)))
        ),
        Ok("no longer attending 1 thing; any check-in it owed is cancelled".into())
    );
    assert_eq!(entries(&turns), 2, "only the run's own entry");
    assert!(control(&mut turns, &clear(clank_core::turn_policy::Clearing::All)).is_ok());
    assert_eq!(entries(&turns), 0);
}

/// The loop's step at an attendance deadline serves the agent's parked
/// wait again, with no repo change and no typed route: the work it had
/// received and the attendance quieted is presented once the deadline
/// passes. The check-in is still owed beside it, and the work comes back
/// without it ever being typed.
#[tokio::test]
async fn a_parked_wait_is_presented_its_quieted_work_at_the_attendance_deadline() {
    let (repo, author, state) = owed_repo().await;
    let then = crate::cli::turn_custody::now_ms() - 120_000;
    let g = gathered(repo.path(), &author, &state).at(then);
    let mut turns = Turns::new(repo.path().to_path_buf());
    let first = turns.answer(&author, 1, &stop_report(), true, &g).unwrap();
    let presentation = first.presentation().expect("the owed work").clone();
    assert!(
        turns
            .result(&author, 1, TurnResult::Delivered { presentation }, &g)
            .unwrap()
            .is_some()
    );
    attend_sleeper(&mut turns, &author, &g);
    let parked = turns
        .answer(&author, 2, &stop_report(), true, &g.at(then + 1))
        .unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park, "received work is quiet");
    let deadline = turns.next_deadline().expect("the quiet arms a check-in");
    assert_eq!(deadline, then + 60_000);

    let (out, mut queue) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        who: Served::Agent {
            author: author.clone(),
        },
        holder: None,
        out,
        writer: None,
        farewell: None,
        shown: Default::default(),
        conn: 2,
        report: Some(Reporting {
            report: stop_report(),
            stepped: true,
            reader: None,
        }),
    }];
    let mut recovery =
        crate::cli::recovery::Driver::new(repo.path().to_path_buf(), "/nonexistent".into());
    let mut round = |clients: &mut Vec<Client>, turns: &mut Turns, now| {
        deadline_round(
            repo.path(),
            clients,
            Some(&state),
            &[],
            &Default::default(),
            turns,
            &mut recovery,
            now,
        )
    };

    let (dues, _) = round(&mut clients, &mut turns, deadline - 1);
    assert!(dues.is_empty());
    assert!(queue.try_recv().is_err(), "nothing before the deadline");
    assert_eq!(clients.len(), 1);

    let (dues, _) = round(&mut clients, &mut turns, deadline);
    assert_eq!(dues.len(), 1, "the check-in is still owed");
    let note = queue.try_recv().expect("the parked wait was served");
    assert!(
        note.contains("deliver") && note.contains("scheduled work"),
        "{note}"
    );
    assert!(clients.is_empty(), "a delivery retires the parked report");
    assert_eq!(turns.next_deadline(), None, "not due again");
}

/// `author`'s Claude record holds a prompt and an answer with no end
/// marker: the record alone reads the turn as running.
fn answered_without_an_end(repo: &std::path::Path, author: &AgentLabel) {
    let record = repo.join("record.jsonl");
    std::fs::write(
        &record,
        concat!(
            r#"{"type":"user","promptId":"P","message":{"role":"user","content":"review it"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"FINISHED"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    crate::agent_store::update_agent_config(repo, author, |config| {
        config.session.as_mut().unwrap().transcript = Some(record.display().to_string());
    })
    .unwrap();
    crate::cli::recovery::tests::enable(
        clank_core::Tool::Claude,
        Some(crate::cli::recovery::Transport::Typed),
    );
}

/// The incident through the monitor's own admission, reader and seams: a
/// Stop parks, the round registers it, its hook dies having been handed
/// nothing, and the close the reader hears leaves the agent unwatched,
/// nudged once work is owed and the idle settle has passed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_park_whose_hook_dies_leaves_its_agent_unwatched_and_nudged() {
    let (repo, author, timer) = timer_repo();
    answered_without_an_end(repo.path(), &author);
    let state = folded(repo.path()).await.unwrap();
    let mut recovery = recovering(repo.path(), &author);
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
    let client = admitted(theirs, repo.path(), &heard_tx).await;
    let conn = client.conn;
    let mut clients = vec![client];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let _leaving = serve(repo.path(), &mut clients, &state, &mut turns);
    let (parked, hook) = tokio::task::spawn_blocking(move || {
        let mut hook = hook;
        (hook.frame(), hook)
    })
    .await
    .unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park);
    let round = |clients: &mut Vec<Client>,
                 turns: &mut Turns,
                 recovery: &mut crate::cli::recovery::Driver| {
        tend(
            repo.path(),
            clients,
            Some(&state),
            &[],
            &Default::default(),
            turns,
            recovery,
        )
    };
    let _ = round(&mut clients, &mut turns, &mut recovery);

    drop(hook.stdin);
    hook.child.join().unwrap();
    let (from, said) = next_heard(&mut heard).await;
    assert_eq!((from, matches!(said, Heard::Closed)), (conn, true));
    let _ = ended(
        from,
        repo.path(),
        &mut clients,
        Some(&state),
        &[],
        &Default::default(),
        &mut turns,
        &mut recovery,
    );
    assert!(turns.state(&author).unwrap().unwatched.is_some());

    log_timer_event(repo.path(), &timer);
    let state = folded(repo.path()).await.unwrap();
    let now = crate::cli::recovery::unix_now();
    let facts = recovery_facts(
        repo.path(),
        &state,
        &clients,
        &Default::default(),
        &mut Default::default(),
        &[],
        &mut turns,
    );
    recovery.reconcile(facts, now);
    let dir = crate::agent_store::agents_root(repo.path()).join(author.as_str());
    assert_eq!(
        crate::cli::agent_state::load(&dir).unwrap().activity,
        crate::cli::agent_state::Activity::Unwatched
    );
    let nudges = |recovery: &mut crate::cli::recovery::Driver, at| {
        recovery
            .due(at, |_, _| crate::cli::recovery::Gate::Go)
            .into_iter()
            .filter(|a| matches!(a, crate::cli::recovery::Act::Dispatch { .. }))
            .count()
    };
    assert_eq!(nudges(&mut recovery, now + 179), 0);
    assert_eq!(nudges(&mut recovery, now + 180), 1);
}

/// A park the monitor answered with work its hook delivered records no
/// loss when that hook's connection then closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_park_that_delivered_records_no_loss() {
    let (repo, author, timer) = timer_repo();
    answered_without_an_end(repo.path(), &author);
    let state = folded(repo.path()).await.unwrap();
    let mut recovery = recovering(repo.path(), &author);
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let hook = Hook::carrying(repo.path(), &author, &stop_report(), ours);
    let client = admitted(theirs, repo.path(), &heard_tx).await;
    let conn = client.conn;
    let mut clients = vec![client];
    let mut turns = Turns::new(repo.path().to_path_buf());
    let _leaving = serve(repo.path(), &mut clients, &state, &mut turns);
    let (parked, hook) = tokio::task::spawn_blocking(move || {
        let mut hook = hook;
        (hook.frame(), hook)
    })
    .await
    .unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park);
    let _ = tend(
        repo.path(),
        &mut clients,
        Some(&state),
        &[],
        &Default::default(),
        &mut turns,
        &mut recovery,
    );

    log_timer_event(repo.path(), &timer);
    let state = folded(repo.path()).await.unwrap();
    let _leaving = serve(repo.path(), &mut clients, &state, &mut turns);
    let (woke, mut hook) = tokio::task::spawn_blocking(move || {
        let mut hook = hook;
        (hook.frame(), hook)
    })
    .await
    .unwrap();
    let presentation = woke.presentation().expect("presented").clone();
    hook.answer(&TurnResult::Delivered { presentation });
    let (from, Heard::Line(line)) = next_heard(&mut heard).await else {
        panic!("the result");
    };
    let g = gathered(repo.path(), &author, &state);
    turns
        .result(&author, from, serde_json::from_str(&line).unwrap(), &g)
        .unwrap();
    drop(hook.stdin);
    hook.child.join().unwrap();
    let (from, said) = next_heard(&mut heard).await;
    assert_eq!((from, matches!(said, Heard::Closed)), (conn, true));
    let _ = ended(
        from,
        repo.path(),
        &mut clients,
        Some(&state),
        &[],
        &Default::default(),
        &mut turns,
        &mut recovery,
    );
    assert_eq!(turns.state(&author).unwrap().unwatched, None);
}

/// The hook's host for a PostCompact: Claude's payload on stdin, each
/// carrier started in this process and its monitor end handed to the test,
/// and the exit code kept.
struct Compacting {
    stdin: String,
    spawner: Reaching,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: Option<i32>,
}

struct Reaching(std::sync::mpsc::Sender<std::os::unix::net::UnixStream>);

impl crate::cli::stop_hook::Spawn for Reaching {
    fn spawn(
        &self,
        repo: &std::path::Path,
        label: &AgentLabel,
        _: &str,
        _: clank_core::Tool,
        report: &Report,
    ) -> Result<crate::cli::stop_hook::OwnedWait, String> {
        use tokio::net::unix::pipe;
        let (owner_read, owner_write) = std::io::pipe().unwrap();
        let (frames_read, frames_write) = std::io::pipe().unwrap();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        carrier(repo, label, owner_read, frames_write, vec![ours]);
        self.0.send(theirs).unwrap();
        crate::cli::stop_hook::OwnedWait::over(
            None,
            Box::new(pipe::Sender::from_owned_fd(owner_write.into()).unwrap()),
            Box::new(pipe::Receiver::from_owned_fd(frames_read.into()).unwrap()),
            None,
            report,
        )
    }
}

impl crate::cli::stop_hook::Host for Compacting {
    fn input(&mut self) -> Result<clank_core::HookInput, String> {
        serde_json::from_str(&self.stdin).map_err(|e| e.to_string())
    }
    fn spawner(&self) -> &dyn crate::cli::stop_hook::Spawn {
        &self.spawner
    }
    fn streams(&mut self) -> (&mut dyn std::io::Write, &mut dyn std::io::Write) {
        (&mut self.stdout, &mut self.stderr)
    }
    fn exit(&mut self, code: i32) {
        self.exit = Some(code);
    }
}

/// The PostCompact entry `clank setup` installs, run from its own argv on
/// Claude's payload over the idle-compaction record: its wait is admitted
/// as the agent's one park, replacing the Stop's, and parks; work logged
/// later is delivered on it, the hook exits 2, and its receipt commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_rearms_the_park_that_replaces_the_stops() {
    let (repo, author, timer) = timer_repo();
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["init", "--quiet"])
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let root = dunce::canonicalize(repo.path()).unwrap();
    let root = root.as_path();
    let state = folded(root).await.unwrap();
    let mut turns = Turns::new(root.to_path_buf());
    let mut recovery = crate::cli::recovery::Driver::new("/".into(), "/nonexistent-home".into());
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel();
    let mut clients = Vec::new();
    let hosted = Default::default();
    let mut admit = |clients: &mut Vec<Client>, client: Client, turns: &mut Turns| {
        let mut leaving = admit_into(
            root,
            clients,
            client,
            Some(&state),
            &mut Vec::new(),
            &hosted,
        );
        leaving.extend(tend(
            root,
            clients,
            Some(&state),
            &[],
            &hosted,
            turns,
            &mut recovery,
        ));
        leaving
    };

    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    let mut stop = Hook::carrying(root, &author, &stop_report(), ours);
    let client = admitted(theirs, root, &heard_tx).await;
    let mut leaving = admit(&mut clients, client, &mut turns);
    leaving.extend(serve(root, &mut clients, &state, &mut turns));
    let (parked, mut stop) = tokio::task::spawn_blocking(move || (stop.frame(), stop))
        .await
        .unwrap();
    assert_eq!(parked.verdict, WireVerdict::Park);

    let profile = tempfile::tempdir().unwrap();
    crate::cli::setup::install_provider(
        &profile.path().join("home"),
        clank_core::Tool::Claude,
        profile.path(),
        true,
        false,
        false,
        &mut Vec::new(),
    )
    .unwrap();
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(profile.path().join("settings.json")).unwrap(),
    )
    .unwrap();
    let [entry] = crate::cli::setup::clank_hook_entries(&settings, "PostCompact")[..] else {
        panic!("one PostCompact entry: {settings}");
    };
    assert_eq!(entry["asyncRewake"], true, "{entry}");
    let argv = shell_words::split(entry["command"].as_str().unwrap()).unwrap();
    let crate::cli::command::Command::StopHook(args) =
        <crate::cli::command::Cli as clap::Parser>::try_parse_from(argv)
            .unwrap()
            .command
    else {
        panic!("the entry runs stop-hook");
    };
    let transcript = root.join("t.jsonl");
    std::fs::write(
        &transcript,
        include_str!("../../tests/fixtures/claude_idle_compaction.jsonl"),
    )
    .unwrap();
    let (carriers, reached) = std::sync::mpsc::channel();
    let mut host = Compacting {
        stdin: serde_json::json!({
            "session_id": SESSION,
            "transcript_path": transcript,
            "cwd": root,
            "hook_event_name": "PostCompact",
            "trigger": "auto",
            "compact_summary": "summary",
        })
        .to_string(),
        spawner: Reaching(carriers),
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit: None,
    };
    let hook = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(crate::cli::stop_hook::run(args, &mut host))
            .unwrap();
        host
    });
    let theirs = tokio::task::spawn_blocking(move || {
        reached.recv_timeout(std::time::Duration::from_secs(10))
    })
    .await
    .unwrap()
    .expect("the PostCompact hook carries a wait");
    let client = admitted(theirs, root, &heard_tx).await;
    let conn = client.conn;
    assert!(matches!(
        client.report.as_ref().map(|r| &r.report),
        Some(Report::Compacted { stop, .. })
            if stop.place.turn.as_deref() == Some("39853d54-8cd0-40a0-ac97-a8fd8111f7a6")
    ));
    leaving.extend(admit(&mut clients, client, &mut turns));
    assert_eq!(
        clients.iter().map(|c| c.conn).collect::<Vec<_>>(),
        [conn],
        "one park per agent session: the newer"
    );
    let (replaced, stop) = tokio::task::spawn_blocking(move || (stop.frame(), stop))
        .await
        .unwrap();
    assert_eq!(
        replaced.verdict,
        WireVerdict::Silent {
            why: clank_core::SilentReason::StaleBinding
        }
    );
    leaving.extend(serve(root, &mut clients, &state, &mut turns));
    assert_eq!(clients.len(), 1, "the re-armed report parks");

    log_timer_event(root, &timer);
    let state = folded(root).await.unwrap();
    leaving.extend(serve(root, &mut clients, &state, &mut turns));
    assert!(clients.is_empty(), "the wake retires the report");
    let said = loop {
        match next_heard(&mut heard).await {
            (from, said) if from == conn => break said,
            _ => {}
        }
    };
    let Heard::Line(line) = said else {
        panic!("the hook's result comes before the close");
    };
    let result: TurnResult = serde_json::from_str(&line).unwrap();
    assert!(matches!(result, TurnResult::Delivered { .. }), "{result:?}");
    let from = conn;
    let g = gathered(root, &author, &state);
    let commit = turns.result(&author, from, result, &g).unwrap();
    assert!(commit.is_some(), "the delivery commits its receipt");

    let host = tokio::task::spawn_blocking(move || hook.join().unwrap())
        .await
        .unwrap();
    assert_eq!(host.exit, Some(clank_core::CLAUDE_CONTINUATION_EXIT));
    assert!(
        String::from_utf8_lossy(&host.stderr).contains("scheduled work"),
        "{:?}",
        String::from_utf8_lossy(&host.stderr)
    );
    stop.child.join().unwrap();
    for writer in leaving {
        let _ = writer.await;
    }
}

/// A compaction's re-arm is the park again, not a Stop: the run the agent
/// attends still quiets the work the session received, and its check-in
/// is owed when it was.
#[tokio::test]
async fn a_compactions_rearm_keeps_what_the_attendance_quiets() {
    let (repo, author, timer) = timer_repo();
    let state = folded(repo.path()).await.unwrap();
    let (mut turns, due) = attended(repo.path(), &author, &state);
    log_timer_event(repo.path(), &timer);
    let state = folded(repo.path()).await.unwrap();
    let g = gathered(repo.path(), &author, &state);
    let shown = turns.answer(&author, 2, &stop_report(), true, &g).unwrap();
    let presentation = shown.presentation().expect("the work is presented").clone();
    turns
        .result(&author, 2, TurnResult::Delivered { presentation }, &g)
        .unwrap();

    let Report::TurnEnd { stop, deadline, .. } = stop_report() else {
        unreachable!()
    };
    let rearm = Report::Compacted {
        stop: clank_core::turn_policy::TurnEndReport { ..stop },
        deadline,
    };
    let frame = turns.answer(&author, 3, &rearm, true, &g).unwrap();
    assert_eq!(frame.verdict, WireVerdict::Park, "{frame:?}");
    assert_eq!(turns.next_deadline(), Some(due));
}
