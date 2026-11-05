//! `~/.clank/config.json`, the user's own config, and the one transaction
//! every writer of it goes through.
//!
//! Several programs own sections of the one document: the CLI's settings,
//! teams and trusted keys, the web door's `remote`. Each writer reads the
//! whole document and writes it back, so a writer outside [`transact`]
//! can put back what another just removed, such as a revoked key. An
//! atomic rename alone does not prevent that: it orders whole-file
//! replacements, not read-modify-write cycles.

pub mod lease;
pub mod versioned;

use anyhow::Context;
use std::path::{Path, PathBuf};

pub use versioned::Document;

/// `<home>/.clank/config.json`.
pub fn path(home: &Path) -> PathBuf {
    home.join(".clank").join("config.json")
}

/// The newest version of this document this build reads and writes.
pub const VERSION: u64 = 1;

/// The document as it stands, empty when there is none. Readers take no
/// lock: every write replaces the file whole. A document this build
/// cannot read is an error carrying a [`versioned::Refusal`].
pub fn read(home: &Path) -> anyhow::Result<Document> {
    Ok(versioned::read(&path(home), VERSION)?.unwrap_or_default())
}

/// The schema a writer holds the whole document to: every setting it
/// knows must read, whichever section the writer edits. Unknown keys
/// pass.
pub type Check = fn(&Path, &Document) -> anyhow::Result<()>;

/// Read the document, let `f` check and change it, and replace it
/// atomically if it changed, all while holding `<home>/.clank/config.lock`,
/// so no other transaction runs between the read and the write. The
/// document is held to `check` before `f` sees it and again before it is
/// written, so a writer never publishes beside a setting no reader can
/// read. Nothing is written when `f` or either check fails. The lock file
/// is never removed: replacing a locked inode would admit a second
/// holder. Holders take no other lock while they hold it.
pub fn transact<T>(
    home: &Path,
    check: Check,
    f: impl FnOnce(&mut Document) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let dir = home.join(".clank");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = lock(&dir.join("config.lock"))?;
    let path = path(home);
    let before = read(home)?;
    check(&path, &before)?;
    after_read();
    let mut doc = before.clone();
    let out = f(&mut doc)?;
    if doc != before {
        check(&path, &doc)?;
        versioned::write(&path, &doc, VERSION)?;
    }
    Ok(out)
}

fn lock(path: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening `{}`", path.display()))?;
    loop {
        // SAFETY: valid owned fd; flock has no memory effects.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            return Ok(file);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).with_context(|| format!("locking `{}`", path.display()));
        }
    }
}

#[cfg(feature = "test-hooks")]
thread_local! {
    static AFTER_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `hook` on this thread the next time a transaction has read the
/// document and not yet written it, so a test can place another writer
/// exactly there.
#[cfg(feature = "test-hooks")]
pub fn on_next_read(hook: impl FnOnce() + 'static) {
    AFTER_READ.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
}

#[doc(hidden)]
pub fn after_read() {
    #[cfg(feature = "test-hooks")]
    if let Some(hook) = AFTER_READ.with(|h| h.borrow_mut().take()) {
        hook();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any(_: &Path, _: &Document) -> anyhow::Result<()> {
        Ok(())
    }

    fn auto_is_on_or_off(path: &Path, doc: &Document) -> anyhow::Result<()> {
        match doc.get("auto") {
            None => Ok(()),
            Some(v) if v == "on" || v == "off" => Ok(()),
            Some(v) => Err(versioned::Refusal::malformed(path, format!("auto is {v}")).into()),
        }
    }

    #[test]
    fn a_document_its_check_refuses_is_never_edited_and_an_edit_it_refuses_is_never_written() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".clank")).unwrap();
        let body = "{\"auto\": \"yes\", \"diff\": {}}";
        std::fs::write(path(home.path()), body).unwrap();
        let ran = std::cell::Cell::new(false);
        assert!(
            transact(home.path(), auto_is_on_or_off, |_| {
                ran.set(true);
                Ok(())
            })
            .is_err()
        );
        assert!(!ran.get(), "the edit ran on a document its check refuses");
        assert_eq!(std::fs::read_to_string(path(home.path())).unwrap(), body);

        let body = "{\"auto\": \"on\"}";
        std::fs::write(path(home.path()), body).unwrap();
        assert!(
            transact(home.path(), auto_is_on_or_off, |doc| {
                doc.insert("auto".into(), "maybe".into());
                Ok(())
            })
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(path(home.path())).unwrap(), body);
    }

    #[test]
    fn a_document_it_cannot_read_is_never_overwritten() {
        for body in ["{\"teams\": {", "{\"version\": 2, \"auto\": \"on\"}"] {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(home.path().join(".clank")).unwrap();
            std::fs::write(path(home.path()), body).unwrap();
            let ran = std::cell::Cell::new(false);
            let err = transact(home.path(), any, |doc| {
                ran.set(true);
                doc.insert("auto".into(), "off".into());
                Ok(())
            })
            .unwrap_err();
            assert!(err.downcast_ref::<versioned::Refusal>().is_some(), "{err}");
            assert!(!ran.get(), "the edit ran on `{body}`");
            assert_eq!(std::fs::read_to_string(path(home.path())).unwrap(), body);
        }
    }

    #[test]
    fn a_missing_document_is_created_versioned() {
        let home = tempfile::tempdir().unwrap();
        transact(home.path(), any, |doc| {
            doc.insert("auto".into(), "on".into());
            Ok(())
        })
        .unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path(home.path())).unwrap()).unwrap();
        assert_eq!(raw, serde_json::json!({"version": 1, "auto": "on"}));
    }
}
