use super::*;
use crate::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::{AutoMode, SessionId, Tool};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn fixture() -> (tempfile::TempDir, AgentLabel, SessionId) {
    let temp = tempfile::tempdir().unwrap();
    let author = AgentLabel::parse("grok").unwrap();
    crate::cli::agent::add_repo_roster_agent(
        temp.path(),
        &author,
        AgentDescription {
            tool: Tool::Grok,
            launch: None,
            initial_prompt: None,
        },
        RosterRole::Master,
    )
    .unwrap();
    crate::agent_store::set_auto_mode(temp.path(), &author, AutoMode::On).unwrap();
    let session = SessionId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    crate::agent_store::bind_session_to_agent(temp.path(), &author, Tool::Grok, &session).unwrap();
    (temp, author, session)
}

async fn connecting(
    repo: &std::path::Path,
    author: &AgentLabel,
    session: &SessionId,
) -> (Client, tokio::io::BufReader<tokio::net::UnixStream>) {
    let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
    let hello = Hello {
        proto: PROTO,
        build: None,
        repo: repo.to_string_lossy().into(),
        who: Who::Agent {
            author: author.to_string(),
            holder: Some(Holder {
                tool: Tool::Grok,
                session: session.to_string(),
            }),
            has_cli_sources: false,
            turn_end: None,
        },
    };
    peer.write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
        .await
        .unwrap();
    let client = admit(server, repo).await.unwrap().client();
    let mut peer = tokio::io::BufReader::new(peer);
    let mut welcome = String::new();
    peer.read_line(&mut welcome).await.unwrap();
    assert!(matches!(
        serde_json::from_str::<Welcome>(&welcome).unwrap(),
        Welcome::Ok { .. }
    ));
    (client, peer)
}

fn joining(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    client: Client,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    admit_into(
        repo,
        clients,
        client,
        None,
        &mut vec![],
        &Default::default(),
    )
}

#[tokio::test]
async fn duplicate_is_refused_and_eof_allows_replacement_without_clearing_it() {
    let (repo, author, session) = fixture();
    let (first, first_peer) = connecting(repo.path(), &author, &session).await;
    let first_closed = first.out.clone();
    let mut clients = vec![];
    assert!(joining(repo.path(), &mut clients, first).is_empty());
    let (duplicate, mut duplicate_peer) = connecting(repo.path(), &author, &session).await;
    let departed = joining(repo.path(), &mut clients, duplicate);
    let mut line = String::new();
    duplicate_peer.read_line(&mut line).await.unwrap();
    assert!(
        matches!(serde_json::from_str::<Note>(&line).unwrap(), Note::Refused { why } if why.contains("already has"))
    );
    assert_eq!(clients.len(), 1);
    for task in departed {
        task.await.unwrap();
    }
    drop(first_peer);
    tokio::time::timeout(Duration::from_secs(1), first_closed.closed())
        .await
        .unwrap();
    let (replacement, replacement_peer) = connecting(repo.path(), &author, &session).await;
    for task in joining(repo.path(), &mut clients, replacement) {
        task.await.unwrap();
    }
    assert_eq!(clients.len(), 1);
    assert!(
        !clients[0].out.is_closed(),
        "old socket cleanup must not clear its replacement"
    );
    drop(replacement_peer);
    drop(clients);
}

#[tokio::test]
async fn native_session_rebinding_rejects_old_wait_and_allows_new_wait() {
    let (repo, author, session) = fixture();
    let (old, old_peer) = connecting(repo.path(), &author, &session).await;
    let mut clients = vec![];
    joining(repo.path(), &mut clients, old);
    let new_session = SessionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
    crate::agent_store::bind_session_to_agent(repo.path(), &author, Tool::Grok, &new_session)
        .unwrap();
    assert!(client_refusal(repo.path(), &clients[0]).is_some());
    let (new, new_peer) = connecting(repo.path(), &author, &new_session).await;
    assert!(joining(repo.path(), &mut clients, new).is_empty());
    assert_eq!(clients.len(), 2);
    let (stale, mut stale_peer) = connecting(repo.path(), &author, &session).await;
    let departed = joining(repo.path(), &mut clients, stale);
    let mut line = String::new();
    stale_peer.read_line(&mut line).await.unwrap();
    assert!(
        matches!(serde_json::from_str::<Note>(&line).unwrap(), Note::Refused { why } if why.contains("no longer owns"))
    );
    for task in departed {
        task.await.unwrap();
    }
    drop((old_peer, new_peer, clients));
}

/// Attendance never quiets Grok: its handoff still asks for the one
/// background wait, and an event logged while that wait is held is
/// delivered on it.
#[tokio::test]
async fn an_attending_groks_background_wait_delivers_a_new_event() {
    use crate::cli::github::log::{EventLog, Transport as Wal};
    use crate::cli::turn_custody::{Gathered, Turns};
    use crate::cli::turn_wire::{Report, WireVerdict};
    use clank_core::turn_policy::AttendRequest;

    let repo = TempRepo::new();
    let author = AgentLabel::parse("grok").unwrap();
    crate::cli::agent::add_repo_roster_agent(
        repo.path(),
        &author,
        AgentDescription {
            tool: Tool::Grok,
            launch: None,
            initial_prompt: None,
        },
        RosterRole::Master,
    )
    .unwrap();
    let timer = clank_core::agent_config::TimerSource {
        interrupt: false,
        interrupt_message: None,
        name: "check".into(),
        repeat_every: None,
        due_at: "1970-01-01T00:01:40Z".into(),
        content: Some("grok scheduled work".into()),
    };
    crate::agent_store::update_agent_config(repo.path(), &author, |config| {
        config.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
            timer.clone(),
        )];
    })
    .unwrap();
    crate::agent_store::set_auto_mode(repo.path(), &author, AutoMode::On).unwrap();
    let session = SessionId::parse("11111111-1111-4111-8111-111111111111").unwrap();
    crate::agent_store::bind_session_to_agent(repo.path(), &author, Tool::Grok, &session).unwrap();

    let g = Gathered::of(repo.path(), &author, Some((&[], None)));
    let mut turns = Turns::new(repo.path().to_path_buf());
    turns
        .attend(
            &author,
            AttendRequest::Declare {
                session: session.clone(),
                name: "sleeper".into(),
                desc: None,
                expect: 600_000,
            },
            &g,
        )
        .unwrap();
    let handoff = Report::Handoff {
        tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
        hook_started: None,
        session: session.clone(),
        continued: false,
        deadline: u64::MAX,
    };
    let frame = turns.answer(&author, 1, &handoff, true, &g).unwrap();
    assert_eq!(
        frame.verdict,
        WireVerdict::Notify,
        "the handoff asks for its wait"
    );

    let (wait, mut peer) = connecting(repo.path(), &author, &session).await;
    let mut clients = vec![];
    let _ = joining(repo.path(), &mut clients, wait);
    let events = crate::agent_store::agents_root(repo.path()).join("grok/events");
    std::fs::create_dir_all(&events).unwrap();
    EventLog::open(&events, &timer.source_key(None))
        .unwrap()
        .append_inbox(
            Wal::Timer,
            Some("timer/100"),
            None,
            Some(100),
            &WaitItem::TimerEvent {
                presentation: None,
                name: "check".into(),
                recurrence: None,
                id: "timer/100".into(),
                due_at: 100,
                content: "grok scheduled work".into(),
                selector: None,
            },
        )
        .unwrap();
    let state = folded(repo.path()).await.unwrap();
    serve(repo.path(), &mut clients, &state);
    let items = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            assert!(
                peer.read_line(&mut line).await.unwrap() > 0,
                "the wait closed"
            );
            if let Note::Items { items, .. } = serde_json::from_str::<Note>(&line).unwrap() {
                return items;
            }
        }
    })
    .await
    .expect("the wait is woken");
    assert!(
        items.iter().any(|item| matches!(
            item,
            WaitItem::TimerEvent { content, .. } if content == "grok scheduled work"
        )),
        "{items:?}"
    );
}

/// Automatic Grok's wait registers no turn-end, and is its agent's park
/// all the same: when its socket closes under it, whether or not a round
/// registered it first, the round records the loss against the record
/// recovery reads.
#[tokio::test]
async fn a_lost_automatic_wait_leaves_its_agent_unwatched() {
    for registered_before_close in [true, false] {
        lost_automatic_wait(registered_before_close).await;
    }
}

async fn lost_automatic_wait(registered_before_close: bool) {
    let (repo, author, session) = fixture();
    let history = repo.path().join("history.jsonl");
    std::fs::write(
        history.with_file_name("events.jsonl"),
        "{\"type\":\"turn_started\",\"turn_number\":1}\n",
    )
    .unwrap();
    let bound = crate::agent_store::binding(repo.path(), &author)
        .unwrap()
        .unwrap();
    crate::agent_store::record_transcript(
        repo.path(),
        &author,
        &bound.id,
        &history.display().to_string(),
    )
    .unwrap();
    let binding = crate::agent_store::binding(repo.path(), &author)
        .unwrap()
        .unwrap();
    let mut recovery =
        crate::cli::recovery::Driver::new(repo.path().to_path_buf(), "/nonexistent".into());
    recovery.reconcile(
        vec![crate::cli::recovery::Facts {
            agent_dir: crate::agent_store::agents_root(repo.path()).join(author.as_str()),
            author: author.clone(),
            session: binding,
            auto: true,
            work: vec![],
            ledger: crate::cli::ledger::Ledger::owing_if(false),
            role: "master".into(),
            blind_nudge: None,
            stall: None,
            attended: false,
            blocked_on: None,
            asking: false,
            projection: Default::default(),
        }],
        0,
    );
    let mut turns = crate::cli::turn_custody::Turns::new(repo.path().to_path_buf());
    let mut round = |clients: &mut Vec<Client>, turns: &mut crate::cli::turn_custody::Turns| {
        let _ = tend(
            repo.path(),
            clients,
            None,
            &[],
            &Default::default(),
            turns,
            &mut recovery,
        );
    };
    let (wait, peer) = connecting(repo.path(), &author, &session).await;
    let closed = wait.out.clone();
    let mut clients = vec![];
    joining(repo.path(), &mut clients, wait);
    if registered_before_close {
        round(&mut clients, &mut turns);
    }
    drop(peer);
    tokio::time::timeout(Duration::from_secs(1), closed.closed())
        .await
        .unwrap();
    drop(closed);
    round(&mut clients, &mut turns);
    assert!(clients.is_empty());
    assert!(
        turns.state(&author).unwrap().unwatched.is_some(),
        "registered before close: {registered_before_close}"
    );
}

/// A Grok Stop's report, carried on a connection that holds nothing, as
/// the hook's own child carries it.
async fn reporting(
    repo: &std::path::Path,
    author: &AgentLabel,
    session: &SessionId,
) -> (Client, tokio::io::BufReader<tokio::net::UnixStream>) {
    let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
    let hello = Hello {
        proto: PROTO,
        build: None,
        repo: repo.to_string_lossy().into(),
        who: Who::Agent {
            author: author.to_string(),
            holder: None,
            has_cli_sources: false,
            turn_end: Some(crate::cli::turn_wire::Report::Handoff {
                tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
                hook_started: None,
                session: session.clone(),
                continued: false,
                deadline: u64::MAX,
            }),
        },
    };
    peer.write_all(format!("{}\n", serde_json::to_string(&hello).unwrap()).as_bytes())
        .await
        .unwrap();
    let client = admit(server, repo).await.unwrap().client();
    let mut peer = tokio::io::BufReader::new(peer);
    let mut welcome = String::new();
    peer.read_line(&mut welcome).await.unwrap();
    (client, peer)
}

/// What the monitor answers one Grok Stop while `clients` are connected.
async fn handoff_answer(
    repo: &std::path::Path,
    author: &AgentLabel,
    session: &SessionId,
    clients: &mut Vec<Client>,
) -> crate::cli::turn_wire::WireVerdict {
    let (report, mut peer) = reporting(repo, author, session).await;
    clients.push(report);
    let mut turns = crate::cli::turn_custody::Turns::new(repo.to_path_buf());
    for writer in serve_reports(
        repo,
        clients,
        None,
        None,
        &[],
        &mut Default::default(),
        &Default::default(),
        &mut turns,
    ) {
        writer.await.unwrap();
    }
    loop {
        let mut line = String::new();
        assert!(peer.read_line(&mut line).await.unwrap() > 0, "no verdict");
        if let Note::Verdict(frame) = serde_json::from_str::<Note>(&line).unwrap() {
            return frame.verdict;
        }
    }
}

/// The monitor's registration of the session's bare background wait is
/// the only evidence it runs: with none the Stop is told to arm one, with
/// one it is silent, and once that wait exits its registration drops and
/// the next Stop is told to arm one again. A second wait is refused by the
/// one-wait-per-session holder, so the registration stays single.
#[tokio::test]
async fn the_registered_waiter_alone_decides_whether_grok_arms_one() {
    use crate::cli::turn_wire::WireVerdict;
    let (repo, author, session) = fixture();
    let mut clients = vec![];
    assert_eq!(
        handoff_answer(repo.path(), &author, &session, &mut clients).await,
        WireVerdict::Notify,
        "no wait registered"
    );

    let (wait, wait_peer) = connecting(repo.path(), &author, &session).await;
    let closed = wait.out.clone();
    assert!(joining(repo.path(), &mut clients, wait).is_empty());
    let (duplicate, mut duplicate_peer) = connecting(repo.path(), &author, &session).await;
    let _ = joining(repo.path(), &mut clients, duplicate);
    let mut refusal = String::new();
    duplicate_peer.read_line(&mut refusal).await.unwrap();
    assert!(
        matches!(
            serde_json::from_str::<Note>(&refusal).unwrap(),
            Note::Refused { .. }
        ),
        "{refusal}"
    );
    assert_eq!(
        handoff_answer(repo.path(), &author, &session, &mut clients).await,
        WireVerdict::Silent {
            why: clank_core::SilentReason::WaitArmed
        },
        "the registered wait carries the next wake"
    );

    let other = SessionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
    crate::agent_store::bind_session_to_agent(repo.path(), &author, Tool::Grok, &other).unwrap();
    assert_eq!(
        handoff_answer(repo.path(), &author, &other, &mut clients).await,
        WireVerdict::Notify,
        "another session's wait is not this one's"
    );
    crate::agent_store::bind_session_to_agent(repo.path(), &author, Tool::Grok, &session).unwrap();

    drop(wait_peer);
    tokio::time::timeout(Duration::from_secs(1), closed.closed())
        .await
        .unwrap();
    assert_eq!(
        handoff_answer(repo.path(), &author, &session, &mut clients).await,
        WireVerdict::Notify,
        "the wait exited, so its registration is gone"
    );
}

/// A due check-in reaches Grok at its native Stop, never through its idle
/// background wait: the wait is served nothing, the Stop's report is handed
/// the check-in as a presentation its own connection carries, and only
/// that connection's delivered result spends it.
#[tokio::test]
async fn groks_check_in_rides_its_stop_and_never_its_background_wait() {
    use crate::cli::turn_wire::{TurnResult, WireVerdict};
    let (repo, author, session) = fixture();
    let mut turns = crate::cli::turn_custody::Turns::new(repo.path().to_path_buf());
    let then = crate::cli::turn_custody::now_ms() - 120_000;
    turns
        .attend(
            &author,
            clank_core::turn_policy::AttendRequest::Declare {
                session: session.clone(),
                name: "sleeper".into(),
                desc: None,
                expect: 60_000,
            },
            &crate::cli::turn_custody::Gathered::of(repo.path(), &author, None).at(then),
        )
        .unwrap();
    let mut clients = vec![];
    let (wait, mut wait_peer) = connecting(repo.path(), &author, &session).await;
    assert!(joining(repo.path(), &mut clients, wait).is_empty());
    let state = folded(repo.path()).await.unwrap();
    serve(repo.path(), &mut clients, &state);
    let mut line = String::new();
    wait_peer.read_line(&mut line).await.unwrap();
    assert!(
        !matches!(
            serde_json::from_str::<Note>(&line).unwrap(),
            Note::Items { .. }
        ),
        "the idle wait carries nothing: {line}"
    );

    let (report, mut peer) = reporting(repo.path(), &author, &session).await;
    let conn = report.conn;
    clients.push(report);
    for writer in serve_reports(
        repo.path(),
        &mut clients,
        None,
        None,
        &[],
        &mut Default::default(),
        &Default::default(),
        &mut turns,
    ) {
        writer.await.unwrap();
    }
    let frame = loop {
        let mut line = String::new();
        assert!(peer.read_line(&mut line).await.unwrap() > 0, "no verdict");
        if let Note::Verdict(frame) = serde_json::from_str::<Note>(&line).unwrap() {
            break frame;
        }
    };
    let presentation = frame.presentation().expect("a presentation").clone();
    assert!(matches!(frame.verdict, WireVerdict::Deliver { .. }));
    let text = frame.rendered.unwrap_or_default();
    assert!(
        text.starts_with("Check-in: declared work \"sleeper\"")
            && text.contains("clank attend sleeper --expect 2m"),
        "{text}"
    );
    assert!(
        !text.contains(crate::cli::wake_render::ARM_WAIT),
        "its wait is registered: {text}"
    );
    assert_eq!(turns.carrier(conn), Some(&author));
    let g = crate::cli::turn_custody::Gathered::of(repo.path(), &author, None);
    let row = |turns: &mut crate::cli::turn_custody::Turns| {
        turns.projection(&author, &g).attended[0].check_in.clone()
    };
    assert_eq!(row(&mut turns).as_deref(), Some("check-in due"));
    assert!(
        turns
            .result(&author, conn, TurnResult::Delivered { presentation }, &g)
            .unwrap()
            .is_some()
    );
    assert_eq!(row(&mut turns).as_deref(), Some("checked in"));
}

/// Starts each of Grok's hook children on a thread of this process, as
/// `clank wait --turn-end` would run it, reaching the test's monitor over
/// these connections, one per attempt, and no monitor once they are spent.
struct InProcess(std::sync::Mutex<Vec<std::os::unix::net::UnixStream>>);

impl InProcess {
    /// A child with one connection to a monitor, and the monitor's end.
    fn reaching() -> (Self, tokio::net::UnixStream) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        theirs.set_nonblocking(true).unwrap();
        (
            InProcess(std::sync::Mutex::new(vec![ours])),
            tokio::net::UnixStream::from_std(theirs).unwrap(),
        )
    }
}

impl crate::cli::stop_hook::Spawn for InProcess {
    fn spawn(
        &self,
        repo: &std::path::Path,
        label: &AgentLabel,
        _: &str,
        _: Tool,
        report: &crate::cli::turn_wire::Report,
    ) -> Result<crate::cli::stop_hook::OwnedWait, String> {
        use tokio::net::unix::pipe;
        let (owner_read, owner_write) = std::io::pipe().unwrap();
        let (frames_read, frames_write) = std::io::pipe().unwrap();
        let ours = std::mem::take(&mut *self.0.lock().unwrap());
        super::turn_tests::carrier(repo, label, owner_read, frames_write, ours);
        crate::cli::stop_hook::OwnedWait::over(
            None,
            Box::new(pipe::Sender::from_owned_fd(owner_write.into()).unwrap()),
            Box::new(pipe::Receiver::from_owned_fd(frames_read.into()).unwrap()),
            None,
            report,
        )
    }
}

/// What Grok's Stop hook writes, on stdout and stderr, for its routed
/// handoff, given `budget` and its children started by `spawner`.
async fn grok_stop(
    repo: &std::path::Path,
    author: &AgentLabel,
    spawner: &InProcess,
    budget: Duration,
) -> (String, String) {
    use crate::cli::turn_child::tests::{handoff_by, within};
    let (mut out, mut err) = (Vec::new(), Vec::new());
    crate::cli::grok_hook::hand_off(
        spawner,
        repo,
        author,
        handoff_by(within(budget)),
        budget,
        &mut out,
        &mut err,
    )
    .await;
    (
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Grok's Stop blocked with the request for its wait.
fn arm_wait() -> String {
    format!(
        "{}\n",
        serde_json::json!({"decision": "block", "reason": crate::cli::wake_render::ARM_WAIT})
    )
}

/// Long enough past the child's last word, which comes two seconds before
/// the deadline, for the exchange to run in real time.
const BUDGET: Duration = Duration::from_secs(3);

/// A Grok Stop the monitor admits and does not answer in time still arms
/// the agent's wait: the hook asks for it, and the wait, attaching to that
/// monitor once it answers, is handed the work the Stop was owed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grok_stop_the_monitor_answers_late_still_arms_its_wait() {
    use crate::cli::turn_child::tests::{SESSION, repo_with};
    let (repo, author) = repo_with(Tool::Grok, true);
    let (spawner, theirs) = InProcess::reaching();
    let root = repo.path().to_path_buf();
    let slow = tokio::spawn(async move { admit(theirs, &root).await.unwrap().client() });
    let (out, err) = grok_stop(repo.path(), &author, &spawner, BUDGET).await;
    assert_eq!((out, err), (arm_wait(), String::new()));
    let report = slow.await.unwrap();
    assert!(
        report.report.is_some(),
        "the monitor took the Stop's report"
    );

    let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    theirs.set_nonblocking(true).unwrap();
    let theirs = tokio::net::UnixStream::from_std(theirs).unwrap();
    let root = repo.path().to_path_buf();
    let monitor = tokio::spawn(async move {
        let mut clients = vec![admit(theirs, &root).await.unwrap().client()];
        let state = folded(&root).await.unwrap();
        serve(&root, &mut clients, &state);
        clients
    });
    let ours = std::sync::Mutex::new(Some(ours));
    let binding = crate::cli::wait::WaitBinding::automatic_grok(SessionId::parse(SESSION).unwrap());
    let (items, _) = tokio::time::timeout(
        Duration::from_secs(30),
        crate::cli::wait::monitored(
            repo.path(),
            &author,
            &binding,
            &mut crate::upgrade::Replacing::never(),
            |who| {
                let stream = ours.lock().unwrap().take();
                let repo = repo.path().to_path_buf();
                async move {
                    let Some(stream) = stream else {
                        return Attempt::Unproven("its monitor is spent".into());
                    };
                    tokio::task::spawn_blocking(move || {
                        settled(greet_at(
                            "test-socket",
                            &repo,
                            who,
                            std::time::Instant::now() + ATTACH_BUDGET,
                            move |_, _| stream.try_clone(),
                        ))
                        .unwrap_or_else(Attempt::Unproven)
                    })
                    .await
                    .unwrap()
                }
            },
            crate::cli::monitor::outage::scripted::Scripted::default(),
        ),
    )
    .await
    .expect("the wait is answered")
    .unwrap();
    drop(monitor.await.unwrap());
    assert!(
        items.iter().any(|item| matches!(
            item,
            WaitItem::TimerEvent { content, .. } if content == "scheduled work"
        )),
        "{items:?}"
    );
}

/// A Grok Stop no monitor ever took, its attempts spent when its deadline
/// comes, arms the agent's wait just the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grok_stop_no_monitor_took_still_arms_its_wait() {
    let (repo, author) = crate::cli::turn_child::tests::repo_with(Tool::Grok, true);
    let spawner = InProcess(std::sync::Mutex::new(Vec::new()));
    let written = grok_stop(repo.path(), &author, &spawner, BUDGET).await;
    assert_eq!(written, (arm_wait(), String::new()));
}

/// A Grok Stop left unanswered while its session lost the label says
/// nothing: the request for a wait is not the new session's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grok_stop_unanswered_for_a_replaced_session_says_nothing() {
    let (repo, author) = crate::cli::turn_child::tests::repo_with(Tool::Grok, true);
    let (spawner, theirs) = InProcess::reaching();
    let root = repo.path().to_path_buf();
    let rebinding = author.clone();
    let slow = tokio::spawn(async move {
        let report = admit(theirs, &root).await.unwrap().client();
        let successor = SessionId::parse("22222222-2222-4222-8222-222222222222").unwrap();
        crate::agent_store::bind_session_to_agent(&root, &rebinding, Tool::Grok, &successor)
            .unwrap();
        report
    });
    let written = grok_stop(repo.path(), &author, &spawner, BUDGET).await;
    assert_eq!(written, (String::new(), String::new()));
    drop(slow.await.unwrap());
}

/// A verdict the monitor gives in time is what Grok's Stop is told.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grok_stop_answered_in_time_is_told_what_the_monitor_said() {
    use crate::cli::turn_wire::{Frame, WireVerdict};
    let (repo, author) = crate::cli::turn_child::tests::repo_with(Tool::Grok, true);
    let (spawner, theirs) = InProcess::reaching();
    let root = repo.path().to_path_buf();
    let answering = tokio::spawn(async move {
        let report = admit(theirs, &root).await.unwrap().client();
        let said = Frame::text(WireVerdict::Notify, "the monitor's own word".into());
        assert!(send_note(&report, &Note::Verdict(said)));
        report
    });
    let written = grok_stop(repo.path(), &author, &spawner, BUDGET).await;
    assert_eq!(
        written,
        (
            format!(
                "{}\n",
                serde_json::json!({"decision": "block", "reason": "the monitor's own word"})
            ),
            String::new()
        )
    );
    drop(answering.await.unwrap());
}
