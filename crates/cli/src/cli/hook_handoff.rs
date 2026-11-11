//! What a parked Stop hook hands the installed build it becomes.
//!
//! `exec` keeps the pid, the environment and the harness's pipes, so the
//! harness sees one hook throughout. What it cannot keep is what the old
//! image held only in memory: the hook input, already read off stdin to
//! its end, and the report the park was armed with, whose evidence was
//! read when the Stop arrived and could not be read again later. Those
//! cross in a file named in the new image's arguments.
//!
//! The file is written by one build and read by another, so its bytes are
//! a format: `tests/fixtures/formats/hook-handoff.json`.

use std::path::{Path, PathBuf};

use super::turn_wire::Report;

const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Handoff {
    v: u32,
    /// The hook input, exactly as the harness wrote it on stdin.
    pub(crate) input: String,
    /// The report the park was armed with. Its deadline is the hook's
    /// own: the installed build holds the wait no longer than this one
    /// would have.
    pub(crate) report: Report,
}

impl Handoff {
    pub(crate) fn new(input: String, report: Report) -> Self {
        Handoff {
            v: VERSION,
            input,
            report,
        }
    }

    /// Write this where only this user can read it, run `exec` on its
    /// path, and remove it again if `exec` came back: a replacement that
    /// failed leaves nothing behind.
    pub(crate) fn hand_to(
        &self,
        dir: &Path,
        exec: impl FnOnce(&Path) -> std::io::Error,
    ) -> std::io::Error {
        match serde_json::to_vec(self) {
            Ok(bytes) => hand_over(dir, "hook-handoff", &bytes, exec),
            Err(e) => std::io::Error::other(e),
        }
    }

    /// Read the handoff a replaced build left at `path`, and remove it.
    /// A file that is not a handoff is left where it is: the path is an
    /// argument, and an argument can name anything.
    pub(crate) fn take(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|e| format!("hook: reading its handoff {}: {e}", path.display()))?;
        let handoff = Self::parse(&bytes)?;
        let _ = std::fs::remove_file(path);
        Ok(handoff)
    }

    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        let handoff: Handoff = serde_json::from_slice(bytes)
            .map_err(|e| format!("hook: its handoff did not parse: {e}"))?;
        match handoff.v {
            VERSION => Ok(handoff),
            other => Err(format!(
                "hook: its handoff is format {other}, and this build reads {VERSION}"
            )),
        }
    }
}

/// Write `bytes` to a file named for `stem` in `dir` that only this user
/// can read, run `exec` on its path, and remove it again if `exec` came
/// back: a replacement that failed leaves nothing behind.
pub(crate) fn hand_over(
    dir: &Path,
    stem: &str,
    bytes: &[u8],
    exec: impl FnOnce(&Path) -> std::io::Error,
) -> std::io::Error {
    let path = match write_private(dir, stem, bytes) {
        Ok(path) => path,
        Err(e) => return e,
    };
    let failed = exec(&path);
    let _ = std::fs::remove_file(&path);
    failed
}

fn write_private(dir: &Path, stem: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // The pid is kept across the exec, so the name is this process's
    // alone, and an attempt that failed is overwritten by the next.
    let path = dir.join(format!("{stem}-{}.json", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// The arguments the installed build is started with: the ones this
/// process was, with `handoff` in place of any it was itself resumed from.
pub(crate) fn resumed_args(
    started_with: impl IntoIterator<Item = std::ffi::OsString>,
    handoff: &Path,
) -> Vec<std::ffi::OsString> {
    let mut out = Vec::new();
    let mut args = started_with.into_iter();
    while let Some(arg) = args.next() {
        if arg == FLAG {
            args.next();
        } else {
            out.push(arg);
        }
    }
    out.push(FLAG.into());
    out.push(handoff.into());
    out
}

pub(crate) const FLAG: &str = "--handoff";

#[cfg(test)]
mod tests {
    use super::*;

    fn held() -> Handoff {
        Handoff::new(
            r#"{"session_id":"session-held","cwd":"/w"}"#.into(),
            Report::Compacted {
                stop: clank_core::turn_policy::TurnEndReport {
                    tool: clank_core::Tool::Claude,
                    session: clank_core::SessionId::parse("session-held").unwrap(),
                    place: Default::default(),
                    tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
                    hook_started: None,
                },
                deadline: 42,
            },
        )
    }

    /// What the old image held reaches the new one whole, in a file only
    /// this user can read, and the new one leaves no file behind.
    #[test]
    fn a_handoff_is_read_back_whole_and_then_gone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let handoff = held();
        let mut taken = None;
        handoff.hand_to(dir.path(), |path| {
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            taken = Some(Handoff::take(path));
            assert!(!path.exists(), "the reader removes what it read");
            std::io::Error::other("no exec in a test")
        });
        assert_eq!(taken, Some(Ok(handoff)));
    }

    /// A replacement that failed leaves no handoff behind, and the next
    /// attempt writes it afresh.
    #[test]
    fn a_failed_replacement_leaves_no_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let handoff = held();
        for _ in 0..2 {
            let mut seen = None;
            let failed = handoff.hand_to(dir.path(), |path| {
                seen = Some((
                    path.to_path_buf(),
                    Handoff::parse(&std::fs::read(path).unwrap()),
                ));
                std::io::Error::other("the binary is missing")
            });
            assert_eq!(failed.to_string(), "the binary is missing");
            let (path, read) = seen.unwrap();
            assert_eq!(read, Ok(handoff.clone()));
            assert!(!path.exists());
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let nowhere = handoff.hand_to(&dir.path().join("missing"), |_| {
            panic!("nothing was written to hand over")
        });
        assert_eq!(nowhere.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn a_handoff_of_another_format_is_refused_by_name() {
        let mut other = serde_json::to_value(held()).unwrap();
        other["v"] = 2.into();
        let why = Handoff::parse(other.to_string().as_bytes()).unwrap_err();
        assert!(why.contains("format 2"), "{why}");
        assert!(Handoff::parse(b"not json").is_err());

        let dir = tempfile::tempdir().unwrap();
        let not_one = dir.path().join("somebody-elses-file");
        std::fs::write(&not_one, "not a handoff").unwrap();
        assert!(Handoff::take(&not_one).is_err());
        assert!(not_one.exists(), "a file that is not a handoff was removed");
    }

    /// The installed build is started as this hook was, resuming from the
    /// newest handoff only: one a second install hands over replaces the
    /// first's.
    #[test]
    fn the_installed_build_is_started_as_this_one_was_with_one_handoff() {
        let args = |list: &[&str]| {
            list.iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        let first = resumed_args(
            args(&["stop-hook", "--tool", "claude", "--repo", "/w"]),
            Path::new("/run/a.json"),
        );
        assert_eq!(
            first,
            args(&[
                "stop-hook",
                "--tool",
                "claude",
                "--repo",
                "/w",
                FLAG,
                "/run/a.json"
            ])
        );
        assert_eq!(
            resumed_args(first, Path::new("/run/b.json")),
            args(&[
                "stop-hook",
                "--tool",
                "claude",
                "--repo",
                "/w",
                FLAG,
                "/run/b.json"
            ])
        );
    }
}
