use super::*;
use crate::cli::monitor::fixtures::*;

const ROSTER: &str = r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#;

fn repo() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".clank")).unwrap();
    std::fs::write(d.path().join(".clank/config.json"), ROSTER).unwrap();
    d
}

fn claude() -> AgentLabel {
    AgentLabel::parse("claude").unwrap()
}

fn bind(repo: &std::path::Path, session: &str) {
    crate::agent_store::bind_session_to_agent(
        repo,
        &claude(),
        clank_core::Tool::Claude,
        &clank_core::SessionId::parse(session).unwrap(),
    )
    .unwrap();
}

fn holder(session: &str) -> Option<Holder> {
    Some(Holder {
        tool: clank_core::Tool::Claude,
        session: session.into(),
    })
}

/// A parked agent client with a queue the test reads, and the slot
/// its farewell lands in.
fn parked(
    who: Served,
    holder: Option<Holder>,
) -> (
    Client,
    tokio::sync::mpsc::Receiver<String>,
    tokio::sync::oneshot::Receiver<String>,
) {
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (farewell, last) = tokio::sync::oneshot::channel();
    (
        Client {
            who,
            holder,
            out,
            writer: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: Some(farewell),
        },
        queued,
        last,
    )
}

fn agent() -> Served {
    Served::Agent { author: claude() }
}

fn admit(repo: &std::path::Path, clients: &mut Vec<Client>, client: Client) {
    let _ = admit_into(
        repo,
        clients,
        client,
        None,
        &mut Sources::hosting(Default::default()),
    );
}

/// The wait admitted last is the session's: the older one is told it
/// is superseded and dropped, so only the newer is ever served.
#[tokio::test]
async fn a_newer_wait_of_the_same_session_supersedes_the_older() {
    let repo = repo();
    bind(repo.path(), "session-a1");
    let mut clients = Vec::new();
    let (older, mut older_queue, older_farewell) = parked(agent(), holder("session-a1"));
    admit(repo.path(), &mut clients, older);
    let (newer, _queue, _farewell) = parked(agent(), holder("session-a1"));
    admit(repo.path(), &mut clients, newer);

    assert_eq!(clients.len(), 1);
    let line = older_farewell.await.expect("the older wait was told");
    assert_eq!(
        serde_json::from_str::<Note>(line.trim_end()).unwrap(),
        Note::Superseded
    );
    assert!(older_queue.recv().await.is_none(), "its queue is closed");
}

/// Only the same agent session's wait is replaced: observers,
/// helpers, holderless clients, other sessions and other authors are
/// not.
#[test]
fn only_the_same_agent_session_is_the_same_wait() {
    let new = parked(agent(), holder("s1")).0;
    let same = |who, holder| same_wait(&parked(who, holder).0, &new);
    assert!(same(agent(), holder("s1")));
    assert!(!same(agent(), holder("s2")));
    assert!(!same(agent(), None));
    assert!(!same(Served::Observer, holder("s1")));
    assert!(!same(Served::Helper { author: claude() }, holder("s1")));
    assert!(!same(
        Served::Agent {
            author: AgentLabel::parse("codex").unwrap()
        },
        holder("s1")
    ));
    let holderless = parked(agent(), None).0;
    assert!(!same_wait(&parked(agent(), None).0, &holderless));
}

/// A newcomer the admission rules refuse displaces nothing.
#[tokio::test]
async fn a_refused_newcomer_displaces_nothing() {
    let repo = repo();
    bind(repo.path(), "session-a1");
    let mut clients = Vec::new();
    let (older, _queue, mut older_farewell) = parked(agent(), holder("session-a1"));
    admit(repo.path(), &mut clients, older);
    bind(repo.path(), "session-b2");
    let (stale, _q, _f) = parked(agent(), holder("session-a1"));
    admit(repo.path(), &mut clients, stale);
    assert_eq!(clients.len(), 1);
    assert!(
        older_farewell.try_recv().is_err(),
        "the older was superseded by a refusal"
    );
}

/// The farewell is written after whatever was queued, by the same
/// writer, so a full queue cannot drop it.
#[tokio::test]
async fn the_farewell_survives_a_full_queue() {
    use tokio::io::AsyncBufReadExt;
    let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (farewell, last) = tokio::sync::oneshot::channel();
    let beat = format!("{}\n", serde_json::to_string(&Note::Beat).unwrap());
    for _ in 0..CLIENT_QUEUE {
        out.try_send(beat.clone()).unwrap();
    }
    assert!(out.try_send(beat.clone()).is_err(), "the queue is full");
    let writer = tokio::spawn(write_to_within(
        ours,
        queued,
        Some(last),
        "claude".into(),
        Duration::from_secs(2),
    ));
    let client = Client {
        who: agent(),
        holder: holder("s1"),
        out,
        writer: Some(writer),
        shown: Default::default(),
        conn: 0,
        admitted: Default::default(),
        unavailable: false,
        report: None,
        farewell: Some(farewell),
    };
    let writer = supersede(client).unwrap();
    let mut lines = tokio::io::BufReader::new(theirs).lines();
    let mut got = Vec::new();
    while let Some(line) = lines.next_line().await.unwrap() {
        got.push(serde_json::from_str::<Note>(&line).unwrap());
    }
    let mut want = vec![Note::Beat; CLIENT_QUEUE];
    want.push(Note::Superseded);
    assert_eq!(got, want);
    assert_eq!(writer.await.unwrap(), Wrote::Drained);
}

/// A delivery already dispatched is not retracted: the served wait
/// has left the parked set, so a later admission of its session
/// supersedes nothing and is owed work by the normal rules.
#[tokio::test]
async fn a_dispatched_delivery_stands() {
    let repo = repo();
    bind(repo.path(), "session-a1");
    std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
    std::fs::write(
        repo.path().join(".clank/queue/500-next.md"),
        "# next\n\nsomething to promote\n",
    )
    .unwrap();
    let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
    let mut clients = Vec::new();
    let (first, mut first_queue, mut first_farewell) = parked(agent(), holder("session-a1"));
    let _ = admit_into(
        repo.path(),
        &mut clients,
        first,
        Some(&state),
        &mut Sources::hosting(Default::default()),
    );
    let served: Note = serde_json::from_str(first_queue.recv().await.unwrap().trim_end()).unwrap();
    assert!(matches!(served, Note::Items { .. }), "{served:?}");

    let (second, mut second_queue, _f) = parked(agent(), holder("session-a1"));
    let _ = admit_into(
        repo.path(),
        &mut clients,
        second,
        Some(&state),
        &mut Sources::hosting(Default::default()),
    );
    assert!(
        first_farewell.try_recv().is_err(),
        "a dispatched wait was superseded after the fact"
    );
    let owed: Note = serde_json::from_str(second_queue.recv().await.unwrap().trim_end()).unwrap();
    assert!(matches!(owed, Note::Items { .. }), "{owed:?}");
}
