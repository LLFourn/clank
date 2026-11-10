use super::*;
use clank_core::{SessionId, Tool};

/// Whether the binding lock was held at the paused write, and the rival
/// started there.
type Paused = std::cell::RefCell<Option<(bool, std::thread::JoinHandle<()>)>>;

fn label() -> AgentLabel {
    AgentLabel::parse("agent").unwrap()
}

fn bind(repo: &Path, session: &str) {
    crate::agent_store::bind_session_to_agent(
        repo,
        &label(),
        Tool::Codex,
        &SessionId::parse(session).unwrap(),
    )
    .unwrap();
}

fn deliver(repo: &Path, session: &str, identity: &str) {
    record_delivered(
        repo,
        &label(),
        Tool::Codex,
        session,
        &Carried {
            events: Vec::new(),
            answers: Vec::new(),
            identities: vec![identity.to_string()],
        },
        || true,
    );
}

fn dir(repo: &Path) -> PathBuf {
    crate::agent_store::agents_root(repo).join("agent")
}

/// Whether the label's binding lock is free to take at this instant.
fn binding_lock_is_free(agent_dir: &Path) -> bool {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(agent_dir.join("binding.lock"))
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_ok()
}

/// Deliver `identity` to `session`, pausing between the merge and the
/// write: note there whether the binding lock is held, and start `rival`
/// on a thread of its own, which can only get anywhere once this write is
/// done. Whether the lock was held, once the rival has finished.
fn deliver_against(
    repo: &Path,
    session: &str,
    identity: &str,
    rival: impl FnOnce() + Send + 'static,
) -> bool {
    let paused = Paused::new(None);
    {
        let agent = label();
        let guard = crate::agent_store::hold_binding(repo, &agent).unwrap();
        let received = receive_with(
            &guard,
            Tool::Codex,
            session,
            &[identity.to_string()],
            1,
            |agent_dir| {
                let held = !binding_lock_is_free(agent_dir);
                *paused.borrow_mut() = Some((held, std::thread::spawn(rival)));
            },
        );
        assert!(received.unwrap(), "the bound session's receipt is taken");
    }
    let (held, rival) = paused.into_inner().expect("the write was reached");
    rival.join().unwrap();
    held
}

/// A delivery commits off the monitor's loop, so two can be publishing at
/// once. Each merges and writes while it holds the label's binding lock,
/// which the other needs: a writer paused between its merge and its write
/// cannot be overtaken, and the delivery that was waiting on it adds to
/// its receipt rather than replacing it.
#[test]
fn concurrent_deliveries_keep_each_others_receipts() {
    let repo = tempfile::tempdir().unwrap();
    bind(repo.path(), "session-one");
    let path = repo.path().to_path_buf();
    let held = deliver_against(repo.path(), "session-one", "first", move || {
        deliver(&path, "session-one", "second")
    });
    assert!(held, "the merge is made under the binding lock");
    let delivered = delivered_to(&dir(repo.path()), "session-one");
    assert_eq!(
        delivered.keys().map(String::as_str).collect::<Vec<_>>(),
        ["first", "second"],
        "neither delivery lost the other's receipt"
    );
}

/// A delivery to a session that has passed its binding check holds the
/// lock a rebind needs, so the rebind and the new session's first receipt
/// land after it, whole. The old session's receipt is then another
/// session's, and the new session's is not replaced by it.
#[test]
fn a_rebind_cannot_land_inside_an_old_sessions_receipt() {
    let repo = tempfile::tempdir().unwrap();
    bind(repo.path(), "session-old");
    let path = repo.path().to_path_buf();
    let held = deliver_against(repo.path(), "session-old", "mine", move || {
        bind(&path, "session-new");
        deliver(&path, "session-new", "theirs");
    });
    assert!(held, "the binding cannot move under a receipt");
    assert_eq!(
        delivered_to(&dir(repo.path()), "session-new")
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["theirs"],
        "the new session's receipt stands"
    );
    assert!(delivered_to(&dir(repo.path()), "session-old").is_empty());
}

/// A delivery whose session lost the label before it could record
/// publishes nothing: the successor's receipts are untouched, whatever
/// the callback's own stale authorization says.
#[test]
fn an_obsolete_delivery_publishes_nothing() {
    let repo = tempfile::tempdir().unwrap();
    bind(repo.path(), "session-old");
    deliver(repo.path(), "session-old", "mine");
    bind(repo.path(), "session-new");
    deliver(repo.path(), "session-new", "theirs");
    let before = std::fs::read(receipts_path(&dir(repo.path()))).unwrap();
    deliver(repo.path(), "session-old", "late");
    assert_eq!(
        std::fs::read(receipts_path(&dir(repo.path()))).unwrap(),
        before,
        "`current` said yes; the binding, read under its lock, said no"
    );
    let agent = label();
    let guard = crate::agent_store::hold_binding(repo.path(), &agent).unwrap();
    assert!(!receive(&guard, Tool::Codex, "session-old", &["late".into()], 9).unwrap());
    assert!(
        !receive(&guard, Tool::Claude, "session-new", &["late".into()], 9).unwrap(),
        "nor another tool's session of the same id"
    );
    assert!(receive(&guard, Tool::Codex, "session-new", &["more".into()], 9).unwrap());
}
