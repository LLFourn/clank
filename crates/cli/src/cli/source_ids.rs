//! Persistent ingestion identities shared by command and native timer sources.

/// Every producer id this source has already turned into an event.
///
/// ONLY an index. The event itself — payload, acked state, the whole
/// lifecycle — lives in the same `EventLog` github uses, so
/// `clank events list/show/ack` finds a command event by the same
/// directory glob and an unacked one is never compacted away. Writing
/// a parallel store was how an earlier version produced events that
/// could be woken on and never handled (codex a43bd85).
///
/// TWO TIERS, because the log and the index can disagree and the log
/// is the authority:
///
/// - `warm` holds what the WAL still vouches for, recovered from its
///   retained identities at arm time, plus everything ingested since.
/// - the directory holds one file per id, unbounded, which is what
///   survives past the WAL's `LOG_IDENTITY_CAP`.
///
/// Neither alone is enough. The file tier is best-effort — a failed
/// write must not let a replay append a second row — and the warm
/// tier cannot outlive the arm. A crash between the log append and
/// the index write leaves an id the WAL knows and the directory does
/// not, and [`Seen::recover`] is what repairs that WITHOUT minting a
/// second event (codex 43f5ed8).
pub(crate) struct Seen {
    dir: std::path::PathBuf,
    warm: std::collections::HashSet<String>,
}

/// What an id's index entry says.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Known {
    /// Never ingested.
    No,
    /// Ingested; its event is in the log.
    Yes,
    /// The entry exists but cannot be read. Treated as NOT seen, so a
    /// replay repairs it: the alternative is what an earlier version
    /// did — suppress the producer's valid replay on behalf of a
    /// record nobody can read, losing the event permanently and
    /// silently (codex a43bd85).
    Damaged(String),
}

impl Seen {
    pub(crate) fn at(events_dir: &std::path::Path, source_key: &str) -> Self {
        Self {
            dir: events_dir.join(format!("{source_key}.seen")),
            warm: std::collections::HashSet::new(),
        }
    }

    /// Reconcile against the WAL's own retained identities.
    ///
    /// The log is authoritative: an id it still accounts for HAS an
    /// event, whatever the derived index says. Called once per arm,
    /// before anything is ingested, so the crash gap between an
    /// append and its index write is closed by the next arm rather
    /// than duplicating the event.
    ///
    /// Repairs the durable tier too, best-effort — a failure there
    /// costs nothing this arm, because `warm` already carries it.
    pub(crate) fn recover(&mut self, ids: impl IntoIterator<Item = String>) {
        for id in ids {
            if self.known(&id) != Known::Yes {
                let _ = self.write(&id);
            }
            self.warm.insert(id);
        }
    }

    /// Persist every identity vouched for by the WAL before its evidence can
    /// be compacted. Unlike best-effort recovery, warm memory is never proof
    /// of a successful durable checkpoint. Keep command recovery unchanged.
    pub(crate) fn checkpoint(
        &mut self,
        ids: impl IntoIterator<Item = String>,
    ) -> std::io::Result<()> {
        let mut files = std::collections::HashMap::new();
        for id in ids {
            let path = self.file(&id);
            if let Some(other) = files.insert(path, id.clone())
                && other != id
            {
                return Err(std::io::Error::other("source ID hash collision"));
            }
            self.warm.insert(id.clone());
            if self.durable(&id) != Known::Yes {
                self.write(&id)?;
            }
        }
        Ok(())
    }

    /// Hashed, because an id is arbitrary producer text and a
    /// filename is not: it may be long, contain separators, or differ
    /// from another id only in case on a case-folding filesystem.
    fn file(&self, id: &str) -> std::path::PathBuf {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in id.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.dir.join(format!("{h:016x}"))
    }

    /// The entry holds the id VERBATIM, so a read can tell "this id"
    /// from a hash collision, and a damaged entry from a missing one.
    pub(crate) fn known(&self, id: &str) -> Known {
        if self.warm.contains(id) {
            return Known::Yes;
        }
        self.durable(id)
    }

    fn durable(&self, id: &str) -> Known {
        let path = self.file(id);
        match std::fs::read_to_string(&path) {
            Ok(text) if text == id => Known::Yes,
            Ok(other) => Known::Damaged(format!(
                "{} holds `{}`, not `{id}`",
                path.display(),
                other.chars().take(40).collect::<String>()
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Known::No,
            Err(e) => Known::Damaged(format!("{}: {e}", path.display())),
        }
    }

    /// Remember `id`. The warm tier is updated even when the durable
    /// write fails, so a disk problem costs a duplicate at the NEXT
    /// arm at worst — never one in this one.
    pub(crate) fn record(&mut self, id: &str) -> std::io::Result<()> {
        self.warm.insert(id.to_string());
        self.write(id)
    }

    /// Discard everything — the log this index describes is gone.
    ///
    /// Only for quarantine. The index is derived, so a log moved
    /// aside leaves it describing events that no longer exist, and
    /// an entry that outlives its event refuses the producer's
    /// replay of it (codex 072e459). Forgetting is the safe
    /// direction: at worst a backend that replays old ids produces
    /// duplicates, which is the trade this whole plan already makes.
    pub(crate) fn forget_all(&mut self) -> std::io::Result<()> {
        self.warm.clear();
        match std::fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }

    fn write(&self, id: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.file(id);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, id.as_bytes())?;
        std::fs::rename(&tmp, &path)
    }
}
