//! Who a pid actually IS.
//!
//! A pid alone cannot identify a process across time. `kill(pid, 0)`
//! answers "does SOME process hold this number", and the OS reuses
//! numbers freely — so a recorded pid whose process has exited can
//! come back as alive, and killable, and be a stranger. Anything that
//! signals a process recorded earlier has to check more than the
//! number.
//!
//! [`ProcToken`] is that check: a per-process birth identity, captured
//! when the pid is recorded and compared before it is trusted. It is
//! versioned because it is PERSISTED — a token this build does not
//! understand must be refused outright rather than compared field by
//! field against a shape it does not have.

/// A process's birth identity. Compared in FULL; a prefix match is the
/// same bug in a smaller form.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "v")]
pub enum ProcToken {
    /// The full start timeval. Seconds alone are not a unique birth —
    /// two processes can be born in the same second, which is exactly
    /// when a reused pid would otherwise pass.
    #[serde(rename = "mac1")]
    MacStart { sec: u64, usec: u64 },
    /// Start ticks are counted from BOOT and so repeat every boot; the
    /// boot id is what stops a pre-restart record matching an
    /// unrelated process after one.
    #[serde(rename = "linux1")]
    LinuxStart { ticks: u64, boot: String },
    /// Written by a different build than this one. Never matches, so a
    /// record carrying it is viewable but not actionable.
    #[serde(other)]
    Unrecognised,
}

impl ProcToken {
    /// Is the process at `pid` still the one this token was taken
    /// from? False whenever that cannot be established — including an
    /// unrecognised token, a dead pid, and a platform that cannot
    /// answer. Refusing is the only safe default: the caller's next
    /// move is to signal something.
    pub fn still_holds(&self, pid: i32) -> bool {
        if matches!(self, ProcToken::Unrecognised) {
            return false;
        }
        token_for(pid).as_ref() == Some(self)
    }
}

/// This process's birth identity, or `None` when it cannot be read —
/// the pid is gone, permission is denied, or the platform has no
/// supported source.
pub fn token_for(pid: i32) -> Option<ProcToken> {
    if pid <= 0 {
        return None;
    }
    platform::token_for(pid)
}

#[cfg(target_os = "macos")]
mod platform {
    use super::ProcToken;

    pub(super) fn token_for(pid: i32) -> Option<ProcToken> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: `info` is a correctly sized, zeroed proc_bsdinfo and
        // `size` describes it; the call only writes into that buffer.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (n == size).then_some(ProcToken::MacStart {
            sec: info.pbi_start_tvsec,
            usec: info.pbi_start_tvusec,
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::ProcToken;

    pub(super) fn token_for(pid: i32) -> Option<ProcToken> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 2 is the comm, parenthesised and free to contain
        // spaces or ')' — so fields are counted from AFTER the last
        // ')', never by splitting the whole line.
        let rest = &stat[stat.rfind(')')? + 1..];
        // starttime is field 22; two fields precede it in `rest`
        // (the empty split before ' ' and state), so it is index 20
        // once `rest` is split on whitespace.
        let ticks: u64 = rest.split_whitespace().nth(19)?.parse().ok()?;
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()?
            .trim()
            .to_string();
        (!boot.is_empty()).then_some(ProcToken::LinuxStart { ticks, boot })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::ProcToken;

    /// No supported source: everything that consults a token treats
    /// `None` as "cannot identify", which refuses rather than guesses.
    pub(super) fn token_for(_pid: i32) -> Option<ProcToken> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_a_token_and_it_matches_itself() {
        let me = std::process::id() as i32;
        let t = token_for(me).expect("a running process has an identity");
        assert!(t.still_holds(me), "and it is stable across two reads");
    }

    #[test]
    fn a_token_never_matches_a_different_process() {
        let me = std::process::id() as i32;
        let t = token_for(me).unwrap();
        // pid 1 exists on every unix and is not this test.
        assert!(!t.still_holds(1), "another live pid must not match");
    }

    #[test]
    fn non_positive_pids_are_refused_before_any_syscall() {
        // `kill` reads 0 as the process group and -1 as everything;
        // no identity lookup may be reached with either.
        assert_eq!(token_for(0), None);
        assert_eq!(token_for(-1), None);
    }

    /// The reuse case, at the granularity that lets it through: two
    /// births in the same second differ only below it.
    #[test]
    fn a_same_second_birth_is_a_different_identity() {
        let a = ProcToken::MacStart { sec: 100, usec: 1 };
        let b = ProcToken::MacStart { sec: 100, usec: 2 };
        assert_ne!(a, b, "the sub-second component must count");
    }

    /// Start ticks repeat every boot, so a record can outlive the
    /// process it named and still match by ticks alone.
    #[test]
    fn matching_ticks_under_a_different_boot_are_a_different_identity() {
        let a = ProcToken::LinuxStart {
            ticks: 4242,
            boot: "aaaa".into(),
        };
        let b = ProcToken::LinuxStart {
            ticks: 4242,
            boot: "bbbb".into(),
        };
        assert_ne!(a, b, "the boot identity must count");
    }

    /// A token from another build is refused outright — never
    /// compared field by field against a shape it does not have.
    #[test]
    fn an_unrecognised_token_matches_nothing() {
        let raw = r#"{"v":"from-the-future","sec":1,"usec":2}"#;
        let t: ProcToken = serde_json::from_str(raw).expect("unknown versions still parse");
        assert_eq!(t, ProcToken::Unrecognised);
        assert!(
            !t.still_holds(std::process::id() as i32),
            "even against a live pid"
        );
    }

    #[test]
    fn tokens_round_trip_through_the_record_format() {
        for t in [
            ProcToken::MacStart { sec: 7, usec: 8 },
            ProcToken::LinuxStart {
                ticks: 9,
                boot: "id".into(),
            },
        ] {
            let json = serde_json::to_string(&t).unwrap();
            assert_eq!(serde_json::from_str::<ProcToken>(&json).unwrap(), t);
        }
    }
}
