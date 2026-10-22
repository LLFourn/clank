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
    use clank_core::turn_policy::{AttendRequest, Handoff, Selector};

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

    let g = Gathered::of(repo.path(), &author, Some((&[], None)), Ok(()));
    let mut turns = Turns::new(repo.path().to_path_buf());
    turns
        .attend(
            &author,
            AttendRequest::Attend {
                session: session.clone(),
                selector: Selector::Name("sleeper".into()),
                desc: None,
                expect: 600_000,
                shell: None,
            },
            &g,
        )
        .unwrap();
    let handoff = Report::Handoff {
        handoff: Handoff {
            session: session.clone(),
            wait_running: false,
            continued: false,
        },
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
