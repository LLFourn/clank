//! The queue, the id locks and the attempt ledger:
//! `~/.clank/user-commands/<repo_tag>/<label>/<id>.json`, `<id>.lock` beside
//! it. Outside every repo, so agents have no reason to edit it; that
//! protects nothing against a same-UID writer, which can reset or delete a
//! genuine envelope's state and so defeat replay prevention (README).
//!
//! Durable, in the home directory rather than `/tmp`, so a reboot inside an
//! envelope's lifetime cannot reset replay state. Directories are 0700 and
//! files 0600, vetted before every read and opened without following links.

use super::envelope::{Envelope, Id};
use super::trust::{KeyName, Pin};
use anyhow::{Context, bail, ensure};
use clank_core::ids::AgentLabel;
use std::path::{Path, PathBuf};

/// Where a request stands. The last four are terminal: nothing moves a
/// record out of them, whatever happens to the key or the clock later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum State {
    Admitted,
    WaitingForIdle,
    /// Persisted, and synced, before the first byte is written.
    Dispatching,
    /// The transport accepted the `!` line and Enter. Not that the command
    /// succeeded: its output is in the agent's conversation.
    Delivered,
    /// Something may have been typed; it is never retried.
    Unconfirmed,
    Refused,
    /// The first write never ran: nothing was typed.
    FailedNotStarted,
}

impl State {
    pub(crate) fn terminal(self) -> bool {
        matches!(
            self,
            State::Delivered | State::Unconfirmed | State::Refused | State::FailedNotStarted
        )
    }
}

/// The pane a request's agent was found in at the monitor's first look.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaneSeen {
    pub(crate) session: String,
    pub(crate) pane: String,
    pub(crate) pid: i32,
    pub(crate) birth: crate::proc_identity::ProcToken,
}

/// One request's entry: the exact envelope as admitted, and clank's own
/// state machine. No unsigned copy of the command, target, id or times:
/// every use derives those from the envelope, verified again.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub(crate) envelope: Envelope,
    pub(crate) state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pane: Option<PaneSeen>,
    /// The fingerprint of the key it was admitted under.
    pub(crate) key: String,
    /// The name that key was trusted as. A record from before keys had
    /// names has none, and was admitted under the key now named `default`.
    #[serde(default = "KeyName::legacy")]
    pub(crate) key_name: KeyName,
    pub(crate) admitted_at: u64,
    pub(crate) updated_at: u64,
}

impl Record {
    pub(crate) fn admitted(envelope: Envelope, pin: Pin, now: u64) -> Self {
        Record {
            envelope,
            state: State::Admitted,
            reason: Some("the repo's monitor has not looked at it yet".into()),
            pane: None,
            key: pin.fingerprint,
            key_name: pin.name,
            admitted_at: now,
            updated_at: now,
        }
    }

    /// The entry every use after admission verifies under.
    pub(crate) fn pin(&self) -> Pin {
        Pin {
            name: self.key_name.clone(),
            fingerprint: self.key.clone(),
        }
    }

    /// Move to `state` for `reason`. False, and nothing changed, when that is
    /// where it already stands or when it is terminal: a terminal outcome is
    /// history, never rewritten.
    pub(crate) fn moved(&mut self, state: State, reason: Option<String>, now: u64) -> bool {
        if self.state.terminal() || (self.state == state && self.reason == reason) {
            return false;
        }
        self.state = state;
        self.reason = reason;
        self.updated_at = now;
        true
    }

    pub(crate) fn status(&self, id: &Id) -> Status {
        Status {
            id: id.as_str().to_string(),
            state: self.state,
            reason: self.reason.clone(),
            admitted_at: self.admitted_at,
            updated_at: self.updated_at,
            key: self.key.clone(),
            key_name: self.key_name.to_string(),
        }
    }
}

/// What `submit` and `status` print: never the envelope, the signature or
/// the command.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Status {
    pub(crate) id: String,
    pub(crate) state: State,
    pub(crate) reason: Option<String>,
    pub(crate) admitted_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) key: String,
    pub(crate) key_name: String,
}

/// The longest record read back.
const RECORD_MAX: u64 = 256 * 1024;

/// A file's identity, so a rewrite (a repeat submit) is told from no change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    ino: u64,
    mtime_ns: i64,
}

/// The ledger rooted at one directory.
#[derive(Debug, Clone)]
pub(crate) struct Store {
    root: PathBuf,
}

impl Store {
    pub(crate) fn at(root: PathBuf) -> Self {
        Store { root }
    }

    /// `~/.clank/user-commands`.
    pub(crate) fn home() -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Ok(Store::in_home(Path::new(&home)))
    }

    /// `<home>/.clank/user-commands`.
    pub(crate) fn in_home(home: &Path) -> Self {
        Store::at(home.join(".clank").join("user-commands"))
    }

    /// The directory holding every label's entries for the repo tagged `tag`.
    pub(crate) fn repo_dir(&self, tag: &str) -> PathBuf {
        self.root.join(tag)
    }

    fn label_dir(&self, tag: &str, label: &AgentLabel) -> PathBuf {
        self.repo_dir(tag).join(label.as_str())
    }

    /// Create the store's directories down to `tag`, each private and vetted.
    pub(crate) fn ensure_repo(&self, tag: &str) -> anyhow::Result<PathBuf> {
        if let Some(parent) = self.root.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating `{}`", parent.display()))?;
        }
        for dir in [self.root.clone(), self.repo_dir(tag)] {
            private_dir(&dir)?;
        }
        Ok(self.repo_dir(tag))
    }

    fn ensure_label(&self, tag: &str, label: &AgentLabel) -> anyhow::Result<PathBuf> {
        self.ensure_repo(tag)?;
        let dir = self.label_dir(tag, label);
        private_dir(&dir)?;
        Ok(dir)
    }

    /// Vet every directory down to `label`'s; `None` when one is absent.
    fn vetted_label(&self, tag: &str, label: &AgentLabel) -> anyhow::Result<Option<PathBuf>> {
        for dir in [
            self.root.clone(),
            self.repo_dir(tag),
            self.label_dir(tag, label),
        ] {
            if !vet(&dir)? {
                return Ok(None);
            }
        }
        Ok(Some(self.label_dir(tag, label)))
    }

    /// Block until this process holds `id`'s lock, creating what it needs.
    pub(crate) fn lock(&self, tag: &str, label: &AgentLabel, id: &Id) -> anyhow::Result<Held> {
        let dir = self.ensure_label(tag, label)?;
        let file = open_lock(&dir, id)?;
        flock(&file, libc::LOCK_EX)?;
        Ok(Held {
            dir,
            id: id.clone(),
            _file: file,
        })
    }

    /// `id`'s lock if no one holds it now; `None` when someone does, or when
    /// its directories do not exist.
    pub(crate) fn try_lock(
        &self,
        tag: &str,
        label: &AgentLabel,
        id: &Id,
    ) -> anyhow::Result<Option<Held>> {
        let Some(dir) = self.vetted_label(tag, label)? else {
            return Ok(None);
        };
        let file = open_lock(&dir, id)?;
        match flock(&file, libc::LOCK_EX | libc::LOCK_NB) {
            Ok(()) => Ok(Some(Held {
                dir,
                id: id.clone(),
                _file: file,
            })),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error)
                    == Some(libc::EWOULDBLOCK) =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// `id`'s record as it stands, read without its lock (for `status`).
    pub(crate) fn read(
        &self,
        tag: &str,
        label: &AgentLabel,
        id: &Id,
    ) -> anyhow::Result<Option<Record>> {
        match self.vetted_label(tag, label)? {
            Some(dir) => read_record(&record_path(&dir, id)),
            None => Ok(None),
        }
    }

    /// The identity of `id`'s record file now.
    pub(crate) fn stamp(&self, tag: &str, label: &AgentLabel, id: &Id) -> Option<Stamp> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(record_path(&self.label_dir(tag, label), id)).ok()?;
        Some(Stamp {
            ino: md.ino(),
            mtime_ns: md.mtime() * 1_000_000_000 + md.mtime_nsec(),
        })
    }

    /// Every entry filed for the repo tagged `tag`, by the label and id its
    /// path names. Entries whose names are not a label and an id are not
    /// entries.
    pub(crate) fn list(&self, tag: &str) -> Vec<(AgentLabel, Id)> {
        let mut out = Vec::new();
        if !matches!(vet(&self.root), Ok(true)) || !matches!(vet(&self.repo_dir(tag)), Ok(true)) {
            return out;
        }
        let Ok(labels) = std::fs::read_dir(self.repo_dir(tag)) else {
            return out;
        };
        for entry in labels.flatten() {
            let Some(label) = entry
                .file_name()
                .to_str()
                .and_then(|n| AgentLabel::parse(n).ok())
            else {
                continue;
            };
            if !matches!(vet(&entry.path()), Ok(true)) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(entry.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name();
                let Some(id) = name
                    .to_str()
                    .and_then(|n| n.strip_suffix(".json"))
                    .and_then(|n| Id::parse(n).ok())
                else {
                    continue;
                };
                out.push((label.clone(), id));
            }
        }
        out.sort();
        out
    }
}

/// One id's lock, held. Records are loaded and saved only through it, so
/// no write of an entry happens without its lock.
pub(crate) struct Held {
    dir: PathBuf,
    id: Id,
    _file: std::fs::File,
}

impl Held {
    pub(crate) fn load(&self) -> anyhow::Result<Option<Record>> {
        read_record(&record_path(&self.dir, &self.id))
    }

    /// Replace the record atomically, synced to disk before this returns.
    pub(crate) fn save(&self, record: &Record) -> anyhow::Result<()> {
        use std::io::Write;
        let mut tmp = tempfile::Builder::new()
            .prefix(".uc-")
            .suffix(".tmp")
            .tempfile_in(&self.dir)?;
        tmp.write_all(&serde_json::to_vec(record)?)?;
        tmp.as_file_mut().sync_all()?;
        tmp.persist(record_path(&self.dir, &self.id))
            .map_err(|e| e.error)?;
        std::fs::File::open(&self.dir)?.sync_all()?;
        Ok(())
    }
}

fn record_path(dir: &Path, id: &Id) -> PathBuf {
    dir.join(format!("{}.json", id.as_str()))
}

fn open_lock(dir: &Path, id: &Id) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(format!("{}.lock", id.as_str()));
    // Never unlinked: replacing a locked inode would admit a second holder.
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("opening `{}`", path.display()))
}

fn flock(file: &std::fs::File, how: libc::c_int) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: valid owned fd; flock has no memory effects.
        if unsafe { libc::flock(file.as_raw_fd(), how) } == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
}

/// Whether `dir` is a private directory of ours: `Ok(false)` when absent,
/// an error when it is anything else.
fn vet(dir: &Path) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let me = unsafe { libc::getuid() };
    match super::super::monitor::trusted_dir(dir, me) {
        super::super::monitor::Trust::Absent => return Ok(false),
        super::super::monitor::Trust::Theirs(why) => bail!("{why}"),
        super::super::monitor::Trust::Ours => {}
    }
    let mode = std::fs::symlink_metadata(dir)?.mode() & 0o777;
    ensure!(
        mode & 0o077 == 0,
        "{} has mode {mode:o}; the user-command store must be 0700",
        dir.display()
    );
    Ok(true)
}

fn private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !vet(dir)? {
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating `{}`", dir.display())),
        }
        ensure!(vet(dir)?, "`{}` vanished as it was made", dir.display());
    }
    Ok(())
}

fn read_record(path: &Path) -> anyhow::Result<Option<Record>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening `{}`", path.display())),
    };
    let md = file.metadata()?;
    ensure!(
        md.is_file() && md.uid() == unsafe { libc::getuid() } && md.mode() & 0o077 == 0,
        "`{}` is not a private regular file of ours",
        path.display()
    );
    let mut bytes = Vec::new();
    file.take(RECORD_MAX + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= RECORD_MAX,
        "`{}` is oversized",
        path.display()
    );
    serde_json::from_slice(&bytes)
        .map(Some)
        .with_context(|| format!("reading `{}`", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn label() -> AgentLabel {
        AgentLabel::parse("claude").unwrap()
    }

    fn id(s: &str) -> Id {
        Id::parse(s).unwrap()
    }

    fn record() -> Record {
        Record::admitted(
            Envelope {
                request: "cmVx".into(),
                sig: "c2ln".into(),
            },
            Pin {
                name: KeyName::parse("site").unwrap(),
                fingerprint: "fp".into(),
            },
            10,
        )
    }

    #[test]
    fn records_round_trip_under_their_lock_and_are_private() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::at(home.path().join("uc"));
        let held = store.lock("tag", &label(), &id("r1")).unwrap();
        assert_eq!(held.load().unwrap(), None);
        held.save(&record()).unwrap();
        assert_eq!(held.load().unwrap(), Some(record()));
        drop(held);
        assert_eq!(
            store.read("tag", &label(), &id("r1")).unwrap(),
            Some(record())
        );
        assert_eq!(store.list("tag"), vec![(label(), id("r1"))]);
        for dir in [
            store.root.clone(),
            store.repo_dir("tag"),
            store.label_dir("tag", &label()),
        ] {
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", dir.display());
        }
        let file = store.label_dir("tag", &label()).join("r1.json");
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    /// A held lock keeps every other holder out; try_lock reports it.
    #[test]
    fn an_id_lock_excludes() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::at(home.path().join("uc"));
        let held = store.lock("tag", &label(), &id("r1")).unwrap();
        assert!(
            store
                .try_lock("tag", &label(), &id("r1"))
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .try_lock("tag", &label(), &id("r2"))
                .unwrap()
                .is_some()
        );
        drop(held);
        assert!(
            store
                .try_lock("tag", &label(), &id("r1"))
                .unwrap()
                .is_some()
        );
    }

    /// A directory opened up to others, or a record replaced by a symlink,
    /// is refused rather than read.
    #[test]
    fn a_loosened_or_linked_store_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let store = Store::at(home.path().join("uc"));
        store
            .lock("tag", &label(), &id("r1"))
            .unwrap()
            .save(&record())
            .unwrap();
        let dir = store.label_dir("tag", &label());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(store.read("tag", &label(), &id("r1")).is_err());
        assert!(store.list("tag").is_empty());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let elsewhere = home.path().join("planted.json");
        std::fs::write(&elsewhere, serde_json::to_vec(&record()).unwrap()).unwrap();
        std::fs::remove_file(dir.join("r1.json")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("r1.json")).unwrap();
        assert!(store.read("tag", &label(), &id("r1")).is_err());
    }

    /// A terminal record is never moved again, and an unchanged move reports
    /// no change, so nothing is rewritten for it.
    #[test]
    fn terminal_outcomes_are_history() {
        let mut r = record();
        assert!(r.moved(State::WaitingForIdle, Some("busy".into()), 11));
        assert!(!r.moved(State::WaitingForIdle, Some("busy".into()), 12));
        assert_eq!(r.updated_at, 11);
        assert!(r.moved(State::Delivered, None, 13));
        for state in [State::Refused, State::WaitingForIdle, State::Unconfirmed] {
            assert!(!r.moved(state, Some("key changed".into()), 14));
        }
        assert_eq!((r.state, r.updated_at), (State::Delivered, 13));
    }

    #[test]
    fn the_status_names_no_envelope() {
        let status = serde_json::to_string(&record().status(&id("r1"))).unwrap();
        assert!(
            !status.contains("cmVx") && !status.contains("c2ln"),
            "{status}"
        );
        assert!(status.contains("\"state\":\"admitted\""), "{status}");
    }
}
