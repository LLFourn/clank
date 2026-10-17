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
pub(crate) struct Holder {
    pub(super) pid: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) pane: Option<String>,
    pub(super) at: String,
    /// Who the holder is, beyond a pid the OS may reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) birth: Option<crate::proc_identity::ProcToken>,
    /// The `clank remote` protocol this holder serves. Absent from a
    /// holder that predates it: positive evidence of an older build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) remote_protocol: Option<u32>,
}

impl Holder {
    pub(crate) fn here() -> Self {
        Self {
            pid: std::process::id() as i32,
            session: std::env::var("ZELLIJ_SESSION_NAME").ok(),
            pane: std::env::var("ZELLIJ_PANE_ID").ok(),
            at: crate::cli::stop_hook::now_rfc3339(),
            birth: crate::proc_identity::token_for(std::process::id() as i32),
            remote_protocol: Some(crate::cli::remote_control::PROTOCOL),
        }
    }

    /// Where to go and close it.
    pub(crate) fn describe(&self) -> String {
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
    // Beside the monitor's runtime state rather than in the repo: it
    // has no meaning once this process is gone, and in the repo its
    // visibility to git depended on each one's `.clank/.gitignore`.
    crate::cli::monitor::runtime_path(repo, "status")
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
    crate::cli::monitor::ensure_runtime_dir()?;
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
                 One `clank tui` per repo is decided by that file; \
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

/// Whether anyone holds `repo`'s lease, asked without keeping it: a
/// non-blocking flock taken and released at once. `true` is authoritative
/// absence. Otherwise, the holder record if it could be read.
pub(crate) fn probe(repo: &Path) -> (bool, Option<crate::cli::remote_control::Holder>) {
    use std::os::fd::AsRawFd;
    let path = lock_path(repo);
    let file = match std::fs::OpenOptions::new().read(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (true, None),
        Err(_) => return (false, None),
    };
    // SAFETY: valid owned fd; flock has no memory effects.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        // SAFETY: as above; the lock is released before anyone else
        // could be told this repo has no owner.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return (true, None);
    }
    let holder = read_holder(&path).map(|h| crate::cli::remote_control::Holder {
        pid: h.pid,
        birth: h.birth,
        remote_protocol: h.remote_protocol,
    });
    (false, holder)
}

/// Take the lease, recording `holder` as its holder: a test standing
/// in for a TUI that is another process.
#[cfg(test)]
pub(crate) fn hold_as(repo: &Path, holder: &crate::cli::remote_control::Holder) -> StatusLease {
    use std::io::Write;
    let lease = acquire(repo).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(lock_path(repo))
        .unwrap();
    let record = Holder {
        pid: holder.pid,
        session: None,
        pane: None,
        at: String::new(),
        birth: holder.birth.clone(),
        remote_protocol: holder.remote_protocol,
    };
    file.write_all(&serde_json::to_vec(&record).unwrap())
        .unwrap();
    lease
}

fn read_holder(path: &Path) -> Option<Holder> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The refusal, with the holder named when one could be read.
pub(super) fn busy_message(holder: Option<Holder>) -> String {
    match holder {
        Some(h) => format!(
            "this repo's status is already open in another `clank tui`: {}. \
             Close that one, or use it — two of them cannot both drive the panes.",
            h.describe()
        ),
        None => "this repo's status is already open in another `clank tui` \
                 (its holder record is unreadable). Close it, or use it — two of them \
                 cannot both drive the panes."
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cli::monitor::TempRepo;

    fn repo() -> TempRepo {
        TempRepo::new()
    }

    /// A repo with NO `.clank/.gitignore` stays clean while BOTH a
    /// monitor and a panel hold their state.
    ///
    /// The shape that produced the plan: a fork from a repo that never
    /// had that file. It propagates by being TRACKED, so the new
    /// worktree had no ignore rule and nothing in it could add one,
    /// and two untracked runtime files appeared in every such team.
    /// No ignore rule here, deliberately — the point is that none is
    /// needed.
    ///
    /// Composed HERE, in the status lease's own module, because the
    /// real `acquire` is what has to be driven. The first version of
    /// this lived beside the monitor and stood the panel in with a
    /// bare `fs::write` to the path the lease was expected to use —
    /// which asserts the test's own belief about that path and nothing
    /// about the code. A regression putting the status lease back
    /// under `.clank/status-tui.lock` passed it (codex on 9dfc3e3).
    #[test]
    fn a_repo_with_no_ignore_rules_stays_clean_while_both_guards_are_held() {
        use crate::cli::monitor;

        let repo = TempRepo::new();
        assert!(
            !repo.path().join(".clank/.gitignore").exists(),
            "the fixture is the repo that has none"
        );
        let before: Vec<String> = entries(repo.path());

        // Both real holders, at once, in the order a session takes
        // them.
        let _monitor = monitor::acquire_lease(repo.path()).expect("the monitor owns it");
        monitor::publish(
            repo.path(),
            &monitor::describe(repo.path(), 4242, Path::new("/tmp/x.sock")),
        )
        .expect("and publishes where a client will look");
        let _panel = acquire(repo.path()).expect("and the panel takes its lease");

        assert!(
            entries(&repo.path().join(".clank")).is_empty(),
            "a running monitor and a live panel left {:?} in `.clank/`",
            entries(&repo.path().join(".clank"))
        );
        assert_eq!(
            entries(repo.path()),
            before,
            "and nothing new anywhere else in the working tree either"
        );
        // The state still EXISTS, outside the repo, or this would pass
        // for a build that simply stopped keeping any.
        assert!(monitor::read_published(repo.path()).is_some(), "findable");
        assert!(
            acquire(repo.path()).is_err(),
            "and the panel's lease is really held"
        );
    }

    /// What a directory holds, sorted, or nothing when it is absent.
    fn entries(dir: &Path) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
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
        // A DIRECTORY where this repo's lock file belongs, so the lock
        // cannot be opened. Scoped to this repo's own runtime path
        // rather than to the shared namespace, which other tests use.
        crate::cli::monitor::ensure_runtime_dir().unwrap();
        let at = crate::cli::monitor::runtime_path(dir.path(), "status");
        std::fs::create_dir_all(&at).unwrap();
        let refused = acquire(dir.path()).expect_err("no lock, no lease");
        // Before the assertions, so a failing one does not leave a
        // DIRECTORY in the shared namespace at a name a later caller
        // would then find unopenable.
        std::fs::remove_dir(&at).unwrap();
        assert!(refused.contains(".status"), "names the path: {refused}");
        assert!(
            refused.len() > at.display().to_string().len() + 20,
            "and the cause, not a bare path: {refused}"
        );
        assert!(
            !refused.trim().is_empty(),
            "an empty refusal is the bug this test exists for"
        );
    }

    /// The probe reads absence only from taking the lock itself, and a
    /// held lease names a holder that serves the protocol.
    #[test]
    fn the_probe_tells_a_free_lease_from_a_held_one() {
        let repo = TempRepo::new();
        assert_eq!(probe(repo.path()), (true, None), "never taken");
        let lease = acquire(repo.path()).unwrap();
        let (free, holder) = probe(repo.path());
        assert!(!free, "held");
        let holder = holder.unwrap();
        assert_eq!(holder.pid, std::process::id() as i32);
        assert!(holder.birth.is_some());
        assert_eq!(
            holder.remote_protocol,
            Some(crate::cli::remote_control::PROTOCOL)
        );
        drop(lease);
        // A test spawning a process in parallel holds a forked copy of
        // the lock's fd until its exec, so release can lag a moment.
        let freed = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(10));
            probe(repo.path()).0
        });
        assert!(freed, "freed with the lease");
    }

    #[test]
    fn the_holder_records_where_to_look() {
        let h = Holder {
            pid: 76361,
            session: Some("clank-full-app-sim--38b0".into()),
            pane: Some("59".into()),
            at: "2026-09-02T17:34:42Z".into(),
            birth: None,
            remote_protocol: None,
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
