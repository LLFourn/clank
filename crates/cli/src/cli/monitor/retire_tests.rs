use crate::cli::monitor::fixtures::*;

/// A retirement gets the last frame OUT, transient included.
///
/// The failure it excludes: `serve_to` only `try_send`s into a
/// client's queue, the writer task does the writing, and a
/// detached handle dies with the runtime — so returning from
/// `start` right after serving could lose the frame. A durable row
/// would replay from the WAL; a TRANSIENT has no row, and
/// `serve_to` clears it from `pending` on send. An orderly
/// retirement must not be the thing that manufactures a lost wake
/// (codex on 294c18d).
///
/// Driven over a real socket pair with the production `write_to`,
/// and the proof is `drained`: only the writer REPORTING that its
/// queue emptied says the bytes left.
#[tokio::test]
async fn a_retirement_writes_the_last_frame_before_it_goes() {
    use tokio::io::AsyncReadExt;

    let repo = repo_with_roster();
    let claude = AgentLabel::parse("claude").unwrap();
    let held = WaitItem::CommandEvent {
        presentation: None,
        source_cwd: None,
        name: "deploys".into(),
        id: "a".into(),
        event: "deploys".into(),
        content: "ci failed".into(),
        at: None,
        failure: false,
        requires_reply: false,
        reply_route: None,
        selector: None,
    };
    let mut sources = holding(vec![(claude.clone().into(), held)]);

    let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let writer = tokio::spawn(write_to(ours, queued, "claude".to_string()));
    let mut clients = vec![Client {
        who: Served::Agent {
            author: claude.clone(),
        },
        out,
        writer: Some(writer),
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];

    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let leaving = serve_to(repo.path(), &mut clients, &state, None, &mut sources);
    assert!(
        sources.pending().is_empty(),
        "the transient left `pending` on send, so only this retirement can still \
             deliver it"
    );
    assert!(
        clients.is_empty() && leaving.len() == 1,
        "the frame that WAKES a client also retires it, so its writer leaves \
             `clients` on that very frame — which is where the last frame was being \
             lost, one step earlier than it looked"
    );

    let decision = retire(
        clients,
        leaving,
        Vec::new(),
        Duration::from_secs(3),
        Duration::from_millis(500),
        0,
    )
    .await;
    assert_eq!(
        decision,
        Shutdown::ReadyToRelease(Retired {
            drained: 1,
            settled: true,
            ..Default::default()
        })
    );

    // Read AFTER the retirement returned: the bytes are already
    // there, which is what `drained` claims.
    let mut buf = Vec::new();
    theirs.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(
        text.contains("ci failed"),
        "the transient must be in what was written: {text:?}"
    );
}

/// A writer that never finishes cannot hold the repo, and one that
/// ENDED BADLY is not a delivery.
///
/// The three non-success outcomes, each driven, because the first
/// version counted every awaited handle as a success — so a
/// panicked task and a peer that errored both reported as frames
/// delivered (codex on d2b012d). The writers here are synthetic:
/// a socket wedged or torn down at exactly the right moment is not
/// portably arrangeable, and what is under test is the
/// CLASSIFICATION, not the kernel.
#[tokio::test]
async fn a_task_that_ended_badly_is_never_counted_as_delivered() {
    let client = |writer: tokio::task::JoinHandle<Wrote>| {
        let (out, _queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        Client {
            who: Served::Observer,
            out,
            writer: Some(writer),
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        }
    };

    let began = std::time::Instant::now();
    let decision = retire(
        vec![
            client(tokio::spawn(async { Wrote::Failed })),
            client(tokio::spawn(async { Wrote::TimedOut })),
            client(tokio::spawn(async { panic!("a writer came apart") })),
            client(tokio::spawn(std::future::pending::<Wrote>())),
        ],
        Vec::new(),
        Vec::new(),
        Duration::from_millis(150),
        Duration::from_millis(500),
        0,
    )
    .await;

    assert_eq!(
        decision,
        Shutdown::ReadyToRelease(Retired {
            drained: 0,
            failed: 2,
            lost: 1,
            abandoned: 1,
            undelivered: 0,
            settled: true,
        }),
        "not one of these wrote a frame"
    );
    assert!(
        began.elapsed() < Duration::from_secs(1),
        "and the one that was never going to finish did not hold the repo"
    );
}

/// A peer that finished long ago is not abandoned because another
/// one is stalled.
///
/// Outcomes are collected as they ARRIVE, not in handle order.
/// Awaiting the vector in sequence would block on the stalled
/// writer for the whole budget and then class the finished one as
/// abandoned at the deadline — reporting a delivered frame as a
/// lost one (codex on 258d979).
#[tokio::test]
async fn a_finished_writer_behind_a_stalled_one_is_still_counted() {
    let client = |writer: tokio::task::JoinHandle<Wrote>| {
        let (out, _queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        Client {
            who: Served::Observer,
            out,
            writer: Some(writer),
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        }
    };

    let decision = retire(
        vec![
            // The stalled one FIRST, so sequence order is the
            // wrong order.
            client(tokio::spawn(std::future::pending::<Wrote>())),
            client(tokio::spawn(async { Wrote::Drained })),
        ],
        Vec::new(),
        Vec::new(),
        Duration::from_millis(150),
        Duration::from_millis(500),
        0,
    )
    .await;

    assert_eq!(
        decision,
        Shutdown::ReadyToRelease(Retired {
            drained: 1,
            abandoned: 1,
            settled: true,
            ..Default::default()
        }),
        "the second writer's frame landed and must be reported as landed"
    );
}

/// A retirement with NO clients still waits for its producers.
///
/// `abort` requests cancellation; it does not establish that the
/// future and the resources it owns have been dropped. With no
/// writers to await, a retirement could return immediately and
/// `start` would release the lease while a producer was still
/// winding down — a successor taking the repo on top of it
/// (codex on d2b012d). The guard's `Drop` is the evidence.
#[tokio::test]
async fn a_retirement_with_no_clients_still_sees_its_producers_stop() {
    struct Guard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let held = dropped.clone();
    let producer = tokio::spawn(async move {
        let _guard = Guard(held);
        std::future::pending::<()>().await
    });
    // Let it reach its await point, so the guard is live inside the
    // task rather than still on this one's stack.
    tokio::task::yield_now().await;

    // As the transition does: stop them, then hand the handles on.
    let producers = Hosted {
        timers: None,
        spec: Vec::new(),
        tasks: [(
            "producer".to_string(),
            HostedTask {
                instance: 0,
                handle: producer,
                stop: None,
            },
        )]
        .into(),
    }
    .stop();

    let decision = retire(
        Vec::new(),
        Vec::new(),
        producers,
        Duration::from_millis(150),
        Duration::from_millis(500),
        0,
    )
    .await;
    assert_eq!(
        decision,
        Shutdown::ReadyToRelease(Retired {
            settled: true,
            ..Default::default()
        })
    );
    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "the producer's resources must be gone BEFORE the lease is released, and \
             nothing but awaiting the cancelled handle establishes that"
    );
}

/// A retirement takes EVERY kind of task the owner holds.
///
/// The rule lives here rather than at the call site because the
/// call site is inside `start`'s loop, which no test reaches: a
/// mutation that left the handshakes behind survived until this
/// moved. Both sets are drained, and what comes back is what the
/// retirement will wait for.
#[tokio::test]
async fn the_owner_hands_over_every_kind_of_task() {
    let mut owned = OwnedTasks::default();
    owned.departing.push(tokio::spawn(async {}));
    owned.departing.push(tokio::spawn(async {}));
    owned.greeting.push(tokio::spawn(async {}));
    let repo = tempfile::tempdir().unwrap();
    owned
        .interrupting
        .notify(repo.path(), AgentLabel::parse("grok").unwrap());

    let all = owned.take_all();
    assert_eq!(
        all.len(),
        4,
        "producers, handshakes and interruption tasks all count"
    );
    assert!(
        owned.departing.is_empty() && owned.greeting.is_empty() && owned.interrupting.is_empty(),
        "and the owner keeps nothing back, or the retirement waits for less than it owns"
    );

    let decision = retire(
        Vec::new(),
        Vec::new(),
        all,
        Duration::from_millis(150),
        Duration::from_millis(500),
        0,
    )
    .await;
    assert!(
        matches!(decision, Shutdown::ReadyToRelease(_)),
        "{decision:?}"
    );
}

/// An admission that lands DURING the cutoff is still owned.
///
/// A handshake that completed moments before the stand-down puts
/// its client in the channel, not in `clients`. Left there it is
/// dropped with a live writer still owning a socket — the detached
/// task the whole inventory exists to prevent (codex on 056c991).
///
/// Closing the channel first is what makes the drain finite, and
/// the retirement afterwards is what proves the claim was worth
/// making: the late client's writer reports `drained`, which only
/// a writer this retirement OWNED could do.
#[tokio::test]
async fn an_admission_arriving_during_the_cutoff_is_claimed_and_flushed() {
    use tokio::io::AsyncReadExt;

    let mut clients = Clients::new();
    let joined = clients.joining();
    let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let writer = tokio::spawn(write_to(ours, queued, "late".to_string()));
    out.try_send("{\"t\":\"beat\"}\n".to_string()).unwrap();

    // As a finishing handshake does, after the stand-down was
    // decided but before anything was taken.
    joined
        .send(Greeted::Client(Client {
            who: Served::Observer,
            out,
            writer: Some(writer),
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        }))
        .unwrap();
    // And a second successor, which is answered on its own
    // connection and owes this one nothing.
    joined.send(Greeted::StandDown).unwrap();

    // Through the WHOLE transition, which is what `start` calls:
    // the cutoff and the retirement together. Driving the claim
    // alone left its only caller unproven.
    let decision = stand_down(
        std::path::Path::new("/w/proj"),
        &mut clients,
        &mut Sources::hosting(Default::default()),
        OwnedTasks::default(),
        None,
        None,
        Duration::from_secs(3),
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(
        decision,
        Shutdown::ReadyToRelease(Retired {
            drained: 1,
            settled: true,
            ..Default::default()
        }),
        "a client claimed at the cutoff gets the same orderly flush as any other"
    );

    let mut buf = Vec::new();
    theirs.read_to_end(&mut buf).await.unwrap();
    assert!(
        String::from_utf8(buf).unwrap().contains("beat"),
        "and its queued frame actually went out"
    );
}

/// An emission the sources already handed over is still presented.
///
/// Acknowledgment of acceptance precedes cancellation of the producer.
/// A durable row replays from the WAL for
/// whoever connects next; a transient has no row, so this is the
/// one the cutoff has to carry (codex on 5f7bc36).
///
/// The ordering is the content: production stops, the buffer is
/// drained into `pending`, and only then is the presentation made
/// — so the item is in hand before the set it is presented to is
/// decided.
#[tokio::test]
async fn an_emission_accepted_before_the_cutoff_still_reaches_its_client() {
    use tokio::io::AsyncReadExt;

    let repo = repo_with_roster();
    let claude = AgentLabel::parse("claude").unwrap();
    let mut sources = Sources::hosting(Default::default());
    let tx = sources.emitter();
    let sink = crate::cli::event_source::EmissionSink::for_owner(tx, claude.clone().into());
    let (accepted, acceptance) = tokio::sync::oneshot::channel();
    let producer = tokio::spawn(async move {
        sink.send((
            WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: "deploys".into(),
                id: "a".into(),
                event: "deploys".into(),
                content: "ci failed".into(),
                at: None,
                failure: false,
                requires_reply: false,
                reply_route: None,
                selector: None,
            },
            crate::cli::event_source::Durability::Transient,
        ))
        .unwrap();
        accepted.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    acceptance.await.unwrap();
    let hosted = std::collections::BTreeMap::from([(
        claude.clone().into(),
        Hosted {
            timers: None,
            spec: Vec::new(),
            tasks: [(
                "producer".to_string(),
                HostedTask {
                    instance: 0,
                    handle: producer,
                    stop: None,
                },
            )]
            .into(),
        },
    )]);

    let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let writer = tokio::spawn(write_to(ours, queued, "claude".to_string()));
    let mut clients = Clients::of(vec![Client {
        who: Served::Agent { author: claude },
        out,
        writer: Some(writer),
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }]);

    sources.host(hosted);
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let decision = stand_down(
        repo.path(),
        &mut clients,
        &mut sources,
        OwnedTasks::default(),
        Some(&state),
        None,
        Duration::from_secs(3),
        Duration::from_millis(500),
    )
    .await;

    let got = decision.report();
    assert_eq!(got.drained, 1, "the client's writer flushed: {got:?}");
    assert_eq!(
        got.undelivered, 0,
        "and the accepted emission is not residual work: it was presented"
    );

    let mut buf = Vec::new();
    theirs.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(
        text.contains("ci failed"),
        "an emission taken off the channel must actually reach the peer: {text:?}"
    );
}

/// A greeting still running at the cutoff cannot welcome a peer
/// and leave its writer behind.
///
/// The race this excludes: the queue closes while the tasks that
/// PRODUCE admissions are still live, so one finishes afterwards,
/// sends `Welcome::Ok`, spawns a socket writer, fails to hand the
/// client over, and the wrapper drops it — a child task nothing
/// owns, after admission was supposed to be shut
/// (codex on b37b6b6).
///
/// Driven with a greeting held at a barrier, which is what an
/// in-progress handshake IS to this transition: a task that has
/// not finished and cannot be allowed to.
#[tokio::test]
async fn a_greeting_running_at_the_cutoff_leaves_no_writer_behind() {
    struct Guard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let torn_down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let held = torn_down.clone();

    let owned = OwnedTasks::default();
    owned.greeting.push(tokio::spawn(async move {
        // Stands for everything a handshake owns while it runs.
        let _socket = Guard(held);
        std::future::pending::<()>().await
    }));
    tokio::task::yield_now().await;

    let decision = stand_down(
        std::path::Path::new("/w/proj"),
        &mut Clients::new(),
        &mut Sources::hosting(Default::default()),
        owned,
        None,
        None,
        Duration::from_millis(200),
        Duration::from_millis(200),
    )
    .await;

    assert!(
        matches!(decision, Shutdown::ReadyToRelease(_)),
        "the greeting was cancellable, so the release is orderly: {decision:?}"
    );
    assert!(
        torn_down.load(std::sync::atomic::Ordering::SeqCst),
        "and what it held is gone BEFORE the lease could be released — a greeting \
             still live past the cutoff is an open admission path"
    );
}

/// A client admitted DURING the cutoff still receives the
/// transient it is the only recipient for.
///
/// The ordering error this excludes: the final presentation used
/// to run in the caller, BEFORE late arrivals were claimed. A
/// transient whose only eligible client was still in the queue was
/// retained by that presentation, then dropped unpresented when
/// the claim took the client and the retirement dropped its
/// sender. Both the item and its recipient were in hand, and the
/// item was reported undelivered (codex on b37b6b6).
///
/// The client's queue starts EMPTY, as a real greeting leaves it:
/// nothing is pre-queued, so the bytes at the peer can only come
/// from the presentation this transition makes.
#[tokio::test]
async fn a_client_admitted_during_the_cutoff_still_gets_its_transient() {
    use tokio::io::AsyncReadExt;

    let repo = repo_with_roster();
    let claude = AgentLabel::parse("claude").unwrap();
    let mut clients = Clients::new();
    let joined = clients.joining();

    let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let writer = tokio::spawn(write_to(ours, queued, "claude".to_string()));
    joined
        .send(Greeted::Client(Client {
            who: Served::Agent {
                author: claude.clone(),
            },
            out,
            writer: Some(writer),
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        }))
        .unwrap();

    let mut sources = holding(vec![(
        claude.into(),
        WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "a".into(),
            event: "deploys".into(),
            content: "ci failed".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        },
    )]);
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());

    let decision = stand_down(
        repo.path(),
        // No client attached: the only eligible one is the
        // admission still sitting in the queue.
        &mut clients,
        &mut sources,
        OwnedTasks::default(),
        Some(&state),
        None,
        Duration::from_secs(3),
        Duration::from_millis(500),
    )
    .await;

    let got = decision.report();
    assert_eq!(got.drained, 1, "the late client's writer flushed: {got:?}");
    assert_eq!(
        got.undelivered, 0,
        "and nothing is owed: the transition had both the item and its recipient"
    );

    let mut buf = Vec::new();
    theirs.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8(buf).unwrap();
    assert!(
        text.contains("ci failed"),
        "the transient must actually reach the peer, not merely be counted: {text:?}"
    );
}

/// A task cancellation cannot reach forbids an orderly release.
///
/// `settled: false` used to be a boolean: the unsettled path
/// printed "releasing anyway" and then released anyway, which is a
/// log line where a contract was wanted (codex on dd67677). The
/// decision is exhaustive now, and this is the branch `start`
/// answers by exiting WITH the lease held rather than unwinding
/// it.
///
/// The producer is a BLOCKING task parked on a channel, because
/// `abort` only takes effect at an await point and a blocking task
/// has none — which is precisely the case the contract exists for.
/// It is released at the end so no thread is left behind.
#[tokio::test]
async fn a_task_cancellation_cannot_reach_forbids_an_orderly_release() {
    let (let_go, parked) = std::sync::mpsc::channel::<()>();
    let (running, started) = std::sync::mpsc::channel::<()>();
    let producer = tokio::task::spawn_blocking(move || {
        let _ = running.send(());
        let _ = parked.recv();
    });
    // It must be RUNNING before the retirement: a blocking task
    // the pool has not picked up yet is cancellable like any
    // other, which is the opposite of the case under test.
    tokio::task::spawn_blocking(move || started.recv())
        .await
        .unwrap()
        .unwrap();

    // As the transition does: stop them, then hand the handles on.
    let producers = Hosted {
        timers: None,
        spec: Vec::new(),
        tasks: [(
            "producer".to_string(),
            HostedTask {
                instance: 0,
                handle: producer,
                stop: None,
            },
        )]
        .into(),
    }
    .stop();

    let decision = retire(
        Vec::new(),
        Vec::new(),
        producers,
        Duration::from_millis(50),
        Duration::from_millis(50),
        0,
    )
    .await;

    assert!(
        matches!(decision, Shutdown::RequiresProcessExit(_)),
        "a producer that has not stopped must not permit the orderly release that \
             drops the lease: {decision:?}"
    );
    let _ = let_go.send(());
}

/// winding down (codex on dd67677). The handles it hands back are
/// the inventory that closes it.
#[tokio::test]
async fn a_producer_removed_just_before_a_retirement_is_still_waited_for() {
    struct Guard(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let held = dropped.clone();
    let producer = tokio::spawn(async move {
        let _guard = Guard(held);
        std::future::pending::<()>().await
    });
    tokio::task::yield_now().await;

    // Exactly what `reconcile_hosting` returns when an author's
    // sources change or its roster entry goes.
    let departing = Hosted {
        timers: None,
        spec: Vec::new(),
        tasks: [(
            "producer".to_string(),
            HostedTask {
                instance: 0,
                handle: producer,
                stop: None,
            },
        )]
        .into(),
    }
    .stop();

    let decision = retire(
        Vec::new(),
        Vec::new(),
        departing,
        Duration::from_millis(150),
        Duration::from_millis(500),
        0,
    )
    .await;

    assert!(
        matches!(decision, Shutdown::ReadyToRelease(_)),
        "it did stop, so the release is orderly: {decision:?}"
    );
    assert!(
        dropped.load(std::sync::atomic::Ordering::SeqCst),
        "and a producer the CURRENT map no longer mentions must still have been \
             seen out before the lease could go"
    );
}
