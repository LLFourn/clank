//! How a clank config document is read and written: one JSON object
//! carrying a schema `version`.
//!
//! A file with no `version` is today's shape, written before the field
//! existed, and reads as version 1 unchanged. A version this build does
//! not know is refused rather than half-read: an older clank applying the
//! settings it understands, or writing back the ones it kept, would
//! silently undo what the newer one wrote.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub type Document = Map<String, Value>;

/// The key the version is stored under. It is the document's envelope,
/// not one of its settings, so [`read`] strips it and [`write`] adds it.
pub const KEY: &str = "version";

/// Why a document that exists cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// Not JSON, not an object, a `version` that is not a positive
    /// integer, or a shape its reader does not accept.
    Malformed { path: PathBuf, why: String },
    /// Written by a newer clank.
    TooNew {
        path: PathBuf,
        version: u64,
        newest: u64,
    },
}

impl Refusal {
    pub fn malformed(path: &Path, why: impl std::fmt::Display) -> Self {
        Refusal::Malformed {
            path: path.to_path_buf(),
            why: why.to_string(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Malformed { path, why } => {
                write!(f, "`{}` is malformed: {why}", path.display())
            }
            Refusal::TooNew {
                path,
                version,
                newest,
            } => write!(
                f,
                "`{}` was written by a newer clank (version {version}; this build reads up to \
                 {newest}); upgrade clank to use it",
                path.display()
            ),
        }
    }
}

impl std::error::Error for Refusal {}

/// A document as stored: its contents without the version, and whether
/// it carried one. A document with a version was written by a clank that
/// stamps it, so it can never be an older, unversioned shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    pub doc: Document,
    pub versioned: bool,
}

/// The document at `path` without its version, `None` when there is no
/// file. A document newer than `newest`, or one that is not a versioned
/// JSON object, is an error carrying a [`Refusal`].
pub fn read(path: &Path, newest: u64) -> anyhow::Result<Option<Document>> {
    Ok(read_stored(path, newest)?.map(|s| s.doc))
}

/// [`read`], keeping whether the document carried a version.
pub fn read_stored(path: &Path, newest: u64) -> anyhow::Result<Option<Stored>> {
    match std::fs::read(path) {
        Ok(raw) => Ok(Some(parse_stored(path, &raw, newest)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::from(e).context(format!("reading `{}`", path.display()))),
    }
}

/// [`read`] of bytes already in hand; `path` names them in the error.
pub fn parse(path: &Path, raw: &[u8], newest: u64) -> Result<Document, Refusal> {
    parse_stored(path, raw, newest).map(|s| s.doc)
}

fn parse_stored(path: &Path, raw: &[u8], newest: u64) -> Result<Stored, Refusal> {
    let mut doc = match serde_json::from_slice::<Value>(raw) {
        Ok(Value::Object(doc)) => doc,
        Ok(_) => return Err(Refusal::malformed(path, "not a JSON object")),
        Err(e) => return Err(Refusal::malformed(path, e)),
    };
    let versioned = doc.contains_key(KEY);
    let version = match doc.remove(KEY) {
        None => 1,
        Some(v) => match v.as_u64() {
            Some(n) if n >= 1 => n,
            _ => {
                return Err(Refusal::malformed(
                    path,
                    format!("`{KEY}` is {v}, not a positive integer"),
                ));
            }
        },
    };
    if version > newest {
        return Err(Refusal::TooNew {
            path: path.to_path_buf(),
            version,
            newest,
        });
    }
    Ok(Stored { doc, versioned })
}

/// Replace `path` with `doc` stamped as `version`: written whole to a
/// synced temp file beside it and renamed over it, so a reader sees the
/// old document or the new one, never part of either.
pub fn write(path: &Path, doc: &Document, version: u64) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent for `{}`", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let mut stamped = Map::new();
    stamped.insert(KEY.into(), Value::from(version));
    stamped.extend(
        doc.iter()
            .filter(|(k, _)| *k != KEY)
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    let mut tmp = tempfile::Builder::new()
        .prefix(".clank-config-")
        .suffix(".json.tmp")
        .tempfile_in(parent)?;
    tmp.write_all(serde_json::to_string_pretty(&stamped)?.as_bytes())?;
    tmp.write_all(b"\n")?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(body: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    fn refusal(err: anyhow::Error) -> Refusal {
        err.downcast::<Refusal>().expect("a Refusal")
    }

    #[test]
    fn a_missing_document_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(&dir.path().join("config.json"), 1).unwrap(), None);
    }

    #[test]
    fn malformed_json_is_refused_naming_the_file() {
        let (_dir, path) = at("{\"a\": 1,");
        let err = read(&path, 1).unwrap_err();
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "{err}"
        );
        assert!(matches!(refusal(err), Refusal::Malformed { .. }));
    }

    #[test]
    fn a_document_that_is_not_an_object_is_refused() {
        let (_dir, path) = at("[1, 2]");
        assert!(matches!(
            refusal(read(&path, 1).unwrap_err()),
            Refusal::Malformed { .. }
        ));
    }

    #[test]
    fn no_version_reads_as_version_one_unchanged() {
        let (_dir, path) = at("{\"a\": {\"b\": [1, \"two\"]}}");
        let doc = read(&path, 1).unwrap().unwrap();
        assert_eq!(
            Value::Object(doc),
            serde_json::json!({"a": {"b": [1, "two"]}})
        );
    }

    #[test]
    fn version_one_reads_without_its_version() {
        let (_dir, path) = at("{\"version\": 1, \"a\": true}");
        let doc = read(&path, 1).unwrap().unwrap();
        assert_eq!(Value::Object(doc), serde_json::json!({"a": true}));
    }

    #[test]
    fn a_future_version_is_refused_as_too_new() {
        let (_dir, path) = at("{\"version\": 3, \"a\": true}");
        let err = read(&path, 2).unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("written by a newer clank (version 3; this build reads up to 2)"),
            "{shown}"
        );
        assert_eq!(
            refusal(err),
            Refusal::TooNew {
                path: path.clone(),
                version: 3,
                newest: 2
            }
        );
    }

    #[test]
    fn a_version_that_is_not_a_positive_integer_is_malformed() {
        for bad in ["\"x\"", "0", "-1", "1.5", "null"] {
            let (_dir, path) = at(&format!("{{\"version\": {bad}}}"));
            let err = read(&path, 1).unwrap_err();
            assert!(err.to_string().contains("`version`"), "{bad}: {err}");
            assert!(
                matches!(refusal(err), Refusal::Malformed { .. }),
                "{bad} must be malformed"
            );
        }
    }

    #[test]
    fn a_stored_document_says_whether_it_carried_a_version() {
        let (_dir, bare) = at("{\"a\": 1}");
        let (_dir2, stamped) = at("{\"version\": 1, \"a\": 1}");
        assert!(!read_stored(&bare, 1).unwrap().unwrap().versioned);
        assert!(read_stored(&stamped, 1).unwrap().unwrap().versioned);
    }

    #[test]
    fn a_write_stores_the_version_and_reads_back_the_same_document() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.json");
        let doc = match serde_json::json!({"a": 1, "version": 9}) {
            Value::Object(m) => m,
            _ => unreachable!(),
        };
        write(&path, &doc, 1).unwrap();
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw, serde_json::json!({"version": 1, "a": 1}));
        assert_eq!(
            Value::Object(read(&path, 1).unwrap().unwrap()),
            serde_json::json!({"a": 1})
        );
    }
}
