use crate::cli::monitor::fixtures::*;

/// A producer removed JUST BEFORE a retirement is still this
/// monitor's to see out.
///
/// `reconcile_hosting` cancels it and the current hosted map stops
/// mentioning it, so a retirement that looked only at that map
/// would report everything settled while the removed one was still
/// The whole mechanism: a monitor that declines a NEWER client is
/// what an install replaced, so it goes — and one that declines an
/// OLDER client keeps the repo.
///
/// Both directions in one test, because getting them backwards is
/// the only way this can be dangerous. The stale party is whoever
/// speaks the lower number: older monitor, it leaves; older
/// client, it does its own local wait exactly as before.
#[test]
fn a_monitor_leaves_for_a_newer_client_and_stays_for_an_older_one() {
    let repo = repo_with_roster();
    let hello = |proto: u32| {
        serde_json::to_string(&Hello {
            proto,
            build: None,
            repo: repo.path().to_string_lossy().to_string(),
            who: agent("codex"),
        })
        .unwrap()
    };

    for newer in [PROTO + 1, PROTO + 9] {
        match greeting_of(&hello(newer), repo.path()) {
            Greeting::Obsolete(Welcome::Decline { proto, .. }) => {
                assert_eq!(proto, PROTO, "it answers as itself before it goes")
            }
            other => panic!("a client on {newer} replaces a monitor on {PROTO}: {other:?}"),
        }
    }

    for older in [PROTO - 1, 23, 1] {
        assert!(
            matches!(
                greeting_of(&hello(older), repo.path()),
                Greeting::Decline(_)
            ),
            "an older client is the stale one; the monitor keeps the repo"
        );
    }

    // And our own version is served, which is the ordinary case
    // this must not disturb.
    assert!(matches!(
        greeting_of(&hello(PROTO), repo.path()),
        Greeting::Client(_)
    ));
}

/// The `user_command` silent reason is protocol 19: a protocol 18 hook,
/// whose closed reason enum cannot decode it, is not admitted to a
/// verdict carrying it, and a 19 client meeting an 18 monitor replaces
/// it rather than relying on a release that monitor never makes.
#[test]
fn the_user_command_verdict_is_negotiated_as_protocol_19() {
    #[derive(Debug, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    #[allow(dead_code)]
    enum Proto18Reason {
        AutoOff,
        StaleBinding,
        OwnerGone,
        WaitArmed,
    }
    let wire = serde_json::to_string(&clank_core::SilentReason::UserCommand).unwrap();
    assert!(serde_json::from_str::<Proto18Reason>(&wire).is_err());
    const { assert!(PROTO >= 19) };

    let repo = repo_with_roster();
    let hello = |proto: u32| {
        serde_json::to_string(&Hello {
            proto,
            build: None,
            repo: repo.path().to_string_lossy().to_string(),
            who: agent("codex"),
        })
        .unwrap()
    };
    assert!(
        matches!(greeting_of(&hello(18), repo.path()), Greeting::Decline(_)),
        "an 18 hook is not admitted"
    );
    assert!(
        !admits(PROTO, 18),
        "a 19 client does not share an 18 monitor"
    );
    assert!(matches!(
        greeting_of(&hello(PROTO), repo.path()),
        Greeting::Client(_)
    ));
}

/// Protocol 20 reads no task listing: a 19 hook's Stop carries one and
/// its Grok handoff carries the hook's own waiter evidence, so it is
/// declined rather than half-read.
#[test]
fn a_hook_that_reports_a_task_listing_is_declined() {
    const { assert!(PROTO >= 20) };
    let repo = repo_with_roster();
    let hello = |proto: u32| {
        serde_json::to_string(&Hello {
            proto,
            build: None,
            repo: repo.path().to_string_lossy().to_string(),
            who: agent("codex"),
        })
        .unwrap()
    };
    assert!(matches!(
        greeting_of(&hello(19), repo.path()),
        Greeting::Decline(_)
    ));
    assert!(!admits(PROTO, 19));
}

/// A higher version alone retires NOTHING.
///
/// The hole this closes, and it was real: the first version
/// re-read `Envelope.proto` and converted any greater number into
/// a retirement, so a bare `{"proto":6}` — no repo, no identity,
/// from anyone who could reach the socket — ended the repo's owner
/// (codex on c4b5899). Each frame here carries a HIGHER proto, so
/// the version is never what distinguishes them; only the routing
/// and the identity differ.
///
/// I had claimed an unknown author could not reach this decision.
/// That was true of the equal-version path and false of the one
/// that matters, which is why these all use `PROTO + 1`.
#[test]
fn a_higher_version_alone_retires_nothing() {
    let repo = repo_with_roster();
    let newer = PROTO + 1;
    let here = repo.path().to_string_lossy().to_string();

    // No repo and no identity at all.
    assert!(
        matches!(
            greeting_of(&format!(r#"{{"proto":{newer}}}"#), repo.path()),
            Greeting::Decline(_)
        ),
        "a bare version is not a client, and ends nothing"
    );

    // Addressed to a DIFFERENT repo.
    let elsewhere = serde_json::to_string(&Hello {
        proto: newer,
        build: None,
        repo: "/some/other/repo".into(),
        who: agent("codex"),
    })
    .unwrap();
    assert!(
        matches!(greeting_of(&elsewhere, repo.path()), Greeting::Decline(_)),
        "a newer client asking for another repo must not end this one's monitor"
    );

    // This repo, but nobody this repo knows.
    let ghost = format!(
        r#"{{"proto":{newer},"repo":{here:?},"who":{{"as":"agent","author":"ghost","holder":null,"has_cli_sources":false}}}}"#
    );
    match greeting_of(&ghost, repo.path()) {
        Greeting::Decline(Welcome::Decline { why, .. }) => {
            assert!(why.contains("roster"), "{why}")
        }
        other => panic!("an unknown author must not end a monitor: {other:?}"),
    }

    // A client KIND this build does not know.
    let alien = format!(r#"{{"proto":{newer},"repo":{here:?},"who":{{"as":"something_later"}}}}"#);
    assert!(
        matches!(greeting_of(&alien, repo.path()), Greeting::Decline(_)),
        "a client we cannot identify is not one whose word ends a repo's owner"
    );

    // And the one that SHOULD: this repo, a known agent, newer.
    let known = serde_json::to_string(&Hello {
        proto: newer,
        build: None,
        repo: here.clone(),
        who: agent("codex"),
    })
    .unwrap();
    assert!(
        matches!(greeting_of(&known, repo.path()), Greeting::Obsolete(_)),
        "addressed here, by somebody this repo knows, on a newer version"
    );
    // An observer too: it has no author to check, and it is still
    // a client of this repo on a newer build.
    let watcher = format!(r#"{{"proto":{newer},"repo":{here:?},"who":{{"as":"observer"}}}}"#);
    assert!(matches!(
        greeting_of(&watcher, repo.path()),
        Greeting::Obsolete(_)
    ));
}

/// The client REACHES an older holder, which is the whole point.
///
/// `attach_by` used to refuse a descriptor whose proto differed
/// before it connected, so the greeting that tells a holder to go
/// was never sent and the self-exit clause never ran in production
/// (codex on c4b5899). The descriptor is a hint about where to
/// knock; the handshake settles the version.
///
/// Driven through `attach_by` — the production discovery path —
/// rather than by opening a socket by hand, which is what hid
/// this.
#[test]
fn a_client_greets_a_holder_whose_descriptor_is_older() {
    use std::io::{BufRead, BufReader, Write};

    let repo = repo_dir();
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_path(dir.path(), 4499).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    publish(
        repo.path(),
        &Descriptor {
            proto: PROTO - 1,
            pid: 4499,
            repo: repo.path().to_string_lossy().to_string(),
            socket: socket.to_string_lossy().to_string(),
            build: None,
        },
    )
    .unwrap();

    // Bounded, because a test must not be able to WAIT FOR EVER on
    // the thing it is checking. A mutation that stopped the client
    // connecting left this `accept` blocked, the join with it, and
    // the mutation run hung holding the mutant in the tree — which
    // is the state this repo has learned to fear.
    listener
        .set_nonblocking(false)
        .and_then(|()| listener.try_clone())
        .unwrap();
    let heard = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let stream = loop {
            listener.set_nonblocking(true).unwrap();
            match listener.accept() {
                Ok((s, _)) => break Some(s),
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break None,
            }
        };
        let Some(stream) = stream else {
            return String::new();
        };
        stream.set_nonblocking(false).unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let decline = Welcome::Decline {
            proto: PROTO - 1,
            why: "older".into(),
        };
        let _ = writeln!(
            reader.get_mut(),
            "{}",
            serde_json::to_string(&decline).unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
        line
    });

    let got = attach_by(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within);
    assert!(
        matches!(&got, Attached::Alone(_)),
        "an older monitor cannot serve us: {got:?}"
    );

    let greeting = heard.join().unwrap();
    assert!(
        !greeting.is_empty(),
        "the client must actually have connected and greeted the older holder"
    );
    let open: Opening = serde_json::from_str(greeting.trim_end())
        .expect("the holder received a greeting it can read at the floor");
    assert_eq!(
        open.proto, PROTO,
        "and it carries OUR version, which is what tells it to go"
    );
}

/// ONLY a version mismatch ends it.
///
/// A client turned away for its repo, or for anything else,
/// says nothing about this monitor — and if such a decline
/// ended it, any stranger could end a repo's owner by asking
/// wrongly. The declines here are all for OUR OWN proto, so the
/// version is never in question and the reason is the only thing
/// that differs.
#[test]
fn only_a_version_mismatch_ends_a_monitor() {
    let repo = repo_with_roster();
    let elsewhere = serde_json::to_string(&Hello {
        proto: PROTO,
        build: None,
        repo: "/some/other/repo".into(),
        who: agent("codex"),
    })
    .unwrap();
    assert!(
        matches!(greeting_of(&elsewhere, repo.path()), Greeting::Decline(_)),
        "asking for another repo must not retire this one's monitor"
    );

    let ghost = serde_json::to_string(&Hello {
        proto: PROTO,
        build: None,
        repo: repo.path().to_string_lossy().to_string(),
        who: agent("ghost"),
    })
    .unwrap();
    // An author off the roster is refused LATER, by `served_for`,
    // so it never reaches this decision at all — the same
    // guarantee arrived at from the other side.
    assert!(
        matches!(greeting_of(&ghost, repo.path()), Greeting::Client(_)),
        "the roster is not this decision's business"
    );

    assert!(
        matches!(
            greeting_of("not a frame at all", repo.path()),
            Greeting::Decline(_)
        ),
        "and an unreadable frame carries no version, so it decides nothing"
    );
}

#[test]
fn a_version_mismatch_declines_rather_than_fails() {
    assert!(admits(PROTO, PROTO));
    assert!(!admits(PROTO, PROTO + 1), "a newer monitor is not ours");
    assert!(!admits(PROTO, PROTO - 1), "nor the version before this");
    assert!(!admits(PROTO - 1, PROTO), "in either direction");
}

/// The ordering that makes a skew legible.
///
/// The version is read FIRST, from a frame that decodes under any
/// version, and a mismatch is a decline. What the body would do is
/// deliberately not relied on: an older frame might fail to decode,
/// or might decode with a missing `Option` silently read as `None`
/// — a bound client served as an unbound one. Neither is a signal,
/// so neither is consulted (codex on 4915f33).
#[test]
fn an_older_frame_is_declined_by_its_version() {
    let proto1 = r#"{"proto":1,"author":"codex","session":null,"has_cli_sources":false}"#;

    let envelope: Envelope = serde_json::from_str(proto1).expect("the version reads first");
    assert_eq!(envelope.proto, 1);
    assert!(!admits(PROTO, envelope.proto), "and that is the decline");
}

/// An unbound terminal wait must stay representable.
#[test]
fn a_wait_that_holds_nothing_says_so() {
    let hello = Hello {
        proto: PROTO,
        build: None,
        repo: "/w/proj".into(),
        who: agent("claude"),
    };
    let back: Hello = serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
    assert_eq!(back, hello, "no session is not an absent field");
}

/// And a half-pair must not be.
#[test]
fn half_a_holder_does_not_parse() {
    for raw in [r#"{"tool":"codex"}"#, r#"{"session":"s-1"}"#] {
        assert!(
            serde_json::from_str::<Holder>(raw).is_err(),
            "a half holder invents or loses an identity: {raw}"
        );
    }
}

/// A descriptor can outlive the repo it describes.
#[test]
fn a_descriptor_for_another_repo_declines() {
    let ours = std::path::Path::new("/w/proj");
    assert!(serves(ours, "/w/proj"));
    assert!(!serves(ours, "/w/other"));
    assert!(
        !serves(ours, "/w/proj-copy"),
        "a prefix is a different repo"
    );
}

#[test]
fn a_descriptor_round_trips() {
    let d = Descriptor {
        proto: PROTO,
        pid: 4242,
        repo: "/w/proj".into(),
        socket: "/tmp/clank-socks-501/4242.sock".into(),
        build: None,
    };
    let back: Descriptor = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
    assert_eq!(back, d);
}

fn hello_line(proto: u32, repo: &str) -> String {
    serde_json::to_string(&serde_json::json!({
        "proto": proto,
        "repo": repo,
        "who": {"as": "agent", "author": "codex", "holder": null, "has_cli_sources": false},
    }))
    .unwrap()
}

#[test]
fn a_matching_hello_is_accepted() {
    let repo = std::path::Path::new("/w/proj");
    let hello = accept_hello(&hello_line(PROTO, "/w/proj"), repo).expect("accepted");
    assert_eq!(hello.who, agent("codex"));
}

/// Every refusal is a Decline the client can act on, never a parse
/// error it cannot tell from corruption.
#[test]
fn every_refusal_names_itself() {
    let repo = std::path::Path::new("/w/proj");
    let why = |line: &str| match accept_hello(line, repo) {
        Err(Welcome::Decline { why, proto }) => {
            assert_eq!(proto, PROTO, "a decline says what we speak");
            why
        }
        other => panic!("expected a decline, got {other:?}"),
    };

    assert!(why("not json at all").contains("not a monitor hello"));
    assert!(why(&hello_line(PROTO + 1, "/w/proj")).contains("client speaks"));
    assert!(why(&hello_line(PROTO, "/w/other")).contains("serves"));
    // Versioned as OURS — a literal here would start declining
    // for its version the next time PROTO moves, and would stop
    // testing the parse at all.
    assert!(
        why(&format!(
            r#"{{"proto":{PROTO},"who":{{"as":"agent","author":"codex"}}}}"#
        ))
        .contains("did not parse"),
        "a well-versioned frame missing the repo is still a decline"
    );
}

/// The version is consulted BEFORE the body, so a frame that is
/// both older AND unparseable declines for its version.
#[test]
fn an_old_frame_declines_for_its_version_not_its_shape() {
    let repo = std::path::Path::new("/w/proj");
    // Proto 1: no repo, no holder. Unparseable as ours either way.
    let old = r#"{"proto":1,"author":"codex","session":null,"has_cli_sources":false}"#;
    match accept_hello(old, repo) {
        Err(Welcome::Decline { why, .. }) => assert!(
            why.contains("client speaks 1"),
            "the version decided, not the parse: {why}"
        ),
        other => panic!("expected a decline, got {other:?}"),
    }
}

/// The handshake over a real socket, in process — no binary is
/// started, which this repo forbids in tests.
#[test]
fn the_handshake_crosses_a_real_socket() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4242).expect("a tempdir holds a socket");
    let listener = UnixListener::bind(&path).expect("bind");

    let repo = std::path::PathBuf::from("/w/proj");
    let served = {
        let repo = repo.clone();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let welcome = match accept_hello(line.trim_end(), &repo) {
                Ok(_) => Welcome::Ok { proto: PROTO },
                Err(decline) => decline,
            };
            let mut out = reader.into_inner();
            writeln!(out, "{}", serde_json::to_string(&welcome).unwrap()).unwrap();
        })
    };

    let mut client = UnixStream::connect(&path).unwrap();
    writeln!(client, "{}", hello_line(PROTO, "/w/proj")).unwrap();
    let mut answer = String::new();
    BufReader::new(&client).read_line(&mut answer).unwrap();
    served.join().unwrap();

    let welcome: Welcome = serde_json::from_str(answer.trim_end()).unwrap();
    assert!(
        matches!(welcome, Welcome::Ok { proto, .. } if proto == PROTO),
        "{welcome:?}"
    );
}

/// The frames must survive the wire, since a client that cannot
/// read a note has no way to tell that from having no work.
#[test]
fn every_frame_round_trips() {
    let hello = Hello {
        proto: PROTO,
        build: None,
        repo: "/w/proj".into(),
        who: Who::Agent {
            author: "codex".into(),
            holder: Some(Holder {
                tool: clank_core::vocab::Tool::Codex,
                session: "s-1".into(),
            }),
            has_cli_sources: true,
            turn_end: None,
        },
    };
    let back: Hello = serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
    assert_eq!(back, hello);

    for note in [
        Note::Beat,
        Note::Items {
            source_cwd: None,
            items: vec![WaitItem::PromoteFromQueue {
                name: "next".into(),
                priority: 500,
            }],
        },
    ] {
        let back: Note = serde_json::from_str(&serde_json::to_string(&note).unwrap()).unwrap();
        assert_eq!(back, note);
    }

    for welcome in [
        Welcome::Ok { proto: PROTO },
        Welcome::Decline {
            proto: PROTO,
            why: "proto 2".into(),
        },
    ] {
        let back: Welcome =
            serde_json::from_str(&serde_json::to_string(&welcome).unwrap()).unwrap();
        assert_eq!(back, welcome);
    }
}
