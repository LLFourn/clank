//! What a live `clank wait` publishes about itself
//! (the-turn-end-is-one-decision).
//!
//! The stop hook decides whether to yield a turn to a background
//! wait, and the only evidence it had was the harness's task list —
//! whose own documentation says it names tasks that have already
//! finished. A completed wait therefore silenced every later
//! turn-end, permanently, because nothing re-checked and the yield
//! left no trace.
//!
//! Clank owns the other end: a `clank wait` is a process clank
//! started, so it can say so. This is that record. The harness list
//! is joined against it, and a task with no claim behind it is
//! `Unknown` — which does not suppress.
//!
//! Three fields, answering three different questions, because a
//! weaker predicate was already tried:
//!
//! - **birth** — a [`ProcToken`], not a pid. Pids are reused, and a
//!   killed waiter's number later belongs to a stranger.
//! - **session** — WHICH session this wait's completion wakes. A
//!   separately launched wait for the same label is not proof that
//!   the listed task will wake THIS one.
//! - **generation** — whose incarnation claimed it. Readers reject a
//!   stale claim; admission distinguishes an existing current wait
//!   from a predecessor that must release before takeover.

use std::path::{Path, PathBuf};

use crate::proc_identity::ProcToken;

/// A live wait's self-description. Versioned because it is
/// PERSISTED: a record from a build this one does not understand is
/// refused outright rather than compared field by field.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq)]
pub struct WaitClaim {
    /// Format version. A mismatch is refused, not migrated — this
    /// record's whole job is to be trusted.
    pub v: u32,
    pub pid: i32,
    /// The pid's birth identity, so a reused number cannot pass.
    pub birth: ProcToken,
    /// The session whose wake this wait's completion delivers.
    pub session: String,
    /// The incarnation that claimed the label.
    pub generation: u64,
    /// The REGISTRATION token from the arm instruction that launched
    /// this wait, when it was launched by one.
    ///
    /// This is the correlation the harness cannot provide: it appears
    /// in the listed task's `command` because the agent ran that
    /// command, and here because the wait read its own `--tag`.
    /// `None` for a wait nobody registered — which can never be
    /// matched to a listed task, and so can never prove one live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// The only version this build writes or accepts.
pub(crate) const CLAIM_V: u32 = 1;

pub(crate) fn claim_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("wait.claim")
}

/// Serializes claim ownership changes.
///
/// Publish and withdraw both read-then-write, and two waits handing
/// over is exactly when they interleave. Same mechanism as the event
/// WAL's sidecar lock.
fn with_lock<T>(agent_dir: &Path, f: impl FnOnce() -> T) -> std::io::Result<T> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(agent_dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(agent_dir.join("wait.claim.lock"))?;
    // SAFETY: valid owned fd; flock has no memory effects.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(f())
}

/// Test fixture for writing a claim without admission. Production
/// calls [`try_acquire`] for a session-bound wait and fails to start
/// if it cannot establish ownership: an unclaimed session-bound wait
/// could race the admitted wait and consume the same work twice.
#[cfg(test)]
pub(crate) fn publish(
    agent_dir: &Path,
    session: &str,
    generation: u64,
    tag: Option<String>,
) -> std::io::Result<WaitClaim> {
    let pid = std::process::id() as i32;
    let Some(birth) = crate::proc_identity::token_for(pid) else {
        return Err(std::io::Error::other(
            "no process birth identity on this platform",
        ));
    };
    let claim = WaitClaim {
        v: CLAIM_V,
        pid,
        birth,
        session: session.to_string(),
        generation,
        tag,
    };
    let bytes = serde_json::to_vec(&claim)?;
    let path = claim_path(agent_dir);
    with_lock(agent_dir, || {
        let tmp = path.with_extension("claim.tmp");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)
    })??;
    Ok(claim)
}

/// One serialized admission decision. The existing holder stays in
/// the record until it withdraws or its exact process birth is gone;
/// a successor never overwrites a live waiter merely because the
/// requested generation moved on.
#[derive(Debug)]
pub(crate) enum Admission {
    Taken(WaitClaim),
    Held(WaitClaim),
}

pub(crate) fn try_acquire(
    agent_dir: &Path,
    session: &str,
    generation: u64,
    tag: Option<String>,
) -> std::io::Result<Admission> {
    let pid = std::process::id() as i32;
    let birth = crate::proc_identity::token_for(pid)
        .ok_or_else(|| std::io::Error::other("no process birth identity on this platform"))?;
    let mine = WaitClaim {
        v: CLAIM_V,
        pid,
        birth,
        session: session.to_owned(),
        generation,
        tag,
    };
    with_lock(agent_dir, || {
        match read_unlocked(agent_dir) {
            ClaimRead::Claim(holder) if holder.birth.still_holds(holder.pid) => {
                return Ok(Admission::Held(holder));
            }
            ClaimRead::Unreadable => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "wait claim is unreadable; refusing to replace an unknown holder",
                ));
            }
            ClaimRead::Absent | ClaimRead::Claim(_) => {}
        }
        let path = claim_path(agent_dir);
        let tmp = path.with_extension("claim.tmp");
        std::fs::write(&tmp, serde_json::to_vec(&mine)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(Admission::Taken(mine))
    })?
}

/// Withdraw a claim THIS process published, and only that one.
///
/// Compare-and-delete under the lock. An unconditional delete let a
/// `--peek` probe, or an older wait exiting after a successor had
/// taken over, erase a live waiter's record — after which the next
/// hook nudged an agent that was already parked (codex 2f38deb).
pub(crate) fn withdraw_mine(agent_dir: &Path, mine: &WaitClaim) {
    let _ = with_lock(agent_dir, || {
        if read_unlocked(agent_dir) == ClaimRead::Claim(mine.clone()) {
            let _ = std::fs::remove_file(claim_path(agent_dir));
        }
    });
}

/// Read the claim, if this build can understand it.
pub fn read(agent_dir: &Path) -> Option<WaitClaim> {
    match read_state(agent_dir) {
        ClaimRead::Claim(c) => Some(c),
        ClaimRead::Absent | ClaimRead::Unreadable => None,
    }
}

/// What the claim file IS, keeping absence apart from a record this
/// build cannot read.
///
/// `read` collapses both to `None`, which is right for a caller
/// asking "may I yield to this?" — neither answers yes. It is wrong
/// for one asking "did the waiter leave?": an unreadable claim then
/// looks like a clean departure, and replacing a dead wait's claim
/// with an unreadable file turned a warning into a false success
/// (codex 392e6f6713b5).
#[derive(Debug, Clone, PartialEq)]
pub enum ClaimRead {
    /// No claim file.
    Absent,
    /// Present and unusable: unparseable, a wrong version, or the
    /// lock could not be taken. Says nothing either way.
    Unreadable,
    Claim(WaitClaim),
}

pub fn read_state(agent_dir: &Path) -> ClaimRead {
    match with_lock(agent_dir, || read_unlocked(agent_dir)) {
        Ok(state) => state,
        // The lock is the file's own; failing to take it is not
        // evidence that nothing is there.
        Err(_) => ClaimRead::Unreadable,
    }
}

fn read_unlocked(agent_dir: &Path) -> ClaimRead {
    let raw = match std::fs::read_to_string(claim_path(agent_dir)) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ClaimRead::Absent,
        Err(_) => return ClaimRead::Unreadable,
    };
    match serde_json::from_str::<WaitClaim>(&raw) {
        Ok(claim) if claim.v == CLAIM_V => ClaimRead::Claim(claim),
        _ => ClaimRead::Unreadable,
    }
}

/// Whether a claim proves a live wait whose wake reaches `session`,
/// published by the CURRENT incarnation.
///
/// Three checks, none sufficient alone:
///
/// - **birth** — what a bare `kill(pid, 0)` cannot do. A pid outlives
///   its process and the OS reuses it.
/// - **session** — the one the wait was LAUNCHED INSIDE, so its
///   completion reaches this hook. An earlier version copied the
///   label's bound session out of config, which is a roster fact and
///   not a delivery fact: a terminal-launched `clank wait --author
///   rev` claimed the session it would never wake, and a stale listed
///   task then silenced that session's turns (codex 2f38deb).
/// - **generation** — the incarnation that claimed the label. A
///   superseded claim describes a wait whose output no longer counts,
///   and yielding to it is yielding to nobody.
pub fn proves(claim: &WaitClaim, session: &str, current_generation: u64) -> bool {
    claim.session == session
        && claim.generation == current_generation
        && claim.birth.still_holds(claim.pid)
}

/// A REGISTERED live wait's token, for matching against the harness's
/// listed task.
///
/// All of `proves`, plus the registration. Without the token there is
/// nothing to match a listed task to — which is the whole reason
/// three earlier predicates each proved something other than
/// delivery (codex 07c7d8d).
pub fn registered_tag(agent_dir: &Path, session: &str, current_generation: u64) -> Option<String> {
    let claim = read(agent_dir)?;
    proves(&claim, session, current_generation)
        .then_some(claim.tag)
        .flatten()
}

/// Test helper for the process-birth liveness predicate. Production
/// admission uses the same predicate under the claim lock in
/// [`try_acquire`], preserving the predecessor until release or exit.
#[cfg(test)]
pub fn other_live_wait(agent_dir: &Path) -> Option<i32> {
    let claim = read(agent_dir)?;
    let mine = std::process::id() as i32;
    (claim.pid != mine && claim.birth.still_holds(claim.pid)).then_some(claim.pid)
}

/// What this process can honestly claim: the session it was LAUNCHED
/// INSIDE, if any.
///
/// `None` for a terminal-launched wait, which is the point — it has
/// no harness to deliver a completion wake to, so it must not claim
/// one. Read from the ambient environment rather than from the
/// roster, because the question is where this process's output GOES.
pub fn launch_session() -> Option<String> {
    launch_session_in(&crate::agent_env::Process)
}

pub(crate) fn launch_session_in(env: &dyn crate::agent_env::Ambient) -> Option<String> {
    crate::agent_env::detect_session_in(env)
        .ok()
        .flatten()
        .map(|(_, sid)| sid.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proc_identity::ProcToken;

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_live_holder_is_never_replaced_before_release_or_verified_exit() {
        let dir = tmpdir();
        let agent = dir.path();
        let first = match try_acquire(agent, "session-old", 1, None).unwrap() {
            Admission::Taken(claim) => claim,
            Admission::Held(_) => panic!("empty slot was occupied"),
        };
        let held = match try_acquire(agent, "session-new", 2, None).unwrap() {
            Admission::Held(claim) => claim,
            Admission::Taken(_) => panic!("live predecessor was overwritten"),
        };
        assert_eq!(held, first, "the predecessor's birth identity survives");
        assert_eq!(read(agent), Some(first.clone()));

        withdraw_mine(agent, &first);
        let next = match try_acquire(agent, "session-new", 2, None).unwrap() {
            Admission::Taken(claim) => claim,
            Admission::Held(_) => panic!("released slot was still held"),
        };
        assert_eq!(read(agent), Some(next.clone()));

        // A claim whose pid was reused is not a live holder.
        let dead = WaitClaim {
            birth: ProcToken::MacStart { sec: 0, usec: 0 },
            ..next
        };
        std::fs::write(claim_path(agent), serde_json::to_vec(&dead).unwrap()).unwrap();
        assert!(matches!(
            try_acquire(agent, "session-later", 3, None).unwrap(),
            Admission::Taken(_)
        ));
    }

    #[test]
    fn an_unreadable_claim_cannot_be_assumed_released() {
        let dir = tmpdir();
        std::fs::write(claim_path(dir.path()), "broken").unwrap();
        let err = try_acquire(dir.path(), "session", 1, None).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// A PID IS NOT AN IDENTITY.
    ///
    /// The number outlives the process and the OS reuses it, so a
    /// killed waiter's claim can point at a stranger that is very
    /// much alive. `kill(pid, 0)` says yes to that; the birth token
    /// says no. Without this the claim would be a liveness check that
    /// passes on the wrong process — a weaker predicate than the
    /// `Attending` record it is modelled on (codex f3edd17).
    #[test]
    fn a_reused_pid_does_not_prove_the_claim() {
        let dir = tmpdir();
        let agent = dir.path();

        // This process, honestly described: proven.
        publish(agent, "s", 0, None).unwrap();
        let mine = read(agent).expect("readable");
        assert!(proves(&mine, "s", 0));

        // The SAME pid, a different birth — which is what a reused
        // number looks like from here.
        let stranger = WaitClaim {
            birth: ProcToken::MacStart { sec: 0, usec: 0 },
            ..mine.clone()
        };
        assert!(
            !proves(&stranger, "s", 0),
            "a live pid whose process is not the one that claimed it proves nothing",
        );

        // A token from a build this one does not understand never
        // matches, so such a record is viewable and not actionable.
        let foreign = WaitClaim {
            birth: ProcToken::Unrecognised,
            ..mine.clone()
        };
        assert!(!proves(&foreign, "s", 0));

        // And a dead pid: no birth to read, so nothing holds.
        let dead = WaitClaim {
            pid: i32::MAX,
            ..mine
        };
        assert!(!proves(&dead, "s", 0));
    }

    /// A record from another build is refused outright.
    ///
    /// Versioned because it is PERSISTED: comparing a shape this
    /// build does not have, field by field, is how a format change
    /// becomes a false liveness.
    #[test]
    fn a_claim_from_another_version_is_not_read() {
        let dir = tmpdir();
        let agent = dir.path();
        publish(agent, "s", 0, None).unwrap();

        let path = claim_path(agent);
        let mut v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        v["v"] = serde_json::json!(CLAIM_V + 1);
        std::fs::write(&path, serde_json::to_vec(&v).unwrap()).unwrap();

        assert_eq!(read(agent), None, "a newer record is refused, not guessed");
    }

    /// Withdrawing leaves nothing to find.
    #[test]
    fn a_withdrawal_removes_only_the_record_it_wrote() {
        let dir = tmpdir();
        let agent = dir.path();

        let mine = publish(agent, "s", 0, None).unwrap();
        assert!(read(agent).is_some());
        withdraw_mine(agent, &mine);
        assert_eq!(read(agent), None);
        // Idempotent: the same guard dropping twice, or one whose
        // record someone else already replaced.
        withdraw_mine(agent, &mine);

        // A SUCCESSOR has published; the predecessor's drop must not
        // erase it. An unconditional delete let an older wait, and a
        // `--peek` probe, remove a live waiter's claim — after which
        // the next hook nudged an agent that was already parked
        // (codex 2f38deb).
        let successor = publish(agent, "s", 1, None).unwrap();
        assert_ne!(successor, mine, "a different incarnation");
        withdraw_mine(agent, &mine);
        assert_eq!(
            read(agent),
            Some(successor.clone()),
            "the successor's claim survives its predecessor's exit",
        );
        withdraw_mine(agent, &successor);
        assert_eq!(read(agent), None);
    }

    /// Another live wait is DETECTED; this process's own claim is not
    /// mistaken for one.
    #[test]
    fn a_second_waiter_is_visible_and_this_one_is_not() {
        let dir = tmpdir();
        let agent = dir.path();
        assert_eq!(other_live_wait(agent), None, "nothing published");

        // This process's own record is not "another" wait.
        let mine = publish(agent, "s", 0, None).unwrap();
        assert_eq!(other_live_wait(agent), None, "a guard must not see itself");

        // A record for a live process that is NOT this one.
        let other = WaitClaim {
            pid: 1,
            ..mine.clone()
        };
        std::fs::write(claim_path(agent), serde_json::to_vec(&other).unwrap()).unwrap();
        // pid 1 exists but its birth is not the one recorded here, so
        // a stale record still reads as gone — the same check that
        // stops a reused pid proving liveness.
        assert_eq!(
            other_live_wait(agent),
            None,
            "a dead or reused pid is not a waiter"
        );
    }

    /// A claim from a superseded incarnation proves nothing.
    ///
    /// The generation was recorded and never compared, so advancing
    /// `wait.gen` while the old process stayed alive still yielded —
    /// to a waiter whose output no longer counts (codex 2f38deb).
    #[test]
    fn a_superseded_claim_does_not_prove() {
        let dir = tmpdir();
        let claim = publish(dir.path(), "s", 7, None).unwrap();
        assert!(proves(&claim, "s", 7));
        assert!(
            !proves(&claim, "s", 8),
            "a newer incarnation has taken the label",
        );
    }
}
