//! Who has this repo's status open.
//!
//! One status TUI per repo, decided when the process starts rather
//! than asked per action. A second one does not run: it says where the
//! first is and exits, which is the whole difference between a
//! duplicate that quietly does nothing for nine days and one you close
//! in ten seconds (nothing-refuses-in-silence).

use std::path::Path;

/// The lease, held for the life of the process. Dropping it — including
/// by dying — frees it: flock needs no cleanup protocol, so there is no
/// stale-holder state to reason about.
#[derive(Debug)]
pub(super) struct StatusLease {
    _file: std::fs::File,
}

/// Who holds it, for the refusal message. Best-effort: written after
/// the lock is taken, read back without it.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
pub(super) struct Holder {
    pub(super) pid: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pane: Option<String>,
    pub(super) at: String,
}

impl Holder {
    fn here() -> Self {
        Self {
            pid: std::process::id() as i32,
            session: std::env::var("ZELLIJ_SESSION_NAME").ok(),
            pane: std::env::var("ZELLIJ_PANE_ID").ok(),
            at: crate::cli::stop_hook::now_rfc3339(),
        }
    }

    /// Where to go and close it.
    fn describe(&self) -> String {
        let mut out = format!("pid {}", self.pid);
        if let Some(session) = &self.session {
            out.push_str(&format!(" in session {session}"));
            if let Some(pane) = &self.pane {
                out.push_str(&format!(" (pane {pane})"));
            }
        }
        out.push_str(&format!(" since {}", self.at));
        out
    }
}

fn lock_path(repo: &Path) -> std::path::PathBuf {
    repo.join(".clank").join("status-tui.lock")
}

/// Take the repo's status lease, or report who has it.
///
/// The holder file is written INSIDE the lock, so what a refused
/// caller reads back is either the current holder's record or nothing
/// worth trusting — and a record it cannot read still refuses. The
/// lock is the gate; the file is only how the refusal names a place.
pub(super) fn acquire(repo: &Path) -> Result<StatusLease, String> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let path = lock_path(repo);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
    {
        Ok(f) => f,
        // Fail CLOSED, and say why. A TUI that cannot take the lease
        // cannot know whether another one holds it, and running
        // anyway is the state this whole plan exists to end — two
        // drivers, neither aware of the other. The message names the
        // path and the cause because this is the one refusal a user
        // cannot diagnose from the outside (codex on 9929ad5).
        Err(e) => {
            return Err(format!(
                "cannot take this repo's status lease at {}: {e}. \
                 One `clank status --tui` per repo is decided by that file; \
                 without it clank cannot tell whether another one is already \
                 driving this repo's panes.",
                path.display()
            ));
        }
    };
    // SAFETY: valid owned fd; flock has no memory effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(busy_message(read_holder(&path)));
    }
    let mut file = file;
    let _ = file.set_len(0);
    if let Ok(body) = serde_json::to_vec(&Holder::here()) {
        let _ = file.write_all(&body);
        let _ = file.flush();
    }
    Ok(StatusLease { _file: file })
}

fn read_holder(path: &Path) -> Option<Holder> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The refusal, with the holder named when one could be read.
pub(super) fn busy_message(holder: Option<Holder>) -> String {
    match holder {
        Some(h) => format!(
            "this repo's status is already open in another `clank status --tui`: {}. \
             Close that one, or use it — two of them cannot both drive the panes.",
            h.describe()
        ),
        None => "this repo's status is already open in another `clank status --tui` \
                 (its holder record is unreadable). Close it, or use it — two of them \
                 cannot both drive the panes."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
        dir
    }

    /// One TUI per repo, and the second is told where the first is.
    /// Death frees it with no cleanup protocol — the property that
    /// makes a holder record safe to trust as a hint and never as a
    /// gate.
    #[test]
    fn one_holder_per_repo_and_the_lease_dies_with_it() {
        let dir = repo();
        let first = acquire(dir.path()).expect("the first one takes it");
        let refused = acquire(dir.path()).expect_err("the second does not");
        assert!(
            refused.contains(&format!("pid {}", std::process::id())),
            "names the holder: {refused}"
        );
        assert!(refused.contains("Close that one"), "says what to do");

        drop(first);
        // Another test's child process, forked but not yet exec'd,
        // holds a copy of the closed description and with it the
        // lock, for the microseconds until exec closes it (CLOEXEC):
        // twice a flake under a loaded full suite. The property is
        // that closing frees it, not that it is free on the very next
        // instruction.
        let freed = (0..200).any(|_| {
            acquire(dir.path()).is_ok() || {
                std::thread::sleep(std::time::Duration::from_millis(5));
                false
            }
        });
        assert!(freed, "freed, so the next TUI may have it");
    }

    #[test]
    fn a_different_repo_is_never_blocked() {
        let a = repo();
        let b = repo();
        let _held = acquire(a.path()).unwrap();
        assert!(acquire(b.path()).is_ok(), "the lease is per repo");
    }

    /// The record is a diagnostic, never the gate: garbage in it still
    /// refuses, just without a place to point at.
    #[test]
    fn an_unreadable_holder_still_refuses() {
        let dir = repo();
        let _held = acquire(dir.path()).unwrap();
        std::fs::write(lock_path(dir.path()), b"not json").unwrap();
        let refused = acquire(dir.path()).expect_err("the lock is what refuses");
        assert!(refused.contains("unreadable"), "{refused}");
        assert!(!refused.contains("pid"), "nothing to point at: {refused}");
    }

    /// The one refusal a user cannot diagnose from outside: the lock
    /// itself is unopenable. It must name the path and the cause —
    /// an empty error here is the silence this plan exists to end.
    #[test]
    fn an_unusable_lock_refuses_out_loud() {
        let dir = tempfile::tempdir().unwrap();
        // `.clank` as a FILE: the lock path cannot be created under it.
        std::fs::write(dir.path().join(".clank"), b"not a directory").unwrap();
        let refused = acquire(dir.path()).expect_err("no lock, no lease");
        assert!(
            refused.contains("status-tui.lock"),
            "names the path: {refused}"
        );
        assert!(
            refused.len() > "status-tui.lock".len() + 20,
            "and the cause, not a bare path: {refused}"
        );
        assert!(
            !refused.trim().is_empty(),
            "an empty refusal is the bug this test exists for"
        );
    }

    #[test]
    fn the_holder_records_where_to_look() {
        let h = Holder {
            pid: 76361,
            session: Some("clank-full-app-sim--38b0".into()),
            pane: Some("59".into()),
            at: "2026-09-02T17:34:42Z".into(),
        };
        assert_eq!(
            h.describe(),
            "pid 76361 in session clank-full-app-sim--38b0 (pane 59) since 2026-09-02T17:34:42Z"
        );
        // Outside zellij there is no session to name, and the pane
        // never appears without one.
        let bare = Holder {
            session: None,
            pane: Some("59".into()),
            ..h
        };
        assert_eq!(bare.describe(), "pid 76361 since 2026-09-02T17:34:42Z");
    }
}
