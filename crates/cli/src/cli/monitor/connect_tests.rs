use super::*;
use crate::cli::monitor::fixtures::*;

#[test]
fn a_client_attaches_and_the_monitor_hears_who_it_is() {
    let repo = repo_dir();
    let (_dir, served) = fake_monitor(repo.path(), Some(welcome_line()));

    let holder = Some(Holder {
        tool: clank_core::vocab::Tool::Codex,
        session: "s-9".into(),
    });
    let got = attach(
        repo.path(),
        Who::Agent {
            author: "codex".into(),
            holder: holder.clone(),
            has_cli_sources: true,
            turn_end: None,
        },
    );
    assert!(matches!(got, Attached::Shared(_)), "{got:?}");

    let heard = served.join().unwrap().expect("a hello arrived");
    assert_eq!(
        heard.who,
        Who::Agent {
            author: "codex".into(),
            holder,
            has_cli_sources: true,
            turn_end: None,
        },
        "the whole identity travelled, pair and sources alike"
    );
}

#[test]
fn a_declining_monitor_sends_the_client_off_alone() {
    let repo = repo_dir();
    let decline = format!(
        "{}\n",
        serde_json::to_string(&Welcome::Decline {
            proto: PROTO,
            why: "serves somewhere else".into(),
        })
        .unwrap()
    );
    let (_dir, served) = fake_monitor(repo.path(), Some(decline));
    let got = attach(repo.path(), agent("codex"));
    served.join().unwrap();
    match got {
        Attached::Alone(why) => assert!(why.contains("declined"), "{why}"),
        other => panic!("expected Alone, got {other:?}"),
    }
}

/// Every way of failing is a reason to work alone, never to wait.
#[test]
fn nothing_about_a_missing_monitor_is_fatal() {
    let repo = repo_dir();

    let nothing = attach(repo.path(), agent("codex"));
    assert!(
        matches!(&nothing, Attached::Alone(why) if why.contains("no monitor published")),
        "{nothing:?}"
    );

    // A descriptor whose monitor is gone: the socket refuses.
    publish(
        repo.path(),
        &Descriptor {
            proto: PROTO,
            pid: 1,
            repo: repo.path().to_string_lossy().to_string(),
            socket: "/tmp/clank-socks-501/nobody-home.sock".into(),
            build: None,
        },
    )
    .unwrap();
    let dead = attach(repo.path(), agent("codex"));
    assert!(
        matches!(&dead, Attached::Alone(why) if why.contains("cannot reach")),
        "{dead:?}"
    );

    // And one for a repo that is not this one.
    publish(
        repo.path(),
        &Descriptor {
            proto: PROTO,
            pid: 1,
            repo: "/w/elsewhere".into(),
            socket: "/tmp/clank-socks-501/elsewhere.sock".into(),
            build: None,
        },
    )
    .unwrap();
    let foreign = attach(repo.path(), agent("codex"));
    assert!(
        matches!(&foreign, Attached::Alone(why) if why.contains("serves /w/elsewhere")),
        "{foreign:?}"
    );
}

/// The read-ahead bug: a welcome and the first note in ONE write.
///
/// `read_line` fills its buffer in chunks, so a throwaway reader
/// swallows whatever followed the newline. The note must survive
/// the handshake and be readable exactly once (codex on af1efd2).
#[test]
fn a_note_written_with_the_welcome_is_not_lost() {
    let repo = repo_dir();
    let note = Note::Items {
        source_cwd: None,
        items: vec![WaitItem::PromoteFromQueue {
            name: "next".into(),
            priority: 500,
        }],
    };
    let coalesced = format!(
        "{}{}\n",
        welcome_line(),
        serde_json::to_string(&note).unwrap()
    );
    let (_dir, served) = fake_monitor(repo.path(), Some(coalesced));

    let mut conn = match attach(repo.path(), agent("codex")) {
        Attached::Shared(conn) => conn,
        other => panic!("expected Shared, got {other:?}"),
    };
    let got = conn
        .note(std::time::Instant::now() + Duration::from_secs(2))
        .expect("the note survived the handshake");
    served.join().unwrap();
    assert_eq!(got, note, "read exactly once, from the retained buffer");
}

/// Frames already delivered by a monitor that has since EXITED.
///
/// The bytes are in the receive buffer and nothing is left to wait
/// for, so a departed peer must cost the client nothing. Bounding
/// the reads with `SO_RCVTIMEO` instead of `poll` fails here with
/// `EINVAL` on a perfectly good five-second deadline: Darwin's
/// `sosetopt` refuses every option once both directions are shut.
/// That is the watchdog's own self-eviction — it would drop the
/// wake it had just delivered. Every other fixture here keeps its
/// peer alive, which is why none of them can see this.
#[test]
fn frames_from_a_peer_that_has_gone_are_still_read() {
    use std::io::Write;
    let (mine, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
    // Non-blocking, as `connect_within` hands it back.
    mine.set_nonblocking(true).unwrap();
    theirs.write_all(b"first\nsecond\n").unwrap();
    drop(theirs);

    let mut reader = std::io::BufReader::new(mine);
    let mut partial = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    assert_eq!(
        read_frame(&mut reader, &mut partial, deadline).unwrap(),
        "first"
    );
    assert_eq!(
        read_frame(&mut reader, &mut partial, deadline).unwrap(),
        "second"
    );
    assert_eq!(
        read_frame(&mut reader, &mut partial, deadline)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof,
        "and the EOF after them is reported as one"
    );
}

/// A frame split across two short deadlines still arrives whole.
///
/// `forward_beats` gives each read a short deadline so it can
/// notice it has been asked to stop. The bytes it has already
/// taken out of the socket cannot be read again, so losing them on
/// that deadline leaves the next read starting mid-frame — and
/// every decode after it fails, turning a healthy subscription
/// into a permanent local watcher. A deadline may cost time, never
/// data.
#[test]
fn a_frame_split_across_deadlines_is_not_lost() {
    use std::io::Write;
    let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    mine.set_nonblocking(true).unwrap();
    let mut conn = Conn::over(mine);

    let first = serde_json::to_string(&Note::Beat).unwrap();
    let second = serde_json::to_string(&Note::Beat).unwrap();
    let (head, tail) = first.split_at(first.len() / 2);

    // Half a frame, then a deadline passes with nothing more.
    (&mut &peer).write_all(head.as_bytes()).unwrap();
    let kind = conn
        .note(std::time::Instant::now() + Duration::from_millis(30))
        .map_err(|e| e.kind())
        .expect_err("half a frame is not a frame");
    assert_eq!(kind, std::io::ErrorKind::TimedOut);

    // The rest arrives, and so does another frame behind it.
    (&mut &peer)
        .write_all(format!("{tail}\n{second}\n").as_bytes())
        .unwrap();
    assert_eq!(
        conn.note(std::time::Instant::now() + Duration::from_secs(2))
            .expect("the frame resumed where it left off"),
        Note::Beat,
        "the prefix survived the deadline"
    );
    assert_eq!(
        conn.note(std::time::Instant::now() + Duration::from_secs(2))
            .expect("and the next frame is intact"),
        Note::Beat,
    );
}

/// A peer that never stops sending and never sends a newline.
///
/// Every read is ready, so nothing ever waits — and the budget was
/// enforced only where something waits. Bytes are buffered first
/// and the peer then dropped, so no read in this test CAN block:
/// a deadline honoured here is one honoured while making progress.
/// Without that, the call runs to the EOF behind the junk and
/// reports it, having bought the whole stream a place in `line`.
#[test]
fn the_deadline_binds_while_bytes_are_still_arriving() {
    use std::io::Write;
    let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    mine.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();

    let junk = vec![b'x'; 64 * 1024];
    let mut sent = 0usize;
    while let Ok(n) = (&mut &peer).write(&junk) {
        if n == 0 {
            break;
        }
        sent += n;
    }
    drop(peer);
    assert!(
        sent > 1024,
        "the fixture buffered an unterminated frame: {sent}"
    );

    let kind = read_frame(
        &mut std::io::BufReader::new(mine),
        &mut Vec::new(),
        std::time::Instant::now() + Duration::from_micros(1),
    )
    .map_err(|e| e.kind())
    .expect_err("an unterminated frame is not a frame");
    assert_eq!(
        kind,
        std::io::ErrorKind::TimedOut,
        "the budget must bind on a ready stream, not just a stalled one"
    );
}

/// A budget already spent writes nothing.
///
/// `attach_by` shares ONE deadline across the hello and the
/// welcome, so a connect that consumed it leaves this with nothing
/// left. The socket here has room and would take the frame
/// instantly, so no write can block: only a check on the PROGRESS
/// path can stop it, and the one in `wait_ready` never gets a turn.
#[test]
fn a_spent_budget_writes_nothing() {
    use std::io::Read;
    let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
    mine.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();

    let kind = write_frame(&mine, b"hello\n", std::time::Instant::now())
        .map_err(|e| e.kind())
        .expect_err("a spent budget is not a write");
    assert_eq!(kind, std::io::ErrorKind::TimedOut);

    let mut got = [0u8; 8];
    let read = (&mut &peer).read(&mut got).map_err(|e| e.kind());
    assert_eq!(
        read,
        Err(std::io::ErrorKind::WouldBlock),
        "nothing reached the peer"
    );
}

/// A monitor that accepts and then says nothing must not strand the
/// client: the budget is the whole handshake's.
#[test]
fn a_silent_monitor_falls_back_within_the_budget() {
    let repo = repo_dir();
    let (_dir, served) = fake_monitor(repo.path(), None);

    let budget = Duration::from_millis(150);
    let began = std::time::Instant::now();
    let got = attach_within(repo.path(), agent("codex"), budget);
    let took = began.elapsed();

    match got {
        Attached::Alone(why) => assert!(why.contains("handshake"), "{why}"),
        other => panic!("expected Alone, got {other:?}"),
    }
    // The peer stays open for 1500ms. Falling back anywhere near
    // the budget proves the DEADLINE ended it; waiting for the
    // peer would land an order of magnitude later.
    assert!(
        took < Duration::from_millis(800),
        "fell back in {took:?}, which is the peer closing and not the budget"
    );
    served.join().unwrap();
}

/// The descriptor says what the monitor WAS; the process answering
/// now is what counts.
#[test]
fn a_welcome_on_another_proto_is_refused() {
    let repo = repo_dir();
    let skewed = format!(
        r#"{{"t":"ok","proto":{},"fold":{{"tick":1,"at_ms":2}}}}"#,
        PROTO + 1
    );
    let (_dir, served) = fake_monitor(repo.path(), Some(format!("{skewed}\n")));

    let got = attach(repo.path(), agent("codex"));
    served.join().unwrap();
    match got {
        Attached::Alone(why) => assert!(why.contains("answered on proto"), "{why}"),
        other => panic!("expected Alone, got {other:?}"),
    }
}

/// A connect that cannot finish inside the budget is a fallback,
/// not a strand. A full backlog is platform-specific, so the
/// connect is supplied instead of provoked.
#[test]
fn a_connect_that_cannot_finish_falls_back() {
    let repo = repo_dir();
    let (_dir, served) = fake_monitor(repo.path(), Some(welcome_line()));

    let budget = Duration::from_millis(150);
    let began = std::time::Instant::now();
    let got = attach_by(repo.path(), agent("codex"), budget, |_path, _deadline| {
        // Stands in for a connect against a full queue: waits
        // past any budget a test sets, then reports the
        // timeout. Deliberately does NOT call `remaining` — a
        // test must not depend on the function it covers for
        // its own termination, and an earlier version of this
        // spun forever under a mutation that made `remaining`
        // always return Some.
        std::thread::sleep(Duration::from_millis(400));
        Err(expired())
    });
    let took = began.elapsed();

    match got {
        Attached::Alone(why) => assert!(why.contains("cannot reach"), "{why}"),
        other => panic!("expected Alone, got {other:?}"),
    }
    assert!(took < Duration::from_millis(1500), "took {took:?}");
    drop(served);
}

/// A socket whose buffer is full, with a peer that never reads.
///
/// The portable way to make a write block and a `POLLOUT` stay
/// unready. A real backlog is platform-specific, which is why the
/// connect itself takes a seam while its WAIT is tested for real.
fn wedged_pair() -> (
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    use std::io::Write;
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    a.set_nonblocking(true).unwrap();
    let junk = vec![0u8; 64 * 1024];
    loop {
        match (&mut &a).write(&junk) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    // Left non-blocking, which is what `connect_within` hands back.
    // `b` is returned unread on purpose: dropping it would turn a
    // blocked write into EPIPE and prove nothing about the bound.
    (a, b)
}

/// Bound the call itself, not just its result.
///
/// If the deadline is ignored the call blocks for an hour, and an
/// assertion after it never runs — the suite hangs instead of
/// failing, which is how a real regression reaches CI unnoticed.
/// Running it on a thread turns that into a clean failure.
fn within<T: Send + 'static>(bound: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx.recv_timeout(bound)
        .expect("the deadline was not honoured: the call blocked instead")
}

#[test]
fn waiting_for_readiness_gives_up_at_the_deadline() {
    let kind = within(Duration::from_millis(900), || {
        let (full, _peer) = wedged_pair();
        wait_ready(
            full.as_fd(),
            PollFlags::OUT,
            std::time::Instant::now() + Duration::from_millis(120),
        )
        .map_err(|e| e.kind())
    })
    .expect_err("a full buffer is not writable");
    assert_eq!(kind, std::io::ErrorKind::TimedOut);
}

/// A frame too big for the peer's buffer has to wait for drain,
/// which a small frame to an attentive peer never exercises.
#[test]
fn a_write_that_cannot_drain_gives_up_at_the_deadline() {
    let kind = within(Duration::from_millis(900), || {
        let (full, _peer) = wedged_pair();
        write_frame(
            &full,
            &vec![b'x'; 256 * 1024],
            std::time::Instant::now() + Duration::from_millis(120),
        )
        .map_err(|e| e.kind())
    })
    .expect_err("a full buffer cannot take it");
    assert_eq!(kind, std::io::ErrorKind::TimedOut);
}

/// The clean half of `SO_ERROR`: a healthy socket reports nothing.
/// Its error arm needs a full backlog and is documented as
/// unexercised rather than covered.
#[test]
fn a_healthy_socket_reports_no_pending_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4242).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let stream = std::os::unix::net::UnixStream::connect(&path).unwrap();

    assert_eq!(socket_error(&stream).expect("getsockopt works"), None);
}

/// A raw socket gets none of std's setup, so the protections it
/// normally supplies are asserted on the real connect's result.
/// A test pair made through std already has them and would mask
/// the difference.
#[test]
fn a_connected_socket_carries_the_protections_std_would_have_given_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4242).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

    let stream = connect_within(&path, std::time::Instant::now() + Duration::from_secs(2))
        .expect("connects");

    let flags = rustix::io::fcntl_getfd(&stream).expect("F_GETFD");
    assert!(
        flags.contains(rustix::io::FdFlags::CLOEXEC),
        "not close-on-exec: a spawned command source would inherit this connection"
    );

    #[cfg(target_vendor = "apple")]
    {
        let on = rustix::net::sockopt::socket_nosigpipe(&stream).expect("getsockopt");
        assert!(
            on,
            "SO_NOSIGPIPE unset: with main's SIG_DFL, a dead monitor would kill the client"
        );
    }
}
