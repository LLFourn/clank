//! `<repo>/.clank/config.json`: the repo's roster and settings, and the
//! one transaction every writer of it goes through.
//!
//! Every reader and every read-modify-write comes here, so a document
//! this build cannot read is refused the same way everywhere and nothing
//! writes over it. Writers hold `.clank/config.lock` from the read to the
//! rename: two writers that each read the whole file and wrote it back
//! put back whatever the other had just changed.

use std::path::{Path, PathBuf};

use clank_user_config::versioned::{self, Document, Refusal, Stored};
use serde_json::Value;

use crate::cli::teams_config::RepoConfigFile;

/// The newest version of this document this build reads and writes.
pub const VERSION: u64 = 1;

pub fn path(repo: &Path) -> PathBuf {
    repo.join(".clank").join("config.json")
}

/// The stored config, `None` when there is none. A legacy-shaped,
/// malformed or newer document is an error.
pub fn read(repo: &Path) -> anyhow::Result<Option<RepoConfigFile>> {
    let path = path(repo);
    match shape(&path, versioned::read_stored(&path, VERSION)?)? {
        Prior::Absent => Ok(None),
        Prior::Legacy => Err(legacy_schema_error(&path)),
        Prior::Valid(cfg) => Ok(Some(*cfg)),
    }
}

/// The stored document, checked as [`read`] checks it, for a reader
/// that takes its own view of the settings.
pub fn read_document(repo: &Path) -> anyhow::Result<Option<Document>> {
    let path = path(repo);
    let stored = versioned::read_stored(&path, VERSION)?;
    let doc = stored.as_ref().map(|s| s.doc.clone());
    match shape(&path, stored)? {
        Prior::Legacy => Err(legacy_schema_error(&path)),
        Prior::Absent | Prior::Valid(_) => Ok(doc),
    }
}

/// What the file holds, legacy shape included, for `clank init`, which
/// recreates a legacy config rather than refusing it.
pub(crate) fn read_prior(repo: &Path) -> anyhow::Result<Prior> {
    let path = path(repo);
    shape(&path, versioned::read_stored(&path, VERSION)?)
}

/// What the file held when a [`replace`] read it.
pub(crate) enum Prior {
    Absent,
    /// The pre-roster team schema, which `clank init` recreates.
    Legacy,
    Valid(Box<RepoConfigFile>),
}

/// Edit the config under the lock, starting from an empty one when
/// there is none, and write it if `f` succeeds and it changed or did not
/// exist. A document this build cannot read is never handed to `f`.
pub fn transact<T>(
    repo: &Path,
    f: impl FnOnce(&mut RepoConfigFile) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = path(repo);
    replace(repo, |prior| {
        let mut cfg = match prior {
            Prior::Absent => RepoConfigFile::default(),
            Prior::Legacy => return Err(legacy_schema_error(&path)),
            Prior::Valid(cfg) => *cfg,
        };
        let out = f(&mut cfg)?;
        Ok((Some(cfg), out))
    })
}

/// [`transact`] on the raw document, for an edit spelled as a JSON path.
/// The edited document must still read as a config.
pub fn transact_document<T>(
    repo: &Path,
    f: impl FnOnce(&mut Document) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let path = path(repo);
    locked(repo, |stored| {
        let before = stored.as_ref().map(|s| s.doc.clone());
        if let Prior::Legacy = shape(&path, stored)? {
            return Err(legacy_schema_error(&path));
        }
        let mut doc = before.clone().unwrap_or_default();
        let out = f(&mut doc)?;
        let changed = before.as_ref() != Some(&doc);
        Ok((changed.then_some(doc), out))
    })
}

/// The general transaction: `f` sees what the file held, legacy shape
/// included, and returns the config to store, or `None` to leave it.
pub(crate) fn replace<T>(
    repo: &Path,
    f: impl FnOnce(Prior) -> anyhow::Result<(Option<RepoConfigFile>, T)>,
) -> anyhow::Result<T> {
    let path = path(repo);
    locked(repo, |before| {
        let prior = shape(&path, before)?;
        let kept = match &prior {
            Prior::Valid(cfg) => Some(document(cfg)?),
            Prior::Absent | Prior::Legacy => None,
        };
        let (after, out) = f(prior)?;
        let write = match after {
            Some(cfg) => {
                let doc = document(&cfg)?;
                (kept.as_ref() != Some(&doc)).then_some(doc)
            }
            None => None,
        };
        Ok((write, out))
    })
}

fn locked<T>(
    repo: &Path,
    f: impl FnOnce(Option<Stored>) -> anyhow::Result<(Option<Document>, T)>,
) -> anyhow::Result<T> {
    let _permit = crate::upgrade::writing()?;
    let path = path(repo);
    let dir = repo.join(".clank");
    std::fs::create_dir_all(&dir)?;
    let _lock = super::exclusive_lock(&dir.join("config.lock"))?;
    let (write, out) = f(versioned::read_stored(&path, VERSION)?)?;
    if let Some(doc) = write {
        // Every edit, typed or raw, must leave what the reader accepts, as
        // a current document since the write stamps its version: the typed
        // roster carries the settings it does not own, legacy markers
        // included, in `extra` unchecked.
        shape(
            &path,
            Some(Stored {
                doc: doc.clone(),
                versioned: true,
            }),
        )?;
        versioned::write(&path, &doc, VERSION)?;
    }
    Ok(out)
}

/// Only an unversioned document can be a legacy one: every clank that
/// stamps a version writes the roster schema, so a versioned document
/// in a legacy shape is a damaged current one, refused rather than
/// recreated.
fn shape(path: &Path, stored: Option<Stored>) -> anyhow::Result<Prior> {
    Ok(match stored {
        None => Prior::Absent,
        Some(Stored { doc, versioned }) if is_legacy(&doc) => {
            if versioned {
                return Err(Refusal::malformed(
                    path,
                    "it carries a version but uses the old team schema",
                )
                .into());
            }
            Prior::Legacy
        }
        Some(Stored { doc, .. }) => Prior::Valid(Box::new(typed(path, doc)?)),
    })
}

/// The document as a config, its known settings checked too: the typed
/// roster keeps the settings it does not own in `extra` unread.
fn typed(path: &Path, doc: Document) -> Result<RepoConfigFile, Refusal> {
    crate::cli::config::check_layer(path, &doc)?;
    serde_json::from_value(Value::Object(doc)).map_err(|e| Refusal::malformed(path, e))
}

fn document(cfg: &RepoConfigFile) -> anyhow::Result<Document> {
    match serde_json::to_value(cfg)? {
        Value::Object(doc) => Ok(doc),
        _ => anyhow::bail!("the repo config did not serialize as an object"),
    }
}

/// The pre-roster schemas: a `team` or `promoted` key, an `agents`
/// array, or an `agents` map in which no description has a role. Checked
/// on the document rather than the typed config, which would take `team`
/// and `promoted` into `extra` and read on. A map with some roles is not
/// one of them: it is a roster with a damaged entry.
fn is_legacy(doc: &Document) -> bool {
    doc.contains_key("promoted")
        || doc.contains_key("team")
        || match doc.get("agents") {
            Some(Value::Array(_)) => true,
            Some(Value::Object(agents)) => {
                !agents.is_empty()
                    && agents
                        .values()
                        .all(|agent| agent.as_object().is_some_and(|a| !a.contains_key("role")))
            }
            _ => false,
        }
}

pub(crate) fn legacy_schema_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "{}: this repo's `.clank/config.json` uses the old team schema; \
         re-run `clank init` to recreate it",
        path.display()
    )
}

/// Replace the stored config with `cfg`, as a test's setup.
#[cfg(test)]
pub(crate) fn store(repo: &Path, cfg: &RepoConfigFile) {
    transact(repo, |stored| {
        *stored = cfg.clone();
        Ok(())
    })
    .unwrap();
}
