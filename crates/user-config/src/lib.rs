//! `~/.clank/config.json`, the user's own config, and the one transaction
//! every writer of it goes through.
//!
//! Several programs own sections of the one document: the CLI's settings,
//! teams and trusted keys, the web door's `remote`. Each writer reads the
//! whole document and writes it back, so a writer outside [`transact`]
//! can put back what another just removed, such as a revoked key. An
//! atomic rename alone does not prevent that: it orders whole-file
//! replacements, not read-modify-write cycles.

use anyhow::Context;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub type Document = Map<String, Value>;

/// `<home>/.clank/config.json`.
pub fn path(home: &Path) -> PathBuf {
    home.join(".clank").join("config.json")
}

/// The document as it stands, empty when there is none. Readers take no
/// lock: every write replaces the file whole.
pub fn read(home: &Path) -> anyhow::Result<Document> {
    let path = path(home);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<Value>(&raw)
            .with_context(|| format!("parsing {}", path.display()))?
        {
            Value::Object(map) => Ok(map),
            _ => anyhow::bail!("`{}` is not a JSON object", path.display()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading {}", path.display()))),
    }
}

/// Read the document, let `f` check and change it, and replace it
/// atomically if it changed, all while holding `<home>/.clank/config.lock`,
/// so no other transaction runs between the read and the write. Nothing is
/// written when `f` fails. The lock file is never removed: replacing a
/// locked inode would admit a second holder. Holders take no other lock
/// while they hold it.
pub fn transact<T>(
    home: &Path,
    f: impl FnOnce(&mut Document) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let dir = home.join(".clank");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let _lock = lock(&dir.join("config.lock"))?;
    let before = read(home)?;
    after_read();
    let mut doc = before.clone();
    let out = f(&mut doc)?;
    if doc != before {
        replace(&path(home), &doc)?;
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

fn replace(path: &Path, doc: &Document) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(doc)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
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
