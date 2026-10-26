//! Which agent labels clank owns panes for, per repo.
//!
//! A pane is named by its agent, and the reconciler only recognises a
//! name that is a candidate. The current roster cannot be the whole
//! candidate set: an agent removed from it would leave its pane
//! unrecognisable, and so never closed. This record keeps the labels
//! whose panes clank opened, adopted or was told about by `clank as`,
//! until a verified listing shows none left. It is lifecycle
//! bookkeeping, not launch provenance.
//!
//! The record lives in the runtime namespace, so it survives a TUI
//! restart but not a reboot, which zellij sessions do not survive either.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// This repo's ownership record.
pub(crate) fn path(repo: &Path) -> PathBuf {
    super::monitor::runtime_path(repo, "pane-owners.json")
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Record {
    version: u32,
    labels: BTreeSet<String>,
}

/// The labels the record holds; `None` when there is no record.
pub(crate) fn load(repo: &Path) -> anyhow::Result<Option<BTreeSet<String>>> {
    load_at(&path(repo))
}

pub(crate) fn load_at(record: &Path) -> anyhow::Result<Option<BTreeSet<String>>> {
    let bytes = match std::fs::read(record) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", record.display())),
    };
    let parsed: Record = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is not a pane-ownership record", record.display()))?;
    anyhow::ensure!(
        parsed.version == 1,
        "{} has unsupported version {}",
        record.display(),
        parsed.version
    );
    Ok(Some(parsed.labels))
}

/// Add `own` and drop `disown`, keeping every other label, as one
/// serialized read-modify-write. Returns the labels now recorded.
pub(crate) fn update(
    repo: &Path,
    own: &BTreeSet<String>,
    disown: &BTreeSet<String>,
) -> anyhow::Result<BTreeSet<String>> {
    super::monitor::ensure_runtime_dir().map_err(anyhow::Error::msg)?;
    update_at(&path(repo), own, disown)
}

pub(crate) fn update_at(
    record: &Path,
    own: &BTreeSet<String>,
    disown: &BTreeSet<String>,
) -> anyhow::Result<BTreeSet<String>> {
    update_with(record, |labels| {
        labels.extend(own.iter().cloned());
        labels.retain(|l| !disown.contains(l));
    })
}

/// [`update_at`] with the change supplied, run under the record's lock.
/// Atomic replacement alone keeps the file whole, not a concurrent
/// writer's change: two writers that both read before either wrote lose
/// one of the two. An unreadable record is an error, never overwritten.
fn update_with(
    record: &Path,
    change: impl FnOnce(&mut BTreeSet<String>),
) -> anyhow::Result<BTreeSet<String>> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let lock_path = record.with_extension("lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    // SAFETY: flock on a descriptor this function owns; released when
    // `lock` drops, after the write below.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("locking {}", lock_path.display()));
    }
    let before = load_at(record)?;
    let mut labels = before.clone().unwrap_or_default();
    change(&mut labels);
    if before.as_ref() == Some(&labels) {
        return Ok(labels);
    }
    let mut file = tempfile::NamedTempFile::new_in(
        record
            .parent()
            .context("pane-ownership record has no parent")?,
    )?;
    serde_json::to_writer(
        &mut file,
        &Record {
            version: 1,
            labels: labels.clone(),
        },
    )?;
    file.flush()?;
    file.as_file().sync_all()?;
    file.persist(record).map_err(|e| e.error)?;
    Ok(labels)
}

/// Every label a pane of `repo` may be identified as: the roster, the
/// owned labels, and `also`. A roster or record that cannot be read
/// contributes nothing, so nothing outside what is known is claimed.
pub(crate) fn candidates(repo: &Path, also: &str) -> BTreeSet<String> {
    let mut out = roster_labels(repo);
    out.extend(owned_labels(repo));
    out.insert(also.to_string());
    out
}

/// The labels the record holds; empty when there is none or it cannot be
/// read.
pub(crate) fn owned_labels(repo: &Path) -> BTreeSet<String> {
    load(repo).ok().flatten().unwrap_or_default()
}

/// The current roster's labels; empty when it cannot be resolved.
pub(crate) fn roster_labels(repo: &Path) -> BTreeSet<String> {
    let Ok(Some(set)) = crate::agent_store::try_resolve_via_team(repo) else {
        return BTreeSet::new();
    };
    std::iter::once(set.master.as_str().to_string())
        .chain(set.members.iter().map(|m| m.label.as_str().to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(labels: &[&str]) -> BTreeSet<String> {
        labels.iter().map(|l| l.to_string()).collect()
    }

    /// No record reads as `None`, distinct from an empty one; a change
    /// is kept beside every label it did not name.
    #[test]
    fn an_update_keeps_what_it_did_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("r.pane-owners.json");
        assert_eq!(load_at(&record).unwrap(), None);
        update_at(&record, &set(&["a", "b"]), &set(&[])).unwrap();
        update_at(&record, &set(&["c"]), &set(&["a"])).unwrap();
        assert_eq!(load_at(&record).unwrap(), Some(set(&["b", "c"])));
        update_at(&record, &set(&[]), &set(&["b", "c"])).unwrap();
        assert_eq!(load_at(&record).unwrap(), Some(set(&[])));
    }

    /// An unchanged record is not rewritten: its inode survives the update.
    #[test]
    fn an_update_that_changes_nothing_writes_nothing() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("r.pane-owners.json");
        update_at(&record, &set(&["a"]), &set(&[])).unwrap();
        let inode = std::fs::metadata(&record).unwrap().ino();
        update_at(&record, &set(&["a"]), &set(&["gone"])).unwrap();
        assert_eq!(std::fs::metadata(&record).unwrap().ino(), inode);
    }

    #[test]
    fn an_unreadable_record_is_an_error_and_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("r.pane-owners.json");
        std::fs::write(&record, "not json").unwrap();
        assert!(load_at(&record).is_err());
        assert!(update_at(&record, &set(&["a"]), &set(&[])).is_err());
        assert_eq!(std::fs::read_to_string(&record).unwrap(), "not json");
    }

    /// Writer A reads the record and, before writing, lets writer B run;
    /// A then waits up to half a second for B to finish. Serialized, B
    /// cannot read until A has written, so both labels survive. Without
    /// the lock B reads the empty record and finishes inside that wait,
    /// and A's write discards B's label.
    #[test]
    fn interleaved_writers_both_keep_their_labels() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("r.pane-owners.json");
        let (go, started) = std::sync::mpsc::channel::<()>();
        let (finished, done) = std::sync::mpsc::channel::<()>();
        let b = {
            let record = record.clone();
            std::thread::spawn(move || {
                started.recv().unwrap();
                update_at(&record, &set(&["b"]), &set(&[])).unwrap();
                let _ = finished.send(());
            })
        };
        update_with(&record, |labels| {
            labels.insert("a".to_string());
            go.send(()).unwrap();
            let _ = done.recv_timeout(std::time::Duration::from_millis(500));
        })
        .unwrap();
        b.join().unwrap();
        assert_eq!(load_at(&record).unwrap(), Some(set(&["a", "b"])));
    }
}
