use super::*;
use crate::cli::monitor::fixtures::*;

/// `wait` stops a waiter whose author left the roster, and treats
/// everything else as transient. The monitor must agree.
#[test]
fn only_a_missing_roster_entry_counts_as_revocation() {
    let repo = repo_with_roster();
    let gone = crate::agent_store::resolve_role(repo.path(), &AgentLabel::parse("ghost").unwrap())
        .expect_err("not on the roster");
    assert!(revoked(&gone), "a removed author is revocation");

    let other = anyhow::anyhow!("the config could not be read");
    assert!(!revoked(&other), "an unreadable config is transient");
}

/// The owner must not wait on anybody's socket.
///
/// A timeout inside a sequential loop bounds one write, not the
/// loop: unread clients used to hold the owner for a budget each,
/// so it could not accept, drain arrivals or tick — long enough
/// for a healthy newcomer to exhaust its attach budget unaccepted
/// (codex on 38088ac). The owner enqueues now.
#[tokio::test]
async fn stalled_readers_do_not_hold_the_fold_owner() {
    let repo = repo_with_roster();
    let (mut clients, peers) = registry(&[("codex", true), ("codex", true), ("claude", false)]);

    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());

    // More ticks than the queue holds, so a reader that cannot keep
    // up is retired instead of buffered forever.
    let began = std::time::Instant::now();
    for _ in 0..(CLIENT_QUEUE + 2) {
        serve(repo.path(), &mut clients, &state);
        // Let the writer tasks run, as the owner's select would.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let took = began.elapsed();

    // The sleeps are the only time this may spend: the owner's own
    // work is enqueueing, which never touches a socket. Awaiting
    // the two wedged writes would have cost 2 x WRITE_BUDGET per
    // tick.
    assert!(
        took < Duration::from_millis(600),
        "the owner spent {took:?} — it is waiting on client sockets"
    );
    assert!(
        clients.iter().any(|c| c.who.name() == "claude"),
        "the healthy client is still registered: {:?}",
        clients.iter().map(|c| c.who.name()).collect::<Vec<_>>()
    );
    assert!(
        clients.len() < 3,
        "a reader that cannot keep up is retired: {:?}",
        clients.iter().map(|c| c.who.name()).collect::<Vec<_>>()
    );
    // And the healthy client really was delivered to.
    peers[2]
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut got = [0u8; 1];
    use std::io::Read;
    assert!(
        (&mut &peers[2]).read(&mut got).is_ok(),
        "the healthy client has a frame waiting"
    );
}

/// An author removed from the roster has its connection retired.
///
/// Keeping it would hold a socket open forever with no beats — and
/// because that branch never writes, the monitor could not even
/// notice the client had gone (codex on 13e9fa7).
#[tokio::test]
async fn a_client_whose_author_left_the_roster_is_dropped() {
    let repo = repo_with_roster();
    let (mut clients, _keep) = registry(&[("codex", false)]);
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());

    serve(repo.path(), &mut clients, &state);
    assert_eq!(clients.len(), 1, "a registered author is served");

    std::fs::write(
        repo.path().join(".clank/config.json"),
        r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
    )
    .unwrap();
    serve(repo.path(), &mut clients, &state);
    assert!(
        clients.is_empty(),
        "a revoked author must not keep a connection"
    );
}

/// A row is presented ONCE, however many times its arrival is
/// signalled — and not at all once somebody has acked it.
///
/// The failure this excludes is two delivery paths with different
/// rules: a backlog replayed from the WAL at admission and a live
/// item broadcast straight off the producer channel. A row logged
/// before a client arrived would then be sent twice, and a row
/// another client acked in between would be sent anyway. Both are
/// gone because the channel carries a SIGNAL and the log decides
/// (codex on ca2ea69).
#[tokio::test]
async fn a_logged_row_is_presented_once_and_never_after_an_ack() {
    use crate::cli::github::log::{EventLog, Transport};

    let repo = repo_with_roster();
    with_source(repo.path(), "codex", "deploys");
    let codex = AgentLabel::parse("codex").unwrap();
    let events = crate::agent_store::agents_root(repo.path())
        .join("codex")
        .join("events");
    let key = match &hostable(repo.path(), &codex).unwrap()[0].0 {
        clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
        other => panic!("the fixture declares a command source, got {other:?}"),
    };

    // A row logged BEFORE anyone connects, as a monitor's producer
    // does: it has been running since the monitor started.
    let mut log = EventLog::open(&events, &key).unwrap();
    let seq = log
        .append_inbox(
            Transport::Command,
            None,
            None,
            None,
            &WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: "deploys".into(),
                id: "a".into(),
                event: "deploys".into(),
                content: "shipped".into(),
                at: None,
                failure: false,
                requires_reply: false,
                reply_route: None,
                selector: None,
            },
        )
        .unwrap();

    let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: codex.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let present = |clients: &mut Vec<Client>| {
        let _ = serve_to(
            repo.path(),
            clients,
            &state,
            Some(&codex),
            &mut Sources::hosting(Default::default()),
        );
    };

    // The first presentation owes it, and WAKES the client — which
    // ends that connection, because a wait returns on its first
    // non-empty frame.
    present(&mut clients);
    let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
    let Note::Items { items, .. } = note else {
        panic!("the backlog is owed, got {note:?}")
    };
    assert_eq!(items.len(), 1, "{items:?}");
    assert!(
        clients.is_empty(),
        "a woken agent connection is spent: it will not read another frame"
    );

    // Its successor is owed the same row, because being shown to a
    // client that has since gone is not being handled.
    let (out, mut later) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let fresh = |out| Client {
        writer: None,
        who: Served::Agent {
            author: codex.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    };
    clients.push(fresh(out));
    present(&mut clients);
    let note: Note = serde_json::from_str(later.recv().await.unwrap().trim_end()).unwrap();
    assert!(
        matches!(note, Note::Items { ref items, .. } if items.len() == 1),
        "an unacked row is owed to the next client too: {note:?}"
    );

    // Until it is acked. Then nobody is owed it, including a client
    // that never saw it.
    let (out, mut after) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    clients.push(fresh(out));
    crate::cli::github::log::EventLog::open(&events, &key)
        .unwrap()
        .append_ack(seq)
        .unwrap();
    present(&mut clients);
    let note: Note = serde_json::from_str(after.recv().await.unwrap().trim_end()).unwrap();
    assert!(
        matches!(note, Note::Beat),
        "an acked row is owed to nobody: {note:?}"
    );
}

/// A source event reaching a BLOCKED agent arrives with its block
/// context, exactly as a local wait delivers it.
///
/// The local path merges externals INSIDE `present_with_blocks`
/// deliberately: deciding before the merge left the channel route
/// outside the decision, so a blocked park returned a source item
/// with no `Blocked` context to explain the silence around it
/// (codex on 344280f). A monitor that appended its source rows
/// after the decision would reintroduce exactly that, which is why
/// `owed_for` takes the externals rather than the caller merging
/// them (codex on ca2ea69).
#[tokio::test]
async fn a_source_event_wakes_a_blocked_agent_with_its_block() {
    use crate::cli::github::log::{EventLog, Transport};

    let repo = repo_with_roster();
    with_source(repo.path(), "codex", "deploys");
    let codex = AgentLabel::parse("codex").unwrap();
    crate::cli::block::asked(
        repo.path(),
        "codex",
        "why-is-this-here",
        "# why is this here\n\nasking the human\n",
    );

    let events = crate::agent_store::agents_root(repo.path()).join("codex/events");
    let key = match &hostable(repo.path(), &codex).unwrap()[0].0 {
        clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
        other => panic!("the fixture declares a command source, got {other:?}"),
    };
    EventLog::open(&events, &key)
        .unwrap()
        .append_inbox(
            Transport::Command,
            None,
            None,
            None,
            &WaitItem::CommandEvent {
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
        )
        .unwrap();

    let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: codex.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(
        repo.path(),
        &mut clients,
        &state,
        Some(&codex),
        &mut Sources::hosting(Default::default()),
    );

    let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
    let Note::Items { items, .. } = note else {
        panic!("a source event wakes a blocked agent, got {note:?}")
    };
    assert!(
        items.iter().any(|i| matches!(i, WaitItem::Blocked { .. })),
        "the block LEADS as context, or the agent has no way to know \
             why its plan work went quiet: {items:?}"
    );
    assert!(
        items
            .iter()
            .any(|i| matches!(i, WaitItem::CommandEvent { .. })),
        "and the event that woke it is still there: {items:?}"
    );
}

/// An emission no log holds still reaches the client, once.
///
/// Github's WAL warns once on an append failure and keeps
/// delivering, so the agent still learns about the event. A monitor
/// that presented only from the log would throw that away — the
/// channel is the only record of it (codex on 0555c8f). Driven by
/// the real discriminator rather than a flag: the item is simply
/// not in any row.
#[tokio::test]
async fn an_emission_no_log_holds_is_still_presented() {
    let repo = repo_with_roster();
    with_source(repo.path(), "codex", "deploys");
    let codex = AgentLabel::parse("codex").unwrap();
    let unlogged = WaitItem::GithubEvent {
        presentation: None,
        repo: "o/n".into(),
        event: "pr_opened".into(),
        detail: None,
        number: Some(7),
        title: None,
        actor: None,
        url: None,
        instructions: None,
        content: None,
    };

    let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: codex.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(
        repo.path(),
        &mut clients,
        &state,
        Some(&codex),
        &mut holding(vec![(codex.clone().into(), unlogged.clone())]),
    );

    let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
    let Note::Items { items, .. } = note else {
        panic!("an unlogged emission is still owed, got {note:?}")
    };
    assert_eq!(items, vec![unlogged.clone()]);

    // And provenance, not inspection, is what decides. The
    // producer saw the append succeed or fail; nothing a reader
    // can measure tells it apart, which is why two attempts to
    // infer it were wrong.
    use crate::cli::event_source::Durability;
    assert!(
        transient_of((unlogged.clone(), Durability::Logged)).is_empty(),
        "a logged emission is carried by the log, not beside it"
    );
    assert_eq!(
        transient_of((unlogged.clone(), Durability::Transient)),
        vec![unlogged],
        "and only a transient is carried here"
    );
}

/// A startup transient rides the SAME frame as the work that fold
/// already owed.
///
/// A client returns on its first non-empty `Items`, so sending the
/// projection first and the held transients second reaches a
/// client that has already gone — and a transient has no row to
/// replay, because the append is what failed. One presentation per
/// completed fold, carrying everything (codex on 328677d).
#[tokio::test]
async fn a_held_transient_rides_the_first_frame_with_the_work_it_found() {
    let repo = repo_with_roster();
    // Something the fold itself owes master, so the first frame is
    // non-empty on its own — which is the case that used to lose
    // the transient.
    std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
    std::fs::write(
        repo.path().join(".clank/queue/500-next.md"),
        "# next\n\nsomething to promote\n",
    )
    .unwrap();

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
    let mut sources = holding(vec![(claude.clone().into(), held.clone())]);

    let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: claude.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(repo.path(), &mut clients, &state, None, &mut sources);

    let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
    let Note::Items { items, .. } = note else {
        panic!("the fold owes work and a transient is held, got {note:?}")
    };
    assert!(
        items
            .iter()
            .any(|i| matches!(i, WaitItem::PromoteFromQueue { .. })),
        "the work the fold found: {items:?}"
    );
    assert!(
        items.contains(&held),
        "AND the transient, in the same frame — a second frame would \
             reach a client that already returned: {items:?}"
    );
    assert!(sources.pending().is_empty(), "delivered, so no longer held");
    assert!(heard.try_recv().is_err(), "and nothing follows it");
}

/// A transient for an author with nobody connected keeps waiting.
#[tokio::test]
async fn a_transient_with_no_client_is_held_rather_than_dropped() {
    let repo = repo_with_roster();
    let codex = AgentLabel::parse("codex").unwrap();
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
    let mut sources = holding(vec![(codex.clone().into(), held.clone())]);

    // Only ANOTHER author is connected.
    let (out, mut other) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: AgentLabel::parse("claude").unwrap(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(repo.path(), &mut clients, &state, None, &mut sources);

    assert_eq!(
        sources.pending(),
        vec![(codex.into(), held.clone())],
        "nothing else will ever hold it, so it waits for its author"
    );
    // And it did not reach the author who happened to be there:
    // a transient is one author's event, like any other.
    let note: Note = serde_json::from_str(other.recv().await.unwrap().trim_end()).unwrap();
    match note {
        Note::Done { what } => panic!("unexpected control answer: {what}"),
        Note::Refused { why } => panic!("unexpected refusal: {why}"),
        Note::Beat => {}
        Note::Recover { .. } => panic!("a wait is never sent a recovery"),
        Note::Superseded => panic!("nothing superseded this wait"),
        Note::Verdict(frame) => panic!("a plain wait is never sent a verdict: {frame:?}"),
        Note::Items { items, .. } => assert!(
            !items.contains(&held),
            "another author's transient must not be delivered here: {items:?}"
        ),
    }
}

/// An admission carries what was held for that author.
///
/// A transient retained while its author had no client is owed to
/// the first client that author gets. Admitting on ordinary work
/// without it would let the newcomer return on that work, leaving
/// the held event waiting for a tick that may never find an
/// eligible client (codex on 1552f66).
#[tokio::test]
async fn an_admission_carries_what_was_held_for_its_author() {
    let repo = repo_with_roster();
    // Ordinary work the fold owes master on its own.
    std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
    std::fs::write(
        repo.path().join(".clank/queue/500-next.md"),
        "# next\n\nsomething to promote\n",
    )
    .unwrap();

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
    let mut sources = holding(vec![(claude.clone().into(), held.clone())]);
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());

    let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = Vec::new();
    admit_into(
        repo.path(),
        &mut clients,
        Client {
            writer: None,
            who: Served::Agent {
                author: claude.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        },
        Some(&state),
        &mut sources,
    );

    let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
    let Note::Items { items, .. } = note else {
        panic!("admission owes both, got {note:?}")
    };
    assert!(
        items
            .iter()
            .any(|i| matches!(i, WaitItem::PromoteFromQueue { .. })),
        "the work admission found: {items:?}"
    );
    assert!(
        items.contains(&held),
        "AND what was held for this author, in the same frame: {items:?}"
    );
    assert!(sources.pending().is_empty(), "delivered, so no longer held");
}

/// A transient arriving after a client was woken stays pending.
///
/// A successful enqueue is not evidence anyone will read it: the
/// wait returned on its first non-empty frame, so a second one is
/// written into a socket nobody is listening to. Clearing the
/// pending buffer against that frame loses the event (codex on
/// 1552f66).
#[tokio::test]
async fn a_transient_after_a_wake_waits_for_the_next_admission() {
    let repo = repo_with_roster();
    std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
    std::fs::write(
        repo.path().join(".clank/queue/500-next.md"),
        "# next\n\nsomething to promote\n",
    )
    .unwrap();

    let claude = AgentLabel::parse("claude").unwrap();
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    // The receiver is HELD open, so an enqueue would succeed — which
    // is exactly what made this look safe.
    let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: claude.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    let mut sources = Sources::hosting(Default::default());

    // One ordinary wake.
    let _ = serve_to(repo.path(), &mut clients, &state, None, &mut sources);
    assert!(
        matches!(
            serde_json::from_str::<Note>(heard.recv().await.unwrap().trim_end()).unwrap(),
            Note::Items { .. }
        ),
        "the first frame wakes it"
    );
    assert!(clients.is_empty(), "and ends that connection");

    // Now a transient. There is no eligible client, so it waits.
    let later = WaitItem::CommandEvent {
        presentation: None,
        source_cwd: None,
        name: "deploys".into(),
        id: "b".into(),
        event: "deploys".into(),
        content: "and then this".into(),
        at: None,
        failure: false,
        requires_reply: false,
        reply_route: None,
        selector: None,
    };
    sources.hold_one((claude.clone().into(), later.clone()));
    let _ = serve_to(repo.path(), &mut clients, &state, None, &mut sources);
    assert_eq!(
        sources.pending(),
        vec![(claude.clone().into(), later.clone())],
        "it must not be cleared against a frame nobody will read"
    );
    assert!(
        heard.try_recv().is_err(),
        "and nothing was queued behind the wake"
    );

    // The next admission is what receives it.
    let (out, mut next) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    admit_into(
        repo.path(),
        &mut clients,
        Client {
            writer: None,
            who: Served::Agent {
                author: claude.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        },
        Some(&state),
        &mut sources,
    );
    let note: Note = serde_json::from_str(next.recv().await.unwrap().trim_end()).unwrap();
    let Note::Items { items, .. } = note else {
        panic!("the next admission is owed it, got {note:?}")
    };
    assert!(items.contains(&later), "{items:?}");
    assert!(sources.pending().is_empty());
}

/// The monitor serves an author's CONFIGURED logs and no others.
///
/// Both logs sit in one directory and only the configured row is
/// served, because the monitor opens the keys it resolves from
/// config rather than whatever is in the directory — so a scoped
/// log is never one it reads.
///
/// Exclusion only. It starts no producer and has no invocation
/// receiver, and an earlier version of this comment claimed both;
/// the delivery half is
/// `wait::a_cli_author_keeps_its_own_channel_while_another_shares`
/// (codex on 1756c68).
#[tokio::test]
async fn the_monitor_reads_only_an_authors_configured_logs() {
    use crate::cli::github::log::{EventLog, Transport};

    let repo = repo_with_roster();
    with_source(repo.path(), "claude", "builds");
    let claude = AgentLabel::parse("claude").unwrap();
    let events = crate::agent_store::agents_root(repo.path())
        .join("claude")
        .join("events");

    // What the monitor hosts: this author's configured source, with
    // a row in it.
    let configured = match &hostable(repo.path(), &claude).unwrap()[0].0 {
        clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
        other => panic!("a command source, got {other:?}"),
    };
    let row = |content: &str| WaitItem::CommandEvent {
        presentation: None,
        source_cwd: None,
        name: "builds".into(),
        id: content.into(),
        event: "builds".into(),
        content: content.into(),
        at: None,
        failure: false,
        requires_reply: false,
        reply_route: None,
        selector: None,
    };
    EventLog::open(&events, &configured)
        .unwrap()
        .append_inbox(Transport::Command, None, None, None, &row("configured"))
        .unwrap();

    // And an INVOCATION's own source, under its scoped key, with a
    // row of its own. This is the log the monitor must not read.
    let invocation = crate::cli::command_source::storage_key(
        configured.clone(),
        crate::cli::event_source::SourceOrigin::Cli,
        "inv-9",
    );
    assert_ne!(invocation, configured);
    EventLog::open(&events, &invocation)
        .unwrap()
        .append_inbox(Transport::Command, None, None, None, &row("invocation"))
        .unwrap();

    let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: claude.clone(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(
        repo.path(),
        &mut clients,
        &state,
        None,
        &mut Sources::hosting(Default::default()),
    );

    let Note::Items { items, .. } =
        serde_json::from_str::<Note>(heard.recv().await.unwrap().trim_end()).unwrap()
    else {
        panic!("the configured row is owed")
    };
    let contents: Vec<String> = items
        .iter()
        .filter_map(|i| match i {
            WaitItem::CommandEvent { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        contents,
        vec!["configured".to_string()],
        "the monitor serves the author's CONFIGURED rows and cannot \
             see an invocation's: it resolves only config keys, so a \
             scoped log is not a log it reads — {contents:?}"
    );
}

/// Two authors in one repo: each one's rows reach only that author.
///
/// Both have a config source, so the monitor hosts both, and the
/// question is whether a row logged for one can reach the other —
/// the failure that would make hosting worse than not hosting.
/// CONFIG routing only; the mixed config/CLI case is its own test.
#[tokio::test]
async fn two_authors_sources_do_not_cross() {
    use crate::cli::github::log::{EventLog, Transport};

    let repo = repo_with_roster();
    with_source(repo.path(), "codex", "deploys");
    with_source(repo.path(), "claude", "builds");

    let row = |who: &str, content: &str| {
        let author = AgentLabel::parse(who).unwrap();
        let events = crate::agent_store::agents_root(repo.path())
            .join(who)
            .join("events");
        let key = match &hostable(repo.path(), &author).unwrap()[0].0 {
            clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
            other => panic!("a command source, got {other:?}"),
        };
        EventLog::open(&events, &key)
            .unwrap()
            .append_inbox(
                Transport::Command,
                None,
                None,
                None,
                &WaitItem::CommandEvent {
                    presentation: None,
                    source_cwd: None,
                    name: "s".into(),
                    id: content.into(),
                    event: "s".into(),
                    content: content.into(),
                    at: None,
                    failure: false,
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();
    };
    row("codex", "theirs");
    row("claude", "mine");

    let (a, mut codex_heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (b, mut claude_heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    // Both are plain shared clients. Invocation-source isolation is
    // covered by `an_invocations_event_reaches_it_and_not_the_monitor`.
    let agent = |who: &str, out| Client {
        writer: None,
        who: Served::Agent {
            author: AgentLabel::parse(who).unwrap(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    };
    let mut clients = vec![agent("codex", a), agent("claude", b)];
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(
        repo.path(),
        &mut clients,
        &state,
        None,
        &mut Sources::hosting(Default::default()),
    );

    let content = |line: String| -> Vec<String> {
        let Note::Items { items, .. } = serde_json::from_str(line.trim_end()).unwrap() else {
            panic!("owed its own row")
        };
        items
            .iter()
            .filter_map(|i| match i {
                WaitItem::CommandEvent { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        content(codex_heard.recv().await.unwrap()),
        vec!["theirs".to_string()],
        "codex is owed its own row and nothing of claude's"
    );
    assert_eq!(
        content(claude_heard.recv().await.unwrap()),
        vec!["mine".to_string()],
        "and claude the same, the other way"
    );
    assert!(
        clients.is_empty(),
        "both were woken, and a woken connection is spent"
    );
}

/// A newcomer is presented to ALONE.
///
/// A client's queue is `CLIENT_QUEUE` deep, so presenting to
/// everybody on each arrival lets a burst of joins retire
/// established readers that were keeping up perfectly well — the
/// same amplification the source path avoids, through the door the
/// source path does not use (codex on 0555c8f).
#[tokio::test]
async fn arrivals_do_not_put_frames_in_established_clients_queues() {
    let repo = repo_with_roster();
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let (out, mut established) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Agent {
            author: AgentLabel::parse("claude").unwrap(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];

    // More arrivals than a queue is deep, none of them drained.
    // Their receivers are HELD: a dropped one closes the channel,
    // which would retire the newcomer for a reason this test is
    // not about.
    let mut newcomers = Vec::new();
    for _ in 0..(CLIENT_QUEUE + 2) {
        let (out, rx) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        newcomers.push(rx);
        admit_into(
            repo.path(),
            &mut clients,
            Client {
                writer: None,
                who: Served::Agent {
                    author: AgentLabel::parse("codex").unwrap(),
                },
                out,
                holder: None,
                shown: Default::default(),
                conn: 0,
                admitted: Default::default(),
                unavailable: false,
                report: None,
                farewell: None,
            },
            Some(&state),
            &mut Sources::hosting(Default::default()),
        );
    }

    assert!(
        established.try_recv().is_err(),
        "an established client is sent nothing because others joined"
    );
    assert!(
        clients.iter().any(|c| c.who.name() == "claude"),
        "and it is still connected: {:?}",
        clients.iter().map(|c| c.who.name()).collect::<Vec<_>>()
    );
}

/// A source signal presents to that author's clients and no others.
///
/// Scope matters beyond tidiness: a client's queue is only
/// `CLIENT_QUEUE` deep, so presenting to everybody whenever one
/// author's log moved would put frames in queues that had no
/// reason to receive them, and a burst would retire readers that
/// were keeping up.
#[tokio::test]
async fn a_source_signal_presents_only_to_its_own_author() {
    let repo = repo_with_roster();
    let (mine, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (theirs, mut unheard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (watching, mut observer) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let agent = |author: &str, out| Client {
        writer: None,
        who: Served::Agent {
            author: AgentLabel::parse(author).unwrap(),
        },
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    };
    let mut clients = vec![
        agent("codex", mine),
        agent("claude", theirs),
        Client {
            writer: None,
            who: Served::Observer,
            out: watching,
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        },
    ];

    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let _ = serve_to(
        repo.path(),
        &mut clients,
        &state,
        Some(&AgentLabel::parse("codex").unwrap()),
        &mut Sources::hosting(Default::default()),
    );

    assert!(
        heard.try_recv().is_ok(),
        "the author whose log moved is presented to"
    );
    assert!(
        unheard.try_recv().is_err(),
        "another author's client must hear nothing"
    );
    assert!(
        observer.try_recv().is_err(),
        "and an observer is not owed anybody's items"
    );
    assert_eq!(clients.len(), 3, "nobody was retired for this");

    // Unscoped, everybody is presented to — which is what a fold
    // tick does and why the scope is a parameter rather than the
    // rule.
    let _ = serve_to(
        repo.path(),
        &mut clients,
        &state,
        None,
        &mut Sources::hosting(Default::default()),
    );
    assert!(unheard.try_recv().is_ok());
    assert!(observer.try_recv().is_ok());
}

/// An observer is told the fold MOVED and never what anyone owes.
///
/// Items are a projection of one author's work; an observer has no
/// author, and sending it somebody's items would leak one agent's
/// work to a panel — besides costing the refold the plan deletes.
#[tokio::test]
async fn an_observer_gets_beats_and_never_items() {
    let repo = repo_with_roster();
    let (out, mut queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let mut clients = vec![Client {
        writer: None,
        who: Served::Observer,
        out,
        holder: None,
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: None,
    }];
    // A repo with work to do: a queued plan is something an AGENT
    // would be handed, so an observer seeing a Beat here is the
    // routing and not an empty repo.
    std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
    std::fs::write(
        repo.path().join(".clank/queue/next.md"),
        "# next

something to promote
",
    )
    .unwrap();
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());

    serve(repo.path(), &mut clients, &state);
    assert_eq!(clients.len(), 1, "an observer stays registered");
    let line = queued.recv().await.expect("it was sent something");
    assert_eq!(
        serde_json::from_str::<Note>(line.trim_end()).unwrap(),
        Note::Beat,
        "the fold moved, and that is all an observer is told"
    );
}
