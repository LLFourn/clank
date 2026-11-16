//! An agent's runtime state: `state.json` in its directory, the one
//! document every runtime fact about the agent is stored in.
//!
//! - `binding` and `resume_pending`: which session holds the label. The
//!   only part a hook reads or writes, through [`super::identity`] (I4).
//! - `bound`: what is known of the bound session, one [`Bound`] part per
//!   writer. A change of binding drops it whole, so no part of it can
//!   outlive its session, and a patch to it names its session and applies
//!   only while that session still holds the label.
//! - [`Kept`] parts: the agent's own, across every session.
//!
//! Every writer takes the agent's lock (a flock of its directory, which
//! `config.json`'s writers share), reloads the document, changes only its
//! own part and replaces the file. No writer puts back a document it read
//! earlier, so a stale copy can restore no binding and erase no other part.
//! Readers take no lock: a write replaces the file whole.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clank_core::agent_config::Session;
use clank_core::ids::SessionId;
use clank_user_config::versioned::{self, Document};

use crate::cli::github::log::Locking;

/// The newest `state.json` this build reads and writes. A document with
/// no version is an older build's published projection: nothing in it is
/// read, and the next write replaces it.
pub(crate) const VERSION: u64 = 1;

pub(crate) fn path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("state.json")
}

/// A part of what is known of the bound session, by its key under `bound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bound {
    /// The monitor's turn record ([`crate::cli::turn_custody`]), run-ended
    /// notices included.
    Turn,
    /// The questions it asked ([`crate::cli::question_state`]).
    Questions,
    /// Its recovery episode ([`crate::cli::recovery`]).
    Recovery,
    /// The items that reached it, and when ([`crate::cli::presentation`]).
    Delivered,
    /// What status and the TUI show of it ([`crate::cli::agent_state`]).
    Projection,
    /// It was told the repo has no monitor ([`super::announce_outage`]).
    Outage,
}

impl Bound {
    fn key(self) -> &'static str {
        match self {
            Bound::Turn => "turn",
            Bound::Questions => "questions",
            Bound::Recovery => "recovery",
            Bound::Delivered => "delivered",
            Bound::Projection => "projection",
            Bound::Outage => "outage",
        }
    }
}

/// A part of the agent's state that no change of binding resets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kept {
    /// The highest recovery attempt number ever issued: never lowered.
    Issued,
    /// What the agent is owed, and since when ([`crate::cli::ledger`]).
    Ledger,
}

impl Kept {
    fn key(self) -> &'static str {
        match self {
            Kept::Issued => "issued",
            Kept::Ledger => "ledger",
        }
    }
}

/// The document as one read found it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct State {
    doc: Document,
}

impl State {
    /// `agent_dir`'s state: empty when there is none. One that cannot be
    /// read is an error, never empty.
    pub(crate) fn load(agent_dir: &Path) -> anyhow::Result<State> {
        let path = path(agent_dir);
        let doc = match versioned::read_stored(&path, VERSION)? {
            Some(stored) if stored.versioned => stored.doc,
            _ => Document::new(),
        };
        Ok(State { doc })
    }

    /// The session that holds the label.
    pub(crate) fn binding(&self) -> anyhow::Result<Option<Session>> {
        parse(&self.doc, "binding")
    }

    /// The bound session's resume that has yet to check in.
    pub(crate) fn resume_pending(
        &self,
    ) -> anyhow::Result<Option<clank_core::agent_config::ResumePending>> {
        parse(&self.doc, "resume_pending")
    }

    /// `part` of `session`, when `session` holds the label.
    pub(crate) fn bound<T: serde::de::DeserializeOwned>(
        &self,
        session: &SessionId,
        part: Bound,
    ) -> anyhow::Result<Option<T>> {
        if self.binding()?.is_none_or(|b| &b.id != session) {
            return Ok(None);
        }
        self.part(part)
    }

    /// `part` of whichever session holds the label, with that session.
    pub(crate) fn current<T: serde::de::DeserializeOwned>(
        &self,
        part: Bound,
    ) -> anyhow::Result<Option<(Session, T)>> {
        let Some(binding) = self.binding()? else {
            return Ok(None);
        };
        Ok(self.part(part)?.map(|value| (binding, value)))
    }

    fn part<T: serde::de::DeserializeOwned>(&self, part: Bound) -> anyhow::Result<Option<T>> {
        match self.doc.get("bound").and_then(|b| b.get(part.key())) {
            None => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .with_context(|| format!("`bound.{}` is malformed", part.key())),
        }
    }

    pub(crate) fn kept<T: serde::de::DeserializeOwned>(
        &self,
        part: Kept,
    ) -> anyhow::Result<Option<T>> {
        parse(&self.doc, part.key())
    }
}

fn parse<T: serde::de::DeserializeOwned>(doc: &Document, key: &str) -> anyhow::Result<Option<T>> {
    match doc.get(key) {
        None => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .with_context(|| format!("`{key}` is malformed")),
    }
}

/// One writer's change of the document, under the agent's lock.
pub(crate) struct Txn {
    state: State,
}

impl std::ops::Deref for Txn {
    type Target = State;

    fn deref(&self) -> &State {
        &self.state
    }
}

impl Txn {
    /// Set `part` of `session`, or remove it with `None`, only while
    /// `session` holds the label: `false`, changing nothing, when it does
    /// not.
    pub(crate) fn patch<T: serde::Serialize>(
        &mut self,
        session: &SessionId,
        part: Bound,
        value: Option<&T>,
    ) -> anyhow::Result<bool> {
        if self.binding()?.is_none_or(|b| &b.id != session) {
            return Ok(false);
        }
        let bound = self
            .state
            .doc
            .entry("bound")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        let serde_json::Value::Object(bound) = bound else {
            anyhow::bail!("`bound` is not an object");
        };
        match value {
            Some(value) => {
                bound.insert(part.key().into(), serde_json::to_value(value)?);
            }
            None => {
                bound.remove(part.key());
            }
        }
        if bound.is_empty() {
            self.state.doc.remove("bound");
        }
        Ok(true)
    }

    /// Set `part`, or remove it with `None`.
    pub(crate) fn keep<T: serde::Serialize>(
        &mut self,
        part: Kept,
        value: Option<&T>,
    ) -> anyhow::Result<()> {
        match value {
            Some(value) => {
                self.state
                    .doc
                    .insert(part.key().into(), serde_json::to_value(value)?);
            }
            None => {
                self.state.doc.remove(part.key());
            }
        }
        Ok(())
    }
}

/// Holds an agent's lock until dropped: no writer of its `state.json` or
/// `config.json` commits meanwhile. A flock of the agent's directory, so it
/// names no file of its own.
pub(crate) struct Lock {
    _dir: std::fs::File,
}

impl Lock {
    /// Wait until `agent_dir`'s lock is held, creating the directory.
    pub(crate) fn acquire(agent_dir: &Path) -> anyhow::Result<Lock> {
        Self::take(agent_dir, Locking::Wait)?
            .context("a waiting take of an agent's lock returned untaken")
    }

    /// [`Self::acquire`] without waiting: `None` while another holds it.
    /// A lock found held is waited on by the monitor's loop (I9).
    pub(crate) fn try_acquire(agent_dir: &Path) -> anyhow::Result<Option<Lock>> {
        Self::take(agent_dir, Locking::Try)
    }

    fn take(agent_dir: &Path, locking: Locking) -> anyhow::Result<Option<Lock>> {
        std::fs::create_dir_all(agent_dir)
            .with_context(|| format!("creating `{}`", agent_dir.display()))?;
        let dir = std::fs::File::open(agent_dir)
            .with_context(|| format!("opening `{}`", agent_dir.display()))?;
        let held = crate::cli::github::log::lock_exclusive(agent_dir, &dir, locking)
            .with_context(|| format!("locking `{}`", agent_dir.display()))?;
        Ok(held.then_some(Lock { _dir: dir }))
    }
}

/// Unlocked rather than only closed: the lock belongs to the open
/// directory, and a child forked meanwhile holds a copy of its descriptor
/// until its exec, which would keep it locked past the drop.
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self._dir, rustix::fs::FlockOperation::Unlock);
    }
}

/// Change `agent_dir`'s state with `change`, waiting for its lock: the
/// document is reloaded under the lock, and replaced only when `change`
/// succeeds and changed it.
pub(crate) fn transact<T>(
    agent_dir: &Path,
    permit: &crate::upgrade::Permit,
    change: impl FnOnce(&mut Txn) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    #[cfg(test)]
    tests::before_lock();
    let lock = Lock::acquire(agent_dir)?;
    under(agent_dir, &lock, permit, change)
}

/// [`transact`] without waiting for the lock, as the monitor's loop must
/// (I9): `None`, changing nothing, while another holds it.
pub(crate) fn try_transact<T>(
    agent_dir: &Path,
    permit: &crate::upgrade::Permit,
    change: impl FnOnce(&mut Txn) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    #[cfg(test)]
    tests::before_lock();
    let Some(lock) = Lock::try_acquire(agent_dir)? else {
        return Ok(None);
    };
    under(agent_dir, &lock, permit, change).map(Some)
}

/// [`transact`] for a caller already holding `agent_dir`'s lock.
pub(crate) fn under<T>(
    agent_dir: &Path,
    _lock: &Lock,
    permit: &crate::upgrade::Permit,
    change: impl FnOnce(&mut Txn) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let before = State::load(agent_dir)?;
    let mut txn = Txn {
        state: before.clone(),
    };
    let out = change(&mut txn)?;
    if txn.state != before {
        write(&path(agent_dir), &txn.state.doc, permit)?;
    }
    Ok(out)
}

/// Replace `path` with `doc`, renamed into place whole. Not synced: what
/// a crash loses is runtime state its writers recover from.
fn write(path: &Path, doc: &Document, _permit: &crate::upgrade::Permit) -> anyhow::Result<()> {
    use std::io::Write as _;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    #[cfg(test)]
    tests::refuse_if_unwritable(parent)?;
    let mut stamped = Document::new();
    stamped.insert(versioned::KEY.into(), VERSION.into());
    stamped.extend(doc.iter().map(|(k, v)| (k.clone(), v.clone())));
    let mut tmp = tempfile::Builder::new()
        .prefix(".state-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(&stamped)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    thread_local! {
        static BEFORE_LOCK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Runs, once, whatever this thread armed with [`interleave`].
    pub(super) fn before_lock() {
        if let Some(hook) = BEFORE_LOCK.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    /// Run `other` on this thread at the start of its next transaction,
    /// before that transaction takes the lock: another writer's whole
    /// transaction landing between a writer computing its change and
    /// committing it.
    pub(crate) fn interleave(other: impl FnOnce() + 'static) {
        BEFORE_LOCK.with(|h| *h.borrow_mut() = Some(Box::new(other)));
    }

    /// Bind `session` of `tool` in `agent_dir`, through the one write a
    /// binding hook makes.
    pub(crate) fn bind(agent_dir: &Path, tool: clank_core::Tool, session: &str) {
        bind_session(
            agent_dir,
            &Session {
                id: SessionId::parse(session).unwrap(),
                tool,
                updated_at: "2026-11-17T00:00:00Z".into(),
                transcript: None,
            },
        );
    }

    /// [`bind`] `session` as it is.
    pub(crate) fn bind_session(agent_dir: &Path, session: &Session) {
        let guard = crate::agent_store::StateGuard::in_dir(agent_dir).unwrap();
        super::super::identity::mutate(&guard, |id| id.binding = Some(session.clone())).unwrap();
    }

    static UNWRITABLE: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());

    pub(super) fn refuse_if_unwritable(agent_dir: &Path) -> anyhow::Result<()> {
        let unwritable = UNWRITABLE.lock().unwrap().contains(agent_dir);
        anyhow::ensure!(!unwritable, "`{}` cannot be written", agent_dir.display());
        Ok(())
    }

    /// Every write of `agent_dir`'s state failing, reads still working,
    /// until dropped. Its other files are written as ever.
    pub(crate) fn unwritable(agent_dir: &Path) -> Unwritable {
        UNWRITABLE.lock().unwrap().insert(agent_dir.to_path_buf());
        Unwritable(agent_dir.to_path_buf())
    }

    pub(crate) struct Unwritable(PathBuf);

    impl Drop for Unwritable {
        fn drop(&mut self) {
            UNWRITABLE.lock().unwrap().remove(&self.0);
        }
    }

    /// The document as stored, version and all.
    pub(crate) fn raw(agent_dir: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path(agent_dir)).unwrap()).unwrap()
    }

    fn permit() -> crate::upgrade::Permit {
        crate::upgrade::writing().unwrap()
    }

    #[test]
    fn a_patch_for_a_session_that_lost_the_label_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        bind(dir.path(), clank_core::Tool::Claude, "session-one");
        let one = SessionId::parse("session-one").unwrap();
        assert!(
            transact(dir.path(), &permit(), |t| t.patch(
                &one,
                Bound::Turn,
                Some(&1)
            ))
            .unwrap()
        );
        bind(dir.path(), clank_core::Tool::Claude, "session-two");
        let before = std::fs::read(path(dir.path())).unwrap();
        assert!(
            !transact(dir.path(), &permit(), |t| t.patch(
                &one,
                Bound::Turn,
                Some(&2)
            ))
            .unwrap()
        );
        assert_eq!(std::fs::read(path(dir.path())).unwrap(), before);
    }

    #[test]
    fn a_rebind_drops_every_bound_part_and_keeps_the_kept_ones() {
        let dir = tempfile::tempdir().unwrap();
        bind(dir.path(), clank_core::Tool::Claude, "session-one");
        let one = SessionId::parse("session-one").unwrap();
        transact(dir.path(), &permit(), |t| {
            for part in [
                Bound::Turn,
                Bound::Questions,
                Bound::Recovery,
                Bound::Delivered,
                Bound::Projection,
                Bound::Outage,
            ] {
                assert!(t.patch(&one, part, Some(&true))?);
            }
            t.keep(Kept::Issued, Some(&7))?;
            t.keep(Kept::Ledger, Some(&Vec::<u8>::new()))
        })
        .unwrap();
        bind(dir.path(), clank_core::Tool::Codex, "session-two");
        let raw = raw(dir.path());
        assert!(raw.get("bound").is_none(), "{raw}");
        assert_eq!(raw["issued"], 7);
        assert_eq!(raw["binding"]["id"], "session-two");

        bind(dir.path(), clank_core::Tool::Codex, "session-two");
        let two = SessionId::parse("session-two").unwrap();
        transact(dir.path(), &permit(), |t| {
            t.patch(&two, Bound::Turn, Some(&3))
        })
        .unwrap();
        bind(dir.path(), clank_core::Tool::Codex, "session-two");
        assert_eq!(
            raw_bound(dir.path(), "turn"),
            Some(serde_json::json!(3)),
            "binding the same session again is no rebind"
        );
    }

    fn raw_bound(dir: &Path, key: &str) -> Option<serde_json::Value> {
        raw(dir).get("bound").and_then(|b| b.get(key)).cloned()
    }

    #[test]
    fn an_older_builds_projection_reads_as_no_state() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            path(dir.path()),
            r#"{"role":"master","session":"s","binding":{"id":"s","tool":"claude","updated_at":"x"}}"#,
        )
        .unwrap();
        let state = State::load(dir.path()).unwrap();
        assert_eq!(state.binding().unwrap(), None);
        assert_eq!(crate::agent_store::binding_in(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_held_lock_is_contended_for_a_take_that_does_not_wait() {
        let dir = tempfile::tempdir().unwrap();
        bind(dir.path(), clank_core::Tool::Claude, "session-one");
        let held = crate::cli::github::log::tests::hold_paths(&[dir.path().to_path_buf()]);
        let tried =
            try_transact(dir.path(), &permit(), |t| t.keep(Kept::Issued, Some(&1))).unwrap();
        assert!(tried.is_none());
        assert!(
            crate::cli::github::log::found_held()
                .iter()
                .any(|(p, _)| p == dir.path()),
            "the loop is told to wait for its release"
        );
        let path = dir.path().to_path_buf();
        let waiter =
            std::thread::spawn(move || crate::cli::github::log::when_released(&path, true));
        held.release();
        waiter
            .join()
            .unwrap()
            .expect("a directory's release is waited on");
        assert!(
            !crate::cli::github::log::found_held()
                .iter()
                .any(|(p, _)| p == dir.path()),
            "seen free, it is waited on no longer"
        );
        assert!(
            try_transact(dir.path(), &permit(), |t| t.keep(Kept::Issued, Some(&1)))
                .unwrap()
                .is_some()
        );
    }
}
