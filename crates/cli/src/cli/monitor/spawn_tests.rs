use crate::cli::monitor::fixtures::*;

/// A monitor listening but NOT yet announced, so a test can make
/// it appear only once the client has asked for one to be started.
fn unannounced_monitor(repo: &std::path::Path) -> (tempfile::TempDir, Descriptor) {
    use std::io::{Read, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4343).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn(move || {
        // Answered streams are KEPT: dropping one closes the
        // connection the client is about to read its welcome from.
        let mut answered = Vec::new();
        while let Ok((mut stream, _)) = listener.accept() {
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {}
            if stream.write_all(welcome_line().as_bytes()).is_err() {
                return;
            }
            answered.push(stream);
        }
    });
    let d = Descriptor {
        proto: PROTO,
        pid: 4343,
        repo: repo.to_string_lossy().to_string(),
        socket: path.to_string_lossy().to_string(),
        build: None,
    };
    (dir, d)
}

/// A spawn that must never be reached.
fn forbidden(_: &std::path::Path) -> std::io::Result<Box<dyn Started>> {
    Err(std::io::Error::other("nothing should have been started"))
}

/// A started monitor that exits only when the test says so, and
/// reports having been reaped.
///
/// Models the case the retry loop could not cover: the monitor is
/// still alive when the attach returns, so whatever reaps it has
/// to outlive that return.
struct ExitsLater {
    exit: std::sync::mpsc::Receiver<()>,
    reaped: std::sync::mpsc::Sender<()>,
}

impl Started for ExitsLater {
    fn reap(self: Box<Self>) {
        std::thread::spawn(move || {
            let _ = self.exit.recv();
            let _ = self.reaped.send(());
        });
    }
}

/// Nothing to reap, for the tests whose subject is elsewhere.
struct NotStarted;

impl Started for NotStarted {
    fn reap(self: Box<Self>) {}
}

/// No monitor running: the client starts one and attaches to it.
///
/// Connect-or-spawn's whole point — without it nothing ever starts
/// a monitor, so every wait watches for itself and the plan buys
/// nothing.
#[tokio::test]
async fn a_client_with_no_monitor_starts_one_and_attaches() {
    let repo = repo_dir();
    let (_dir, d) = unannounced_monitor(repo.path());
    let starts = std::sync::atomic::AtomicUsize::new(0);

    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        ATTACH_BUDGET,
        Duration::from_millis(200),
        SHARE_ATTEMPTS,
        |r| {
            starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            publish(r, &d).map(|()| Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;

    assert!(matches!(got, Attached::Shared(_)), "{got:?}");
    assert_eq!(
        starts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one start is enough when it works: the retry attaches rather than \
             starting again"
    );
}

/// Where in the handshake a dying monitor closes the client's
/// connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HangUp {
    /// Before accepting it: the listener closes with the connection
    /// still in its backlog.
    Unaccepted,
    /// Before the client has written its hello.
    BeforeHello,
    /// After it has read the hello, before answering.
    AfterHello,
}

/// A published monitor that closes the client's first connection at
/// `when`, then stops listening — what a client reconnecting to a
/// monitor that was just killed meets — and a replacement the
/// client's start publishes, already listening.
///
/// Both ends of the race are signalled, not timed. The client's
/// first connect returns only after the hang-up when `when` puts it
/// before the hello, and every later knock at the dying address
/// waits for the listener to be gone, so where the hang-up lands is
/// `when` and nothing else. The replacement is bound before its
/// descriptor is published, so it answers whenever it is found.
async fn replaced_after(when: HangUp) -> (Attached, usize) {
    let repo = repo_dir();
    let dir = tempfile::tempdir().unwrap();
    let dying = socket_path(dir.path(), 4747).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&dying).unwrap();
    publish(
        repo.path(),
        &Descriptor {
            proto: PROTO,
            pid: 4747,
            repo: repo.path().to_string_lossy().to_string(),
            socket: dying.to_string_lossy().to_string(),
            build: None,
        },
    )
    .unwrap();
    let (hung_up, gone) = std::sync::mpsc::channel::<()>();
    let (knocked, knock) = std::sync::mpsc::channel::<()>();
    let closer = std::thread::spawn(move || {
        use std::io::Read;
        if when == HangUp::Unaccepted {
            knock.recv().unwrap();
        } else {
            let (mut stream, _) = listener.accept().unwrap();
            if when == HangUp::AfterHello {
                let mut byte = [0u8; 1];
                while stream.read(&mut byte).unwrap() == 1 && byte[0] != b'\n' {}
            }
            drop(stream);
        }
        drop(listener);
        hung_up.send(()).unwrap();
    });
    let gone = std::sync::Mutex::new(Some(gone));
    let wait_gone = || {
        if let Some(gone) = gone.lock().unwrap().take() {
            gone.recv().unwrap();
        }
    };
    let knocks = std::sync::atomic::AtomicUsize::new(0);
    let (_live, d) = unannounced_monitor(repo.path());
    let starts = std::sync::atomic::AtomicUsize::new(0);

    let (dying, wait_gone, knocks, knocked) = (&dying, &wait_gone, &knocks, &knocked);
    let connect = move |path: &std::path::Path, deadline| {
        if path != dying {
            return connect_within(path, deadline);
        }
        if knocks.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
            wait_gone();
            return connect_within(path, deadline);
        }
        let stream = connect_within(path, deadline)?;
        match when {
            HangUp::Unaccepted => {
                knocked.send(()).unwrap();
                wait_gone();
            }
            HangUp::BeforeHello => wait_gone(),
            HangUp::AfterHello => {}
        }
        Ok(stream)
    };
    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        ATTACH_BUDGET,
        Duration::from_millis(200),
        SHARE_ATTEMPTS,
        |r| {
            starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            publish(r, &d).map(|()| Box::new(NotStarted) as Box<dyn Started>)
        },
        connect,
        |r| acquire_lease(r).is_ok(),
    )
    .await;
    closer.join().unwrap();
    (got, starts.load(std::sync::atomic::Ordering::SeqCst))
}

/// A monitor that hangs up during the handshake is repaired, not
/// gone around, wherever in the handshake the hang-up lands.
#[tokio::test]
async fn a_monitor_that_hangs_up_mid_handshake_is_replaced() {
    for when in [HangUp::Unaccepted, HangUp::BeforeHello, HangUp::AfterHello] {
        let (got, starts) = replaced_after(when).await;
        assert!(matches!(got, Attached::Shared(_)), "{when:?}: {got:?}");
        assert_eq!(starts, 1, "{when:?}");
    }
}

/// A hang-up is told apart from a peer that never answers: only the
/// first says the monitor is going.
#[test]
fn a_hang_up_is_not_a_silence() {
    let repo = repo_dir();
    let deadline = || std::time::Instant::now() + Duration::from_millis(150);
    let paired = |hang_up: bool| {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        let theirs = (!hang_up).then_some(theirs);
        move |_: &std::path::Path, _: std::time::Instant| {
            let _ = &theirs;
            ours.try_clone()
        }
    };
    assert!(matches!(
        greet_at(
            "pair",
            repo.path(),
            agent("codex"),
            deadline(),
            paired(true)
        ),
        Seen::HungUp(_)
    ));
    assert!(matches!(
        greet_at(
            "pair",
            repo.path(),
            agent("codex"),
            deadline(),
            paired(false)
        ),
        Seen::Unproven(_)
    ));
}

/// A hello that meets `ENOTCONN` met a monitor going away.
///
/// Darwin answers a write racing the peer's close that way, a window
/// no sequencing of the two ends can land in, so an unconnected
/// socket stands in for it: it gives the same answer every time.
#[test]
fn a_hello_that_finds_the_connection_gone_is_a_hang_up() {
    use rustix::net::{AddressFamily, SocketType};
    let repo = repo_dir();
    let unconnected = |_: &std::path::Path, _: std::time::Instant| {
        let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)?;
        #[cfg(target_vendor = "apple")]
        set_nosigpipe(&fd)?;
        set_nonblocking(&fd, true)?;
        Ok(std::os::unix::net::UnixStream::from(fd))
    };
    let seen = greet_at(
        "unconnected",
        repo.path(),
        agent("codex"),
        std::time::Instant::now() + Duration::from_millis(150),
        unconnected,
    );
    assert!(matches!(seen, Seen::HungUp(_)), "{seen:?}");
}

/// The runtime namespace is derived, not created, by a LOOKUP.
///
/// Discovery is read-only: it must be able to ask where a
/// descriptor would be without making a directory to find out, and
/// without write access (codex on 2a0c59a).
#[test]
fn a_lookup_creates_nothing() {
    let repo = repo_dir();
    let at = runtime_path(repo.path(), "json");
    assert!(at.is_absolute());
    assert!(!at.starts_with(repo.path()), "and never inside the repo");

    // Against a namespace that does NOT exist, because the real
    // one does: other tests created it, so asking there proves
    // nothing either way.
    let fresh = tempfile::tempdir().unwrap();
    let absent = fresh.path().join("not-made-yet");
    let at = runtime_path_in(&absent, repo.path(), "json");
    assert!(
        !absent.exists(),
        "deriving a path must not create the directory it is in"
    );
    assert!(at.starts_with(&absent));
    // And a read of a missing descriptor is an absence, not a
    // reason to make anything.
    assert_eq!(published(repo.path()), Published::Nothing);
}

/// A descriptor from a namespace that is not ours never becomes an
/// address.
///
/// The read half of the trust boundary, and the half this plan
/// nearly shipped without. The descriptor used to live in a repo
/// we owned; it now lives at a PREDICTABLE path under
/// world-writable `/tmp`, so another uid can get there first and
/// leave one naming the right repo and proto with `socket`
/// pointing at theirs. Matching fields are not evidence of who
/// wrote them, and refusing to PUBLISH into a planted namespace
/// does nothing about consuming what is already in it (codex on
/// 9dfc3e3).
///
/// Driven through `look_at_in` with a connect SPY, because the vet
/// returning the right answer proves nothing about whether
/// discovery asks it: the bug is a client that connects.
#[test]
fn a_planted_descriptor_never_reaches_connect() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    let repo = repo_dir();
    let me = rustix::process::getuid().as_raw();
    let d = describe(repo.path(), 4242, std::path::Path::new("/tmp/planted.sock"));
    let home = tempfile::tempdir().unwrap();

    let tries = AtomicUsize::new(0);
    let spy = |_: &std::path::Path, _: std::time::Instant| {
        tries.fetch_add(1, SeqCst);
        Err::<std::os::unix::net::UnixStream, _>(std::io::Error::other("the spy never dials"))
    };
    let budget = Duration::from_millis(50);
    let plant = |dir: &std::path::Path, mode: u32| {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            runtime_path_in(dir, repo.path(), "json"),
            serde_json::to_vec(&d).unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    };

    // Writable by anyone, so anyone could have left that descriptor.
    let open = home.path().join("open");
    plant(&open, 0o777);
    assert!(
        matches!(
            published_in(&open, repo.path(), me),
            Published::Untrusted(_)
        ),
        "a namespace others can write is not evidence"
    );
    assert!(matches!(
        look_at_in(&open, repo.path(), agent("codex"), budget, spy),
        Seen::Unproven(_)
    ));

    // A symlink INTO a directory we own, which reads back as ours
    // unless the final component is left unfollowed — and whose
    // owner may replace it later even under a sticky `/tmp`.
    let real = home.path().join("real");
    plant(&real, 0o700);
    let link = home.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(
        matches!(
            published_in(&link, repo.path(), me),
            Published::Untrusted(_)
        ),
        "an indirection is refused on the strength of the link, not its target"
    );
    assert!(matches!(
        look_at_in(&link, repo.path(), agent("codex"), budget, spy),
        Seen::Unproven(_)
    ));

    // Missing is an ordinary absence, and reading makes nothing.
    let absent = home.path().join("absent");
    assert_eq!(published_in(&absent, repo.path(), me), Published::Nothing);
    assert!(!absent.exists(), "a read must not create the namespace");
    assert!(matches!(
        look_at_in(&absent, repo.path(), agent("codex"), budget, spy),
        Seen::Vacant(_)
    ));

    assert_eq!(
        tries.load(SeqCst),
        0,
        "no descriptor from an untrusted or absent namespace may be dialled"
    );

    // And one we DO own is still used, so the refusals above are
    // the boundary working rather than discovery being broken.
    assert!(matches!(
        published_in(&real, repo.path(), me),
        Published::Ours(_)
    ));
    let _ = look_at_in(&real, repo.path(), agent("codex"), budget, spy);
    assert!(
        tries.load(SeqCst) > 0,
        "a trusted descriptor is an address, and gets knocked on"
    );
}

/// EVERY mutation boundary in the runtime namespace vets it.
///
/// One rule, driven at all three call sites rather than at the
/// helper, because a boundary that skips the vet is the bug and the
/// helper cannot tell you whether anyone called it.
///
/// It matters more here than it did in the repo. `/tmp` is
/// world-writable, so a directory another uid plants there is a
/// directory we would otherwise write into — and the descriptor is
/// the DISCOVERY record, so theirs would name their socket and
/// every client would connect to it. 0o777 is also what a 002
/// umask leaves behind, so the same fixture is both the attack and
/// the accident: an unvetted `create_dir_all` could make the
/// namespace at a mode every vetted caller then refuses forever.
#[test]
fn every_mutation_boundary_refuses_a_namespace_it_cannot_vet() {
    let repo = repo_dir();
    let me = rustix::process::getuid().as_raw();
    let d = describe(repo.path(), 4242, std::path::Path::new("/tmp/x.sock"));

    let home = tempfile::tempdir().unwrap();
    let open = home.path().join("open");
    std::fs::create_dir(&open).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        !dir_is_acceptable(me, 0o777, me),
        "the fixture plants a mode the vet rejects"
    );

    let refused = publish_in(repo.path(), &open, me, &d).expect_err("no descriptor");
    assert!(
        format!("{refused}").contains("mode"),
        "and it says what was wrong: {refused}"
    );
    let refused = acquire_lease_in(repo.path(), &open, me).expect_err("no lease");
    assert!(
        matches!(&refused, LeaseError::Unverifiable(why) if why.contains("mode")),
        "an unreadable namespace is never owned: {refused:?}"
    );
    let refused = take_repair_permit_in(repo.path(), &open, me);
    assert!(
        matches!(&refused, Repair::Unavailable(why) if why.contains("mode")),
        "{refused:?}"
    );

    assert!(
        std::fs::read_dir(&open).unwrap().next().is_none(),
        "and not one of them left a file there: no client can be pointed at a \
             planted socket, and no lease was taken beside somebody else's"
    );

    // A namespace they accept gets everything, so the refusals
    // above are the vet working and not these three being broken.
    let mine = home.path().join("mine");
    publish_in(repo.path(), &mine, me, &d).expect("its own 0o700 namespace is fine");
    let _lease = acquire_lease_in(repo.path(), &mine, me).expect("and the lease");
    assert!(matches!(
        take_repair_permit_in(repo.path(), &mine, me),
        Repair::Acquired(_)
    ));
    assert!(runtime_path_in(&mine, repo.path(), "json").exists());
}

/// The supported upgrade, and the rollback's HONEST limit.
///
/// This is a cutover: after it, a binary predating it is not
/// supported running concurrently, and two monitors could own one
/// repo in that configuration. The plan does not claim otherwise,
/// so the test does not either — it records what rollback actually
/// does rather than leaving the limitation to a sentence somebody
/// may not read (codex on c0ebe23).
#[test]
fn the_cutover_is_exclusive_among_supported_binaries_and_not_across_it() {
    let repo = repo_dir();
    ensure_runtime_dir().unwrap();

    // Supported: two of THIS version cannot both own a repo.
    let held = acquire_lease(repo.path()).expect("the first owns it");
    assert!(
        acquire_lease(repo.path()).is_err(),
        "one monitor per repo, which is the whole authority"
    );
    drop(held);
    assert!(
        acquire_lease(repo.path()).is_ok(),
        "and it frees when the owner goes"
    );

    // Across the cutover: a pre-cutover binary locked
    // `.clank/monitor.lock`, which this one never touches. Both
    // can hold their own. Asserted so the limitation is a recorded
    // fact rather than a claim nobody checked.
    let legacy = repo.path().join(".clank").join("monitor.lock");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let old = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&legacy)
        .unwrap();
    assert!(
        clank_user_config::lease::take(&old).unwrap(),
        "a pre-cutover binary takes the old path"
    );
    assert!(
        acquire_lease(repo.path()).is_ok(),
        "and this one takes the new one regardless — which is why a pre-cutover \
             binary must be retired at install rather than left running"
    );
}

/// A namespace that cannot be trusted is UNAVAILABLE, not busy.
///
/// The vetting is part of this lock's correctness rather than a
/// security aside: a gate file replaced while a holder keeps its
/// old fd lets the next caller lock a different inode at the same
/// name, and then both are repairing. And "unusable" is not
/// "somebody else has it" — reporting one as the other is a guess
/// stated as a fact.
#[test]
fn an_untrusted_namespace_is_unavailable_rather_than_busy() {
    let repo = repo_dir();
    let me = rustix::process::getuid().as_raw();

    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    // World-writable: anyone could replace the gate beside us.
    std::fs::set_permissions(&shared, std::os::unix::fs::PermissionsExt::from_mode(0o777)).unwrap();

    match take_repair_permit_in(repo.path(), &shared, me) {
        Repair::Unavailable(why) => assert!(!why.is_empty(), "it says what was wrong"),
        other => panic!("a directory anyone can write is not a gate: {other:?}"),
    }
}

/// An older peer's reply must be READABLE before it means
/// anything.
///
/// A bare `{"proto":4}` and an unknown body are not evidence about
/// retirement, and the first version read every older envelope as
/// "standing down" — then started a monitor on the strength of it.
/// Even a well-formed decline only establishes an older peer:
/// proto 3 and 4 cannot self-exit, and the wire does not
/// distinguish refusal from retirement progress.
#[test]
fn an_unreadable_older_reply_establishes_nothing() {
    let repo = repo_dir();
    let older = PROTO - 1;

    for answer in [
        format!("{{\"proto\":{older}}}\n"),
        format!("{{\"proto\":{older},\"t\":\"something_later\"}}\n"),
    ] {
        let (_dir, _served) = fake_monitor(repo.path(), Some(answer.clone()));
        match look_at(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within) {
            Seen::Unproven(why) => assert!(why.contains("cannot read"), "{answer}: {why}"),
            other => panic!("{answer} establishes nothing: {other:?}"),
        }
    }

    // A decline we CAN read is an older peer — named for what it
    // is, not for a retirement nobody confirmed.
    let decline = format!(
        "{}\n",
        serde_json::to_string(&Welcome::Decline {
            proto: older,
            why: "older".into(),
        })
        .unwrap()
    );
    let (_dir, _served) = fake_monitor(repo.path(), Some(decline));
    assert!(matches!(
        look_at(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within),
        Seen::OlderPeer { .. }
    ));
}

/// A proto-16 monitor is retired, never served. Its `unblocked` work
/// carries no answer identity, which this build can neither decode nor
/// receive on delivery, so the handshake must turn it away before any
/// of its frames is read and treat it as an older peer to replace.
#[test]
fn a_proto_16_monitor_is_an_older_peer_to_replace() {
    const OLD: u32 = 16;
    assert!(!admits(PROTO, OLD));
    let old_items = r#"{"t":"items","items":[{"kind":"unblocked","name":"q","answer":"yes"}]}"#;
    assert!(
        serde_json::from_str::<Note>(old_items).is_err(),
        "a 16 monitor's answer is not one this build could deliver"
    );

    let repo = repo_dir();
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4616).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    publish(
        repo.path(),
        &Descriptor {
            proto: OLD,
            pid: 4616,
            repo: repo.path().to_string_lossy().to_string(),
            socket: path.to_string_lossy().to_string(),
            build: None,
        },
    )
    .unwrap();
    let served = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Write};
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let open: Opening = serde_json::from_str(line.trim_end()).unwrap();
        let decline = Welcome::Decline {
            proto: OLD,
            why: obsolete_why(OLD, open.proto),
        };
        writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&decline).unwrap()
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        open.proto
    });
    match look_at(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within) {
        Seen::OlderPeer { obsolete, .. } => {
            assert!(obsolete, "it agreed it is obsolete, so it is replaced")
        }
        other => panic!("a proto-16 monitor must not be shared: {other:?}"),
    }
    assert_eq!(
        served.join().unwrap(),
        PROTO,
        "greeted with this build's version"
    );
}

/// ONE repair per repo: a second client does not also start one.
///
/// The canonical lease stops two monitors OWNING a repo and says
/// nothing about how many clients may poll for it or fork a child
/// at it — each could take and drop it and each then start one,
/// which is a poller and a fork per waiting client (codex on
/// cf29219). The permit is flocked so it coalesces ACROSS
/// processes, every parked wait being its own.
#[tokio::test]
async fn only_one_client_repairs_a_repo_at_a_time() {
    let repo = repo_dir();
    let dir = tempfile::tempdir().unwrap();

    let mine = repair_permit(repo.path(), dir.path())
        .expect("the gate is usable")
        .expect("nobody else is on it");
    assert!(
        repair_permit(repo.path(), dir.path()).unwrap().is_none(),
        "a second client must not become a second repairer"
    );

    // Another repo is another repair, which is the point of the
    // tag: one in flight must not block an unrelated one.
    let other = repo_dir();
    assert!(repair_permit(other.path(), dir.path()).unwrap().is_some());

    drop(mine);
    assert!(
        repair_permit(repo.path(), dir.path()).unwrap().is_some(),
        "and it frees when the repairer is done"
    );
}

/// The permit is HELD while the startup it covers is happening.
///
/// Releasing it when the lease is taken rather than when the work
/// is done lets the next client in while this one is still
/// starting a monitor — which is the fork-per-client this exists
/// to prevent, arriving a few milliseconds later. Asked from
/// INSIDE the spawn, which is the only moment that distinguishes
/// the two.
#[tokio::test]
async fn the_permit_is_still_held_while_the_startup_runs() {
    let repo = repo_dir();
    let free_during_spawn = std::sync::atomic::AtomicBool::new(false);

    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        Duration::from_millis(40),
        Duration::from_millis(40),
        SHARE_ATTEMPTS,
        |r| {
            if matches!(take_repair_permit(r), Repair::Acquired(_)) {
                free_during_spawn.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;

    assert!(matches!(got, Attached::Alone(_)), "{got:?}");
    assert!(
        !free_during_spawn.load(std::sync::atomic::Ordering::SeqCst),
        "another client could have become a second repairer mid-startup"
    );
}

/// A deadline already gone is never readiness.
///
/// The wait took the lease BEFORE checking the time, so an
/// expired deadline with a free lease returned Ready and the
/// caller started a child with no further check — as could a timer
/// that resumed after the deadline with the lease freed meanwhile
/// (codex on cf29219).
#[tokio::test]
async fn an_expired_deadline_is_never_ready_however_free_the_lease_is() {
    let repo = repo_dir();
    // Nothing holds it: the ONLY thing that can say Busy here is
    // the expiry, which is the point.
    assert!(acquire_lease(repo.path()).is_ok());

    let gone = std::time::Instant::now() - Duration::from_millis(1);
    assert!(
        matches!(
            wait_for_ownership(repo.path(), gone, Duration::from_millis(5)).await,
            Ownership::Busy
        ),
        "a free lease past the deadline is still not permission to start"
    );
}

/// Waiting for ownership is bounded by the DEADLINE, with no
/// count in it.
///
/// A retry count ended the wait after about 450ms with production
/// settings, while a holder standing down legitimately has its
/// retirement budget plus settlement. Here the lease is released
/// well past any small number of steps, and the wait must still
/// see it.
#[tokio::test]
async fn waiting_for_ownership_is_bounded_by_time_not_by_tries() {
    let repo = repo_dir();
    let held = acquire_lease(repo.path()).expect("the test owns it first");
    let releasing = tokio::task::spawn_blocking(move || {
        std::thread::sleep(Duration::from_millis(400));
        drop(held);
    });

    // Twenty-odd steps before the release: a wait that gave up on
    // a handful of tries would never reach it.
    let by = std::time::Instant::now() + Duration::from_secs(3);
    let got = wait_for_ownership(repo.path(), by, Duration::from_millis(15)).await;
    assert!(matches!(got, Ownership::Ready), "{got:?}");
    releasing.await.unwrap();

    // And a lease that never frees is Busy rather than a wait
    // without end.
    let still = acquire_lease(repo.path()).unwrap();
    let began = std::time::Instant::now();
    let by = std::time::Instant::now() + Duration::from_millis(80);
    assert!(matches!(
        wait_for_ownership(repo.path(), by, Duration::from_millis(15)).await,
        Ownership::Busy
    ));
    assert!(began.elapsed() < Duration::from_secs(1));
    drop(still);
}

/// The whole handoff fits ONE deadline, including its last
/// handshake.
///
/// Each attach used to take a fresh budget, so an attempt entering
/// its handshake just before the deadline could block for another
/// whole `ATTACH_BUDGET` — and an outer check on the deadline
/// could not make that bounded (codex on 1c0ce9d). The connect
/// here outlasts any budget a test sets, so a fresh one is
/// measurable.
#[tokio::test]
async fn the_handoff_fits_one_deadline_including_its_last_handshake() {
    let repo = repo_dir();
    let (_dir, d) = unannounced_monitor(repo.path());

    let began = std::time::Instant::now();
    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        // A big per-attach budget against a small overall one, so
        // the two are far apart.
        Duration::from_millis(400),
        Duration::from_millis(60),
        SHARE_ATTEMPTS,
        |r| publish(r, &d).map(|()| Box::new(NotStarted) as Box<dyn Started>),
        |_path, deadline: std::time::Instant| {
            // Stands in for a connect against a full backlog: it
            // uses the whole deadline it was GIVEN and then
            // reports the timeout. That is what makes a fresh
            // budget measurable — a connect that slept a fixed
            // time would take the same either way, which is how
            // the first version of this fixture missed it.
            std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the connect did not finish",
            ))
        },
        |r| acquire_lease(r).is_ok(),
    )
    .await;
    let took = began.elapsed();

    assert!(matches!(got, Attached::Alone(_)), "{got:?}");
    assert!(
        took < Duration::from_millis(650),
        "the retries share what is LEFT of one deadline; a fresh 400ms budget each \
             would run far past it: {took:?}"
    );
}

/// Ownership that frees and then the deadline passing starts
/// NOTHING — through the production path, not just the probe.
///
/// The probe is a seam because taking the real lease is too fast
/// to arrange that gap by scheduling, and a guard nothing can
/// drive is a guard nobody has checked.
#[tokio::test]
async fn readiness_arriving_past_the_deadline_starts_no_monitor() {
    let repo = repo_dir();
    let started = std::sync::atomic::AtomicUsize::new(0);

    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        Duration::from_millis(20),
        Duration::from_millis(20),
        SHARE_ATTEMPTS,
        |_r| {
            started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        // Succeeds, but only after the deadline has gone.
        |_r| {
            std::thread::sleep(Duration::from_millis(120));
            true
        },
    )
    .await;

    match got {
        // Under load the 40 ms deadline can lapse during the lease
        // probe as well as after it; either way nothing may start.
        Attached::Alone(why) => assert!(
            why.contains("ran out of time") || why.contains("its lease is still held"),
            "{why}"
        ),
        other => panic!("nothing was started, so nothing can be shared: {other:?}"),
    }
    assert_eq!(
        started.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "ownership confirmed after the deadline is not permission to start"
    );
}

/// Ownership that frees and then the deadline passing starts
/// NOTHING.
///
/// Checking expiry before the probe is necessary and does not
/// cover a probe that FINISHED after the deadline, or this task
/// resuming past it before startup (codex on 06021ee). The probe
/// is a seam because taking the real lease is too fast to arrange
/// that gap by scheduling, and a guard nothing can drive is a
/// guard nobody has checked.
#[tokio::test]
async fn readiness_that_arrives_too_late_is_not_permission() {
    let repo = repo_dir();
    let by = std::time::Instant::now() + Duration::from_millis(40);

    // Succeeds — but only after the deadline has gone.
    let got = wait_for_ownership_by(
        repo.path(),
        by,
        Duration::from_millis(5),
        |_| {
            std::thread::sleep(Duration::from_millis(60));
            true
        },
        |_| None,
    )
    .await;
    assert!(
        matches!(got, Ownership::Ready),
        "the probe did take it, which is what makes the guard after it necessary"
    );
    assert!(
        by.saturating_duration_since(std::time::Instant::now())
            .is_zero(),
        "and the deadline is gone by the time it says so"
    );
}

/// The whole handoff, end to end: an older holder goes and this
/// client ends up SHARING a current one.
///
/// The acceptance demonstrated here, in order: the old holder is
/// NOTIFIED; it keeps the canonical lease well beyond the obsolete
/// count-based give-up for the parameters actually supplied; it
/// releases; exactly one useful replacement starts; the client
/// gets `Shared`.
///
/// Both halves of that mattered and the first version had neither.
/// It released at 250ms while passing `settle = 2000` — so the
/// old four-step wait would have had 500ms steps and reached it
/// on the first one, meaning a restored retry count would still
/// have passed (codex on 685e022). And it started its countdown
/// from the test's own clock, so a slow start could find the
/// holder already gone before any notification: the interval is
/// tied to an observed GREETING now.
#[tokio::test]
async fn an_older_holder_goes_and_the_client_shares_its_replacement() {
    use std::io::{BufRead, BufReader, Write};

    // Ordered, not timed. This used to be budget 800ms + settle
    // 400ms against a release at 600ms, leaving ~600ms for the
    // client to take the lease, bind, publish and handshake — and
    // it duly failed once in four full-suite runs, on the run that
    // coincided with two heavy concurrent builds. A margin chosen
    // to survive contention asserts this machine's arithmetic, not
    // the rule, and widening it would have hidden a real slowdown
    // (codex on fbb84e3).
    //
    // The holder now stands down because the CLIENT asked one
    // time more than the old retry count would have, so the thing
    // under test is the ordering and the deadline is only a
    // backstop against a wedge.
    let budget = Duration::from_secs(5);
    let settle = Duration::from_millis(40);
    let stand_down_on = SHARE_ATTEMPTS as usize + 1;

    let repo = repo_dir();
    let dir = tempfile::tempdir().unwrap();
    let old_sock = socket_path(dir.path(), 4801).unwrap();
    let old = std::os::unix::net::UnixListener::bind(&old_sock).unwrap();
    publish(
        repo.path(),
        &Descriptor {
            proto: PROTO - 1,
            pid: 4801,
            repo: repo.path().to_string_lossy().to_string(),
            socket: old_sock.to_string_lossy().to_string(),
            build: None,
        },
    )
    .unwrap();
    // The old monitor OWNS the repo, as a real one does.
    let lease = acquire_lease(repo.path()).expect("the old holder owns it");

    let greeted_at = std::sync::Arc::new(std::sync::Mutex::new(None));
    let noticed = greeted_at.clone();
    // Raised by the client's own ownership probe, below.
    let retire_now = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let asked = retire_now.clone();
    let serving = std::thread::spawn(move || {
        // It keeps answering until it goes: a client greets it
        // more than once — on discovery, and again after taking
        // the repair gate — and a holder that answered only the
        // first would look like one that died mid-frame.
        old.set_nonblocking(true).unwrap();
        let decline = serde_json::to_string(&Welcome::Decline {
            proto: PROTO - 1,
            why: "older".into(),
        })
        .unwrap();
        loop {
            match old.accept() {
                Ok((stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line);
                    // A valid greeting from a newer client is what
                    // starts the clock: the retirement interval is
                    // measured from NOTIFICATION, not from the
                    // test's own start.
                    if serde_json::from_str::<Opening>(line.trim_end())
                        .is_ok_and(|o| o.proto > PROTO - 1)
                    {
                        let mut at = noticed.lock().unwrap();
                        at.get_or_insert_with(std::time::Instant::now);
                    }
                    let _ = writeln!(reader.get_mut(), "{decline}");
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
            // It goes when the client has asked for ownership
            // once more than the old count would have allowed —
            // and only after it has actually been greeted, so the
            // retirement is still something this monitor LEARNED
            // rather than something the test did to it.
            let greeted = noticed.lock().unwrap().is_some();
            if greeted && asked.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
        }
        // Stands down: the lease is what says it has gone.
        drop(lease);
        drop(old);
    });

    // The replacement: takes ownership and serves this version.
    let new_dir = tempfile::tempdir().unwrap();
    let new_sock = socket_path(new_dir.path(), 4802).unwrap();
    let started = std::sync::atomic::AtomicUsize::new(0);
    let probes = std::sync::atomic::AtomicUsize::new(0);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let replacement = std::sync::Arc::new(std::sync::Mutex::new(None));
    let keep = replacement.clone();
    let halt = stop.clone();
    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        budget,
        settle,
        SHARE_ATTEMPTS,
        |r| {
            started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let held = acquire_lease(r).map_err(std::io::Error::other)?;
            let listener = std::os::unix::net::UnixListener::bind(&new_sock)?;
            listener.set_nonblocking(true)?;
            publish(
                r,
                &Descriptor {
                    proto: PROTO,
                    pid: 4802,
                    repo: r.to_string_lossy().to_string(),
                    socket: new_sock.to_string_lossy().to_string(),
                    build: None,
                },
            )
            .map_err(std::io::Error::other)?;
            let halt = halt.clone();
            let handle = std::thread::spawn(move || {
                let _own = held;
                while !halt.load(std::sync::atomic::Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let mut reader = BufReader::new(stream);
                            let mut line = String::new();
                            let _ = reader.read_line(&mut line);
                            let ok = serde_json::to_string(&Welcome::Ok { proto: PROTO }).unwrap();
                            let _ = writeln!(reader.get_mut(), "{ok}");
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
            *keep.lock().unwrap() = Some(handle);
            Ok(Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        |r| {
            // The ASK is the signal: one probe past the count the
            // old implementation stopped at.
            if probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == stand_down_on {
                retire_now.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            acquire_lease(r).is_ok()
        },
    )
    .await;

    assert!(
        matches!(got, Attached::Shared(_)),
        "the client must end up sharing the replacement: {got:?}"
    );
    assert_eq!(
        started.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "and exactly one monitor was started to get there"
    );
    assert!(
        greeted_at.lock().unwrap().is_some(),
        "the holder retired because a newer client GREETED it, not because the \
             test reached in and dropped its lease"
    );
    let asks = probes.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        asks > SHARE_ATTEMPTS as usize,
        "and the client kept asking past the old retry count, which is the bound \
             the deadline replaced: {asks} ask(s)"
    );

    // Both fixture servers are owned and ended, so neither
    // outlives the test holding a lease or a socket.
    serving.join().unwrap();
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(h) = replacement.lock().unwrap().take() {
        h.join().unwrap();
    }
}

/// Publishes a welcoming monitor for `repo` after `delay`, as one
/// that is still reading its installation and binding does.
fn publishes_after(
    repo: std::path::PathBuf,
    delay: Duration,
) -> std::thread::JoinHandle<(tempfile::TempDir, std::thread::JoinHandle<Option<Hello>>)> {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        fake_monitor(&repo, Some(welcome_line()))
    })
}

/// Far more steps than `SHARE_ATTEMPTS` at the settle these tests
/// pass, so only a wait bounded by the deadline reaches it.
const LATE: Duration = Duration::from_millis(300);
const SETTLE: Duration = Duration::from_millis(40);

/// A lease held with nothing published yet is a monitor coming up,
/// and the client attaches to it once it publishes.
#[tokio::test]
async fn a_client_waiting_on_a_held_lease_attaches_when_its_holder_publishes() {
    let repo = repo_dir();
    let lease = acquire_lease(repo.path()).expect("the starting monitor owns it");
    let late = publishes_after(repo.path().to_path_buf(), LATE);

    let got = attach_or_spawn_by(
        repo.path(),
        agent("grok"),
        Duration::from_secs(5),
        SETTLE,
        SHARE_ATTEMPTS,
        forbidden,
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;

    assert!(matches!(got, Attached::Shared(_)), "{got:?}");
    let (_dir, served) = late.join().unwrap();
    served.join().unwrap();
    drop(lease);
}

/// A client finding the repair permit held attaches to the
/// repairer's monitor once it publishes, and starts nothing.
#[tokio::test]
async fn a_client_finding_the_permit_held_attaches_to_the_repairers_monitor() {
    let repo = repo_dir();
    let me = rustix::process::getuid().as_raw();
    let gate = match take_repair_permit_in(repo.path(), &runtime_dir(me), me) {
        Repair::Acquired(p) => p,
        other => panic!("the repairer takes it: {other:?}"),
    };
    let late = publishes_after(repo.path().to_path_buf(), LATE);

    let got = attach_or_spawn_by(
        repo.path(),
        agent("grok"),
        Duration::from_secs(5),
        SETTLE,
        SHARE_ATTEMPTS,
        forbidden,
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;

    assert!(matches!(got, Attached::Shared(_)), "{got:?}");
    let (_dir, served) = late.join().unwrap();
    served.join().unwrap();
    drop(gate);
}

/// A client that started a monitor waits for it to publish until
/// the deadline, not for a count of looks.
#[tokio::test]
async fn a_client_attaches_to_the_monitor_it_started_however_many_looks_it_takes() {
    let repo = repo_dir();
    let late = std::sync::Mutex::new(None);

    let got = attach_or_spawn_by(
        repo.path(),
        agent("grok"),
        Duration::from_secs(5),
        SETTLE,
        SHARE_ATTEMPTS,
        |r| {
            *late.lock().unwrap() = Some(publishes_after(r.to_path_buf(), LATE));
            Ok(Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;

    assert!(matches!(got, Attached::Shared(_)), "{got:?}");
    let (_dir, served) = late.lock().unwrap().take().unwrap().join().unwrap();
    served.join().unwrap();
}

/// A second contender does not become a second repairer.
///
/// It takes the documented local fallback while the first holds
/// the gate, and starts nothing — the whole point of coalescing.
#[tokio::test]
async fn a_second_contender_starts_nothing_while_the_first_repairs() {
    let repo = repo_dir();
    let me = rustix::process::getuid().as_raw();
    let gate = match take_repair_permit_in(repo.path(), &runtime_dir(me), me) {
        Repair::Acquired(p) => p,
        other => panic!("the first contender takes it: {other:?}"),
    };

    let started = std::sync::atomic::AtomicUsize::new(0);
    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        Duration::from_millis(50),
        Duration::from_millis(50),
        SHARE_ATTEMPTS,
        |_r| {
            started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;

    match got {
        Attached::Alone(why) => assert!(why.contains("already starting one"), "{why}"),
        other => panic!("it has nothing to share yet: {other:?}"),
    }
    assert_eq!(
        started.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a second repairer is the fork-per-client this gate exists to prevent"
    );
    drop(gate);
}

/// Ownership that never frees starts NOTHING.
///
/// This fixture used to hold the lease for the whole call and
/// REQUIRE a start, which is exactly the bug: all three ways out
/// of the wait — acquired, retries exhausted, deadline gone — fell
/// through to the same spawn, so a failed ownership confirmation
/// started a monitor that could not own anything (codex on
/// 1c0ce9d). The right answer for a lease that stays held is a
/// bounded local fallback.
#[tokio::test]
async fn ownership_that_never_frees_starts_nothing() {
    let repo = repo_dir();
    let held = acquire_lease(repo.path()).expect("the test owns it first");
    let starts = std::sync::atomic::AtomicUsize::new(0);

    let began = std::time::Instant::now();
    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        Duration::from_millis(50),
        Duration::from_millis(40),
        SHARE_ATTEMPTS,
        |_r| {
            starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;
    let took = began.elapsed();

    match got {
        Attached::Alone(why) => assert!(why.contains("lease is still held"), "{why}"),
        other => panic!("nothing can serve this repo yet: {other:?}"),
    }
    assert_eq!(
        starts.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a monitor started against a held lease loses it and exits, having \
             helped nobody"
    );
    assert!(
        took < Duration::from_secs(2),
        "and it gave up inside its own deadline: {took:?}"
    );
    drop(held);
}

/// Ownership that frees DURING the wait is taken.
///
/// The case the wait exists for: a holder standing down may keep
/// its lease well past the old retry COUNT, and the client must
/// still be the one that starts the replacement once it goes.
///
/// The release is ordered, not timed. It used to be a 250ms
/// `thread::sleep` racing a 650ms deadline, which asserted the
/// arithmetic of this machine rather than the rule: the lease
/// frees on the probe after the old count would have given up, so
/// a build that went back to count-bounding returns `Busy` before
/// the release can happen and this test fails for the right reason
/// (codex on fbb84e3).
#[tokio::test]
async fn ownership_that_frees_during_the_wait_starts_one() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    let repo = repo_dir();
    let held = std::sync::Mutex::new(Some(
        acquire_lease(repo.path()).expect("the test owns it first"),
    ));
    let starts = AtomicUsize::new(0);
    let probes = AtomicUsize::new(0);

    // One past the count, so "waited longer than the old budget"
    // is a fact about ORDER rather than about how loaded the
    // machine is.
    let release_on = SHARE_ATTEMPTS as usize + 1;

    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        // Generous and bounded: the deadline exists so a hung
        // handoff cannot wedge the suite, and it is deliberately
        // far from the decision this test makes.
        Duration::from_secs(5),
        Duration::from_millis(40),
        SHARE_ATTEMPTS,
        |_r| {
            starts.fetch_add(1, SeqCst);
            Ok(Box::new(NotStarted) as Box<dyn Started>)
        },
        connect_within,
        |r| {
            if probes.fetch_add(1, SeqCst) + 1 == release_on {
                drop(held.lock().unwrap().take());
            }
            acquire_lease(r).is_ok()
        },
    )
    .await;

    assert!(matches!(got, Attached::Alone(_)), "{got:?}");
    assert_eq!(
        starts.load(SeqCst),
        1,
        "the repo became free, so this client is the one that starts its monitor"
    );
    assert!(
        probes.load(SeqCst) > SHARE_ATTEMPTS as usize,
        "and it kept asking past the old retry count, which is the bound this \
             replaced: {} probe(s)",
        probes.load(SeqCst)
    );
}

/// A monitor that should KEEP the repo gets no replacement at all.
///
/// Newer than us, or refusing this client on its merits: either
/// way a second monitor would own nothing and refuse us the same
/// way. Starting one per park against a healthy newer owner was
/// the cost that made this classification necessary.
#[tokio::test]
async fn a_holder_that_keeps_the_repo_gets_no_replacement() {
    use std::io::{BufRead, BufReader, Write};

    for answer in [
        Welcome::Decline {
            proto: PROTO + 1,
            why: "we are newer".into(),
        },
        Welcome::Decline {
            proto: PROTO,
            why: "`codex` is not on this repo's roster".into(),
        },
    ] {
        let repo = repo_dir();
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_path(dir.path(), 4777).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        publish(
            repo.path(),
            &Descriptor {
                proto: PROTO,
                pid: 4777,
                repo: repo.path().to_string_lossy().to_string(),
                socket: socket.to_string_lossy().to_string(),
                build: None,
            },
        )
        .unwrap();
        let line = serde_json::to_string(&answer).unwrap();
        std::thread::spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut discard = String::new();
                let _ = reader.read_line(&mut discard);
                let _ = writeln!(reader.get_mut(), "{line}");
                std::thread::sleep(Duration::from_millis(30));
            }
        });

        let starts = std::sync::atomic::AtomicUsize::new(0);
        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            Duration::from_millis(200),
            Duration::from_millis(40),
            SHARE_ATTEMPTS,
            |_r| {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
            |r| acquire_lease(r).is_ok(),
        )
        .await;

        assert!(matches!(got, Attached::Alone(_)), "{got:?}");
        assert_eq!(
            starts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{answer:?} means the repo has an owner; starting one helps nobody"
        );
    }
}

/// A monitor that exits AFTER the attach returned is still reaped.
///
/// The case ownership inside the retry loop cannot cover: two
/// clients start at once, ours is still coming up when we attach
/// to the winner and return, and ours then loses the lease and
/// exits. Nothing in the loop is left to notice, so without
/// supervision that outlives the return it is a zombie for the
/// whole park — one per parked wait.
#[tokio::test]
async fn a_monitor_that_exits_after_the_attach_is_still_reaped() {
    let repo = repo_dir();
    let (_dir, d) = unannounced_monitor(repo.path());
    let (let_it_exit, exit) = std::sync::mpsc::channel();
    let (reaped, was_reaped) = std::sync::mpsc::channel();
    let held = std::sync::Mutex::new(Some(ExitsLater { exit, reaped }));

    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        ATTACH_BUDGET,
        Duration::from_millis(200),
        SHARE_ATTEMPTS,
        |r| {
            publish(r, &d)?;
            Ok(Box::new(held.lock().unwrap().take().expect("started once")) as Box<dyn Started>)
        },
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;
    assert!(matches!(got, Attached::Shared(_)), "{got:?}");

    // Still alive at this point, so nothing can have reaped it yet.
    assert_eq!(
        was_reaped.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty),
        "a live monitor must NOT be waited out: it has to outlive this client"
    );

    // Now it exits — long after the helper returned.
    drop(let_it_exit);
    was_reaped
        .recv_timeout(Duration::from_secs(2))
        .expect("a monitor that exits later is still reaped");
}

/// A monitor already running is attached to, not duplicated.
#[tokio::test]
async fn a_client_that_finds_a_monitor_starts_nothing() {
    let repo = repo_dir();
    let (_dir, d) = unannounced_monitor(repo.path());
    publish(repo.path(), &d).unwrap();

    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        ATTACH_BUDGET,
        Duration::from_millis(200),
        SHARE_ATTEMPTS,
        forbidden,
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;
    assert!(matches!(got, Attached::Shared(_)), "{got:?}");
}

/// A start that fails is not fatal: the wait watches for itself.
#[tokio::test]
async fn a_start_that_fails_leaves_the_client_alone() {
    let repo = repo_dir();
    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        ATTACH_BUDGET,
        Duration::from_millis(200),
        SHARE_ATTEMPTS,
        |_| Err(std::io::Error::other("fork failed")),
        connect_within,
        |r| acquire_lease(r).is_ok(),
    )
    .await;
    match got {
        Attached::Alone(why) => {
            assert!(why.contains("no monitor published"), "{why}");
            assert!(
                why.contains("fork failed"),
                "and why starting failed: {why}"
            );
        }
        other => panic!("expected Alone, got {other:?}"),
    }
}

/// A monitor that never comes up is given up on at the deadline.
///
/// Not after a count of looks: that ended the wait before a debug
/// monitor could publish, and the client keeps looking until the
/// deadline instead.
#[tokio::test]
async fn a_monitor_that_never_comes_up_is_given_up_on_at_the_deadline() {
    let repo = repo_dir();
    let dir = tempfile::tempdir().unwrap();
    // Published, but nothing is listening there: every attempt
    // reaches the connect and fails, so the attempts are counted.
    publish(
        repo.path(),
        &Descriptor {
            proto: PROTO,
            pid: 4344,
            repo: repo.path().to_string_lossy().to_string(),
            socket: socket_path(dir.path(), 4344)
                .unwrap()
                .to_string_lossy()
                .to_string(),
            build: None,
        },
    )
    .unwrap();
    let tries = std::sync::atomic::AtomicUsize::new(0);
    let (budget, settle) = (Duration::from_millis(400), Duration::from_millis(80));
    let began = std::time::Instant::now();

    let got = attach_or_spawn_by(
        repo.path(),
        agent("codex"),
        budget,
        settle,
        SHARE_ATTEMPTS,
        |_| Ok(Box::new(NotStarted) as Box<dyn Started>),
        |path, deadline| {
            tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            connect_within(path, deadline)
        },
        |r| acquire_lease(r).is_ok(),
    )
    .await;

    let took = began.elapsed();
    match got {
        Attached::Alone(why) => assert!(why.contains("started a monitor"), "{why}"),
        other => panic!("nothing ever listened: {other:?}"),
    }
    assert!(
        took >= budget + settle,
        "it waited out the deadline: {took:?}"
    );
    assert!(
        took < budget + settle + Duration::from_secs(2),
        "and no longer: {took:?}"
    );
    // The initial look, the re-read after taking the permit, then
    // one per step of `settle / SHARE_ATTEMPTS` until the deadline.
    assert!(
        tries.load(std::sync::atomic::Ordering::SeqCst) > SHARE_ATTEMPTS as usize + 1,
        "it kept looking past the old count"
    );
}

/// With no monitor, a subscriber watches the repo itself — the
/// panel must not go blind just because nothing is running.
#[test]
fn a_subscriber_with_no_monitor_watches_the_repo_itself() {
    let repo = repo_with_roster();
    // A real git repo, because the watcher watches the gitdir too.
    // Test code may spawn `git` — the boundary gate exempts it.
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo.path())
        .args(["init", "--quiet"])
        .status()
        .expect("git");
    assert!(status.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let wakes = core_wakes(repo.path(), tx).expect("a watcher of our own");

    std::fs::write(repo.path().join(".clank/config.json"), ROSTER).unwrap();
    rx.recv_timeout(Duration::from_secs(5))
        .expect("a change in the repo still wakes it");
    drop(wakes);
}

/// The handoff reconciles the gap, and only when it covered it.
#[test]
fn the_handoff_wakes_the_consumer_once_it_is_watching() {
    let (tx, rx) = std::sync::mpsc::channel();
    let wake = crate::fs_notice::Wake::from(tx);
    assert!(reconcile_after(&wake, Ok(())).is_some());
    assert_eq!(
        rx.try_recv(),
        Ok(()),
        "a change in the gap is baselined by the new watcher, not \
             reported, so the handoff has to say so itself"
    );
    assert_eq!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty),
        "once, not a stream of them"
    );

    let failed: anyhow::Result<()> = Err(anyhow::anyhow!("no watcher"));
    assert!(reconcile_after(&wake, failed).is_none());
    assert_eq!(
        rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty),
        "and with nothing watching, a wake would promise a freshness \
             that is not coming"
    );
}
