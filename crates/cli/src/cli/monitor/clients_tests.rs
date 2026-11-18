use super::*;
use crate::cli::monitor::fixtures::*;

/// Over a real socket: the client is TOLD why, and the accept path
/// reports a retirement rather than a client.
///
/// The answer goes out before the process does, so a client is not
/// left reading an abrupt EOF and guessing whether it was refused
/// or the monitor died.
#[tokio::test]
async fn an_obsolete_monitor_answers_before_it_stands_down() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let repo = repo_with_roster();
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4488).unwrap();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let line = serde_json::to_string(&Hello {
        proto: PROTO + 1,
        build: None,
        repo: repo.path().to_string_lossy().to_string(),
        who: agent("codex"),
    })
    .unwrap();

    let heard = tokio::spawn(async move {
        let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
        c.write_all(format!("{line}\n").as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::io::BufReader::new(c)
            .read_line(&mut answer)
            .await
            .unwrap();
        answer
    });

    let (stream, _) = listener.accept().await.unwrap();
    let greeted = greet(stream, repo.path()).await.expect("it answers");
    assert!(
        matches!(greeted, Greeted::StandDown),
        "a newer client is not a client this monitor can serve: {greeted:?}"
    );

    let answer = heard.await.unwrap();
    match serde_json::from_str::<Welcome>(answer.trim_end()) {
        Ok(Welcome::Decline { proto, why }) => {
            assert_eq!(proto, PROTO);
            assert!(
                why.contains("proto"),
                "it says what the mismatch was: {why}"
            );
        }
        other => panic!("the client must be told, not dropped: {other:?}"),
    }
}

fn hello_for(repo: &std::path::Path, author: &str) -> String {
    serde_json::to_string(&Hello {
        proto: PROTO,
        build: None,
        repo: repo.to_string_lossy().to_string(),
        who: agent(author),
    })
    .unwrap()
}

/// A hello that arrives AFTER the accept, in pieces.
///
/// The production path used `into_std` and read synchronously.
/// Tokio keeps `O_NONBLOCK` across that conversion and `read_frame`
/// reads `WouldBlock` as the deadline, so a healthy client whose
/// bytes had not landed yet was refused instantly (codex on
/// 13e9fa7).
#[tokio::test]
async fn a_hello_arriving_late_and_in_fragments_is_admitted() {
    use tokio::io::AsyncWriteExt;

    let repo = repo_with_roster();
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4242).unwrap();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    let line = hello_for(repo.path(), "codex");
    let (first, second) = line.split_at(line.len() / 2);
    let first = first.to_string();
    let second = format!("{second}\n");
    tokio::spawn(async move {
        let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
        // Nothing at all for a moment, then half, then the rest.
        tokio::time::sleep(Duration::from_millis(60)).await;
        client.write_all(first.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
        client.write_all(second.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let (stream, _) = listener.accept().await.unwrap();
    let client = greet(stream, repo.path())
        .await
        .expect("a fragmented hello is still a hello")
        .client();
    assert_eq!(
        client.who,
        Served::Agent {
            author: AgentLabel::parse("codex").unwrap(),
        }
    );
}

/// One silent peer must not hold up anybody else.
#[tokio::test]
async fn a_silent_client_does_not_delay_a_healthy_one() {
    use tokio::io::AsyncWriteExt;

    let repo = repo_with_roster();
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4243).unwrap();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    // Connects and says nothing, ever.
    let quiet_path = path.clone();
    let quiet = tokio::spawn(async move {
        let _s = tokio::net::UnixStream::connect(&quiet_path).await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let (silent, _) = listener.accept().await.unwrap();
    let repo_for_task = repo.path().to_path_buf();
    // As `start` does: the handshake goes off the accept loop.
    let stalled = tokio::spawn(async move { greet(silent, &repo_for_task).await });

    let line = format!("{}\n", hello_for(repo.path(), "claude"));
    tokio::spawn(async move {
        let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
        c.write_all(line.as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let began = std::time::Instant::now();
    let (healthy, _) = listener.accept().await.unwrap();
    let admitted = greet(healthy, repo.path())
        .await
        .expect("admitted")
        .client();
    let took = began.elapsed();

    assert_eq!(admitted.who.name(), "claude");
    assert!(
        took < Duration::from_secs(1),
        "the healthy client waited {took:?} behind a silent one"
    );
    assert!(!stalled.is_finished(), "the silent one is still pending");
    stalled.abort();
    quiet.abort();
}

/// An author the roster does not know is declined BEFORE admission:
/// welcoming one would register a connection nothing can serve.
#[tokio::test]
async fn an_author_off_the_roster_is_declined_not_registered() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let repo = repo_with_roster();
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4244).unwrap();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    let line = format!("{}\n", hello_for(repo.path(), "ghost"));
    let heard = tokio::spawn(async move {
        let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
        c.write_all(line.as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::io::BufReader::new(c)
            .read_line(&mut answer)
            .await
            .unwrap();
        answer
    });

    let (stream, _) = listener.accept().await.unwrap();
    let refused = greet(stream, repo.path()).await;
    assert!(refused.is_err(), "`ghost` must not be registered");

    // And it is told why, rather than left hanging.
    let answer = heard.await.unwrap();
    let welcome: Welcome = serde_json::from_str(answer.trim_end()).unwrap();
    match welcome {
        Welcome::Decline { why, .. } => assert!(why.contains("roster"), "{why}"),
        other => panic!("expected a decline, got {other:?}"),
    }
}

/// A writer that cannot drain gives up, closing its socket.
///
/// Ignoring the timeout and looping instead leaves the task and its
/// fd alive forever. The owner still retires the client, so the
/// registry looks correct while a task and a socket leak — the
/// accumulation this whole plan is about, in a new place.
#[tokio::test]
async fn a_writer_that_cannot_drain_gives_up_and_closes() {
    let (mine, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
    assert!(wedge(&mine) > 0, "the wedge wrote nothing");
    mine.set_nonblocking(true).unwrap();
    let mine = tokio::net::UnixStream::from_std(mine).unwrap();

    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let task = tokio::spawn(write_to_within(
        mine,
        queued,
        None,
        "codex".to_string(),
        Duration::from_millis(80),
    ));
    out.try_send("one\n".to_string()).unwrap();

    // It must END, not retry forever.
    let ended = tokio::time::timeout(Duration::from_secs(2), task).await;
    assert!(
        ended.is_ok(),
        "the writer is still trying; its socket will never close"
    );
}

/// And a writer whose peer has GONE gives up too.
///
/// A separate path from the stall: this one fails rather than
/// hangs, so a timeout never fires and ignoring the error would
/// loop on a dead socket forever.
#[tokio::test]
async fn a_writer_whose_peer_closed_gives_up() {
    let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(peer);
    mine.set_nonblocking(true).unwrap();
    let mine = tokio::net::UnixStream::from_std(mine).unwrap();

    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let task = tokio::spawn(write_to_within(
        mine,
        queued,
        None,
        "codex".to_string(),
        // Long on purpose: only an ERROR can end this, not the
        // budget.
        Duration::from_secs(30),
    ));
    // Enough to be certain of EPIPE rather than a short write.
    for _ in 0..CLIENT_QUEUE {
        let _ = out.try_send("x".repeat(4096));
    }

    let ended = tokio::time::timeout(Duration::from_secs(2), task).await;
    assert!(
        ended.is_ok(),
        "the writer is still trying to reach a closed peer"
    );
}

/// A peer that says nothing is given up on at the budget, not kept
/// forever: the monitor would otherwise accumulate half-open
/// registrations nobody can serve.
#[tokio::test]
async fn a_hello_that_never_arrives_is_given_up_on() {
    let repo = repo_with_roster();
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4245).unwrap();
    let listener = tokio::net::UnixListener::bind(&path).unwrap();

    let held = tokio::spawn(async move {
        let _s = tokio::net::UnixStream::connect(&path).await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });

    let (stream, _) = listener.accept().await.unwrap();
    let began = std::time::Instant::now();
    let why = greet_within(stream, repo.path(), Duration::from_millis(120))
        .await
        .expect_err("a client that never speaks is not admitted")
        .to_string();
    let took = began.elapsed();

    assert!(why.contains("did not finish its hello"), "{why}");
    assert!(
        took < Duration::from_secs(1),
        "waited {took:?} — the peer stays open, so this is the budget or nothing"
    );
    held.abort();
}

/// An observer needs no roster entry; an agent still does.
///
/// A panel is not an agent. Requiring an identity would have meant
/// inventing a roster entry for `clank tui`, and refusing one
/// would have meant the panel keeps its own watcher forever —
/// which is the duplication this plan is here to delete.
///
/// Through `greet`, because the roster check lives in `admit` and
/// not in `accept_hello`: asserting on the parsed frame proved
/// only that the frame parses, which the mutation check caught.
#[tokio::test]
async fn an_observer_is_admitted_without_being_on_the_roster() {
    async fn admitted(repo: &std::path::Path, who: Who, pid: i32) -> anyhow::Result<Greeted> {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), pid).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let line = serde_json::to_string(&Hello {
            proto: PROTO,
            build: None,
            repo: repo.to_string_lossy().to_string(),
            who,
        })
        .unwrap();
        tokio::spawn(async move {
            let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
            client
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });
        let (stream, _) = listener.accept().await.unwrap();
        greet(stream, repo).await
    }

    let repo = repo_with_roster();
    let client = admitted(repo.path(), Who::Observer, 4401)
        .await
        .expect("an observer is welcome")
        .client();
    assert_eq!(client.who, Served::Observer);

    // `nobody` is emphatically not on that roster, and the same
    // handshake must still refuse it: having no identity is not
    // the same as claiming one nobody knows.
    let why = admitted(repo.path(), agent("nobody"), 4402)
        .await
        .expect_err("an unknown agent is not admitted")
        .to_string();
    assert!(why.contains("not on this repo's roster"), "{why}");
}
