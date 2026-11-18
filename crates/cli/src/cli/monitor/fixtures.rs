pub(super) use super::{
    clients::*, connect::*, custody::*, deadline_round, expire_deliveries, fold::*, hosted::*,
    lease::*, published::*, releases::*, retire::*, retire_gone_runs, retry_contended, runtime::*,
    settle_result, socket::*, spawn::*, wire::*,
};
pub(super) use crate::cli::watch_store::Stream;
pub(super) use clank_core::ids::AgentLabel;
pub(super) use clank_core::wait::WaitItem;
pub(super) use std::time::Duration;

pub(super) fn repo_dir() -> TempRepo {
    TempRepo::new()
}

/// A monitor, for the length of one handshake. `reply` is written
/// verbatim so a test can coalesce frames, or write nothing and
/// stay open.
pub(super) fn fake_monitor(
    repo: &std::path::Path,
    reply: Option<String>,
) -> (tempfile::TempDir, std::thread::JoinHandle<Option<Hello>>) {
    use std::io::{Read, Write};
    let dir = tempfile::tempdir().unwrap();
    let path = socket_path(dir.path(), 4242).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    publish(
        repo,
        &Descriptor {
            proto: PROTO,
            pid: 4242,
            repo: repo.to_string_lossy().to_string(),
            socket: path.to_string_lossy().to_string(),
            build: None,
        },
    )
    .unwrap();

    let served = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // Read the hello a byte at a time: a BufReader here would
        // have the same read-ahead problem this test is about.
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
            line.push(byte[0]);
        }
        let heard = serde_json::from_slice::<Hello>(&line).ok();
        match reply {
            Some(reply) => {
                stream.write_all(reply.as_bytes()).unwrap();
                stream.flush().unwrap();
                // Hold the connection open so the client reads from
                // a live peer rather than racing an EOF.
                std::thread::sleep(Duration::from_millis(200));
            }
            // Accept, then say nothing at all — and stay open
            // well past any budget a test sets, so a fallback is
            // evidence of the DEADLINE and not of an EOF.
            None => std::thread::sleep(Duration::from_millis(1500)),
        }
        heard
    });
    (dir, served)
}

pub(super) fn welcome_line() -> String {
    format!(
        "{}\n",
        serde_json::to_string(&Welcome::Ok { proto: PROTO }).unwrap()
    )
}

/// An agent client, for the tests whose subject is not identity.
pub(super) fn agent(author: &str) -> Who {
    Who::Agent {
        author: author.into(),
        holder: None,
        has_cli_sources: false,
        turn_end: None,
    }
}

pub(super) const ROSTER: &str = r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#;

pub(super) fn repo_with_roster() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(d.path().join(".clank")).unwrap();
    std::fs::write(d.path().join(".clank/config.json"), ROSTER).unwrap();
    d
}

/// Fill a stream so a later `write_all` on it cannot drain, and
/// report how much went in.
///
/// Filled through STD, not tokio: `tokio::try_write` reports
/// `WouldBlock` until readiness has been registered, so a fill loop
/// over a fresh tokio stream exits having written nothing and every
/// client drains happily. And filled from the WRITER's side — the
/// writer task writes mine→peer, so filling the peer's send buffer
/// would block nothing. Both mistakes were made before this comment
/// existed.
pub(super) fn wedge(stream: &std::os::unix::net::UnixStream) -> usize {
    use std::io::Write;
    stream.set_nonblocking(true).unwrap();
    let junk = vec![0u8; 32 * 1024];
    let mut total = 0;
    loop {
        match (&mut &*stream).write(&junk) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    stream.set_nonblocking(false).unwrap();
    total
}

/// Clients as the owner holds them. Peers are returned so a queue
/// is not closed by a dropped socket, and are never read — a
/// wedged one stays wedged.
pub(super) fn registry(
    authors: &[(&str, bool)],
) -> (Vec<Client>, Vec<std::os::unix::net::UnixStream>) {
    let mut clients = Vec::new();
    let mut peers = Vec::new();
    for (author, wedged) in authors {
        let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        if *wedged {
            assert!(wedge(&mine) > 0, "the wedge wrote nothing");
        }
        mine.set_nonblocking(true).unwrap();
        let mine = tokio::net::UnixStream::from_std(mine).unwrap();
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let label = AgentLabel::parse(author).unwrap();
        tokio::spawn(write_to(mine, queued, label.to_string()));
        clients.push(Client {
            writer: None,
            who: Served::Agent { author: label },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            admitted: Default::default(),
            unavailable: false,
            report: None,
            farewell: None,
        });
        peers.push(theirs);
    }
    (clients, peers)
}

/// Give an agent a config source, as `wait_events` would.
pub(super) fn with_source(repo: &std::path::Path, author: &str, name: &str) {
    let dir = crate::agent_store::agents_root(repo).join(author);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "wait_events": [
                {"kind": "command", "name": name, "command": ["true"]}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}

/// Sources hosting nothing and holding `items`.
pub(super) fn holding(items: Vec<(Stream, WaitItem)>) -> Sources {
    let mut sources = Sources::hosting(Default::default());
    sources.hold(items);
    sources
}

/// Run `f` with `turns` and `recovery` in a custody of their own, handed
/// back after.
pub(super) fn in_custody<T>(
    turns: &mut crate::cli::turn_custody::Turns,
    recovery: &mut crate::cli::recovery::Driver,
    f: impl FnOnce(&mut Custody) -> T,
) -> T {
    let mut custody = Custody::of(
        std::mem::replace(
            turns,
            crate::cli::turn_custody::Turns::new(Default::default()),
        ),
        std::mem::replace(
            recovery,
            crate::cli::recovery::Driver::new(Default::default(), Default::default()),
        ),
    );
    let out = f(&mut custody);
    (*turns, *recovery) = custody.into_parts();
    out
}

/// [`in_custody`] for a step that consults no recovery.
pub(super) fn in_turns<T>(
    turns: &mut crate::cli::turn_custody::Turns,
    f: impl FnOnce(&mut Custody) -> T,
) -> T {
    in_custody(
        turns,
        &mut crate::cli::recovery::Driver::new(Default::default(), Default::default()),
        f,
    )
}

/// Run `f` with sources hosting `hosted` and holding `pending`, both
/// handed back after.
pub(super) fn in_sources<T>(
    hosted: &mut std::collections::BTreeMap<crate::cli::watch_store::Producer, Hosted>,
    pending: &mut Vec<(Stream, WaitItem)>,
    f: impl FnOnce(&mut Sources) -> T,
) -> T {
    let mut sources = Sources::hosting(std::mem::take(hosted));
    sources.hold(std::mem::take(pending));
    let out = f(&mut sources);
    (*hosted, *pending) = sources.into_parts();
    out
}

/// [`in_sources`] hosting nothing.
pub(super) fn holding_in<T>(
    pending: &mut Vec<(Stream, WaitItem)>,
    f: impl FnOnce(&mut Sources) -> T,
) -> T {
    in_sources(&mut Default::default(), pending, f)
}

/// Run `f` with `list` connected, handed back after with the writers of
/// the clients it retired.
pub(super) fn in_clients<T>(
    list: &mut Vec<Client>,
    f: impl FnOnce(&mut Clients) -> T,
) -> (T, Vec<tokio::task::JoinHandle<Wrote>>) {
    let mut clients = Clients::of(std::mem::take(list));
    let out = f(&mut clients);
    let (served, leaving) = clients.retire();
    *list = served;
    (out, leaving)
}

/// [`unavailable_somewhere`] over `hosted`, `clients` and `recovery`.
pub(super) fn unavailable_in(
    hosted: &mut std::collections::BTreeMap<crate::cli::watch_store::Producer, Hosted>,
    clients: &mut Vec<Client>,
    recovery: &mut crate::cli::recovery::Driver,
) -> bool {
    in_sources(hosted, &mut Vec::new(), |sources| {
        in_clients(clients, |clients| {
            in_custody(
                &mut crate::cli::turn_custody::Turns::new(Default::default()),
                recovery,
                |custody| unavailable_somewhere(sources, clients, custody),
            )
        })
        .0
    })
}
