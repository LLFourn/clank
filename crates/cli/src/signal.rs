//! Signal targets, validated by construction.
//!
//! `kill` reads three numbers as something other than the one process
//! or group you meant: -1 is every process the caller may signal, 0 is
//! the caller's own group, and 1 is init. The API puts all three one
//! digit away from the intended target.
//!
//! [`Pgid`] exists because a validating `if` was not enough. A mutation
//! run deleted the `if pgid <= 1` inside the signalling function, the
//! run was interrupted before it restored the file, and the test
//! written to prove the guard held — which called
//! `signal_group(1, SIGKILL)` — then ran `kill(-1, SIGKILL)` and took
//! down a developer's entire login session, twice in one day. A guard
//! made of a statement protects nothing from a process whose job is to
//! delete statements.
//!
//! DELIVERY and PROBING are separated here because the hazard is not
//! the same for both, and treating them alike would have cost either
//! safety or an answer:
//!
//! - Delivering needs a newtype. [`signal_process`] and
//!   [`signal_group`] take [`Pid`] and [`Pgid`], so the catastrophic
//!   targets cannot be spelled at a call site at all.
//! - Probing with signal 0 delivers nothing, so a bad id there is a
//!   WRONG ANSWER rather than a disaster, and the guard can stay a
//!   statement — once, here. [`probe_process`] therefore takes a bare
//!   pid, which keeps `probe_process(1)` answerable: init is the only
//!   portable example of a process that exists and is somebody else's,
//!   and a liveness check has every reason to ask about it. The group
//!   probe keeps its newtype, because `kill(-1, 0)` succeeds and would
//!   be read as "that group is occupied".

/// A process group id that is safe to address.
///
/// There is no statement to delete and no test has to issue a real
/// signal to check it: `signal_group(1, …)` does not compile, and the
/// only way to obtain one of these is through a constructor that
/// refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pgid(rustix::process::Pid);

impl Pgid {
    /// `None` for the ids that do not name a group we may address.
    pub fn new(pgid: i32) -> Option<Self> {
        (pgid > 1)
            .then(|| rustix::process::Pid::from_raw(pgid))
            .flatten()
            .map(Self)
    }

    pub fn get(self) -> i32 {
        self.0.as_raw_pid()
    }
}

impl std::fmt::Display for Pgid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

/// One process that is safe to address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pid(rustix::process::Pid);

impl Pid {
    /// `None` for the ids that do not name a process we may signal.
    ///
    /// The same threshold as [`Pgid::new`], and for the same reasons:
    /// the single-process form of `kill` reads -1 and 0 exactly as the
    /// group form does, and 1 is not ours to end.
    pub fn new(pid: i32) -> Option<Self> {
        (pid > 1)
            .then(|| rustix::process::Pid::from_raw(pid))
            .flatten()
            .map(Self)
    }

    pub fn get(self) -> i32 {
        self.0.as_raw_pid()
    }
}

impl std::fmt::Display for Pid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

/// What signal 0 says about a target.
///
/// Three answers rather than a bool, because the callers disagree about
/// `EPERM` and should have to say which they mean: a process that
/// exists but belongs to somebody else is ALIVE to a liveness check and
/// NOT OURS to a reaper. Collapsing it to a bool is how one probe came
/// to be written three times with two different answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// It is there, and ours to signal.
    Ours,
    /// It is there and belongs to somebody else (`EPERM`).
    Theirs,
    /// Nothing is there (`ESRCH`).
    Gone,
    /// The kernel said something else, which is not an answer.
    Unknown(i32),
}

/// Whether `pid` names a running process, without signalling it.
pub fn probe_process(pid: i32) -> Probe {
    if pid <= 0 {
        return Probe::Gone;
    }
    let Some(pid) = rustix::process::Pid::from_raw(pid) else {
        return Probe::Gone;
    };
    probe_from(outcome(rustix::process::test_kill_process(pid)))
}

/// Whether `pid` names a process that EXISTS, whoever owns it.
///
/// `Theirs` counts as alive: the process is there, which is the whole
/// question, and permission to signal it is a different one. This
/// reading was written out three times before it lived here, and two of
/// those also carried their own `if pid <= 0` guard.
pub fn alive(pid: i32) -> bool {
    matches!(probe_process(pid), Probe::Ours | Probe::Theirs)
}

/// Whether anything remains in `pgid`, without signalling it.
///
/// Asks the kernel rather than enumerating: the GROUP is what a spawned
/// command owns, and its leader's absence does not answer the question —
/// a `sh -c` that backgrounds something exits while that descendant
/// keeps running in the same group.
pub fn probe_group(pgid: Pgid) -> Probe {
    probe_from(outcome(rustix::process::test_kill_process_group(pgid.0)))
}

/// The processes in `pgid`, or `None` when the system will not say.
///
/// Enumerates, unlike [`probe_group`], because the question it answers
/// is WHO is left: a group led by a process that outlives its work is
/// occupied for as long as that leader lives.
pub fn group_members(pgid: Pgid) -> Option<Vec<i32>> {
    group_members_native(pgid.get())
}

#[cfg(target_os = "macos")]
fn group_members_native(pgid: i32) -> Option<Vec<i32>> {
    crate::proc_identity::inspect::group_members(pgid)
}

#[cfg(not(target_os = "macos"))]
fn group_members_native(pgid: i32) -> Option<Vec<i32>> {
    let mut members = Vec::new();
    for entry in std::fs::read_dir("/proc").ok()? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // The command name may hold spaces and parentheses, so the
        // fields are counted from its LAST closing parenthesis.
        let Some(rest) = stat.rfind(')').map(|at| &stat[at + 1..]) else {
            continue;
        };
        if rest.split_whitespace().nth(2).and_then(|s| s.parse().ok()) == Some(pgid) {
            members.push(pid);
        }
    }
    Some(members)
}

/// Signal every process in `pgid`.
pub fn signal_group(pgid: Pgid, sig: libc::c_int) {
    deliver(Aim::Group(pgid), sig, &Kernel);
}

/// Signal one process.
pub fn signal_process(pid: Pid, sig: libc::c_int) {
    deliver(Aim::Process(pid), sig, &Kernel);
}

/// Who a signal is for, validated by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Aim {
    Process(Pid),
    Group(Pgid),
}

/// The two ways a signal reaches the kernel.
///
/// rustix names only the standard signals, and its unchecked constructor
/// forbids sending the libc-reserved real-time ones, so any other number
/// goes to `kill` as it is: delivered if valid, refused if not, exactly
/// as before rustix.
trait Deliver {
    fn named(&self, aim: Aim, sig: rustix::process::Signal);
    fn raw(&self, aim: Aim, sig: libc::c_int);
}

fn deliver(aim: Aim, sig: libc::c_int, via: &dyn Deliver) {
    match rustix::process::Signal::from_named_raw(sig) {
        Some(named) => via.named(aim, named),
        None => via.raw(aim, sig),
    }
}

struct Kernel;

impl Deliver for Kernel {
    fn named(&self, aim: Aim, sig: rustix::process::Signal) {
        let _ = match aim {
            Aim::Process(pid) => rustix::process::kill_process(pid.0, sig),
            Aim::Group(pgid) => rustix::process::kill_process_group(pgid.0, sig),
        };
    }

    /// Unsafe because rustix has no way to send a signal it cannot name.
    #[allow(unsafe_code)]
    fn raw(&self, aim: Aim, sig: libc::c_int) {
        let target = match aim {
            Aim::Process(pid) => pid.get(),
            Aim::Group(pgid) => -pgid.get(),
        };
        // SAFETY: `kill` takes no pointers. `target` comes from a `Pid` or
        // `Pgid`, which refuse every id at or below 1, so it names exactly
        // that process or group; an invalid `sig` is refused with EINVAL.
        unsafe {
            libc::kill(target, sig);
        }
    }
}

/// Reap `pid` if it is this process's child and has exited, without
/// waiting for it.
///
/// An exited child that nobody has waited for is still a member of
/// its process group, so a host that has just killed its own group
/// cannot see it empty until it does this. `ECHILD` — not our child,
/// or already reaped by the runtime — leaves nothing to do.
pub fn collect_exited_child(pid: Pid) {
    let _ = rustix::process::waitpid(Some(pid.0), rustix::process::WaitOptions::NOHANG);
}

/// A `kill` result read as an errno, with `None` meaning it succeeded.
fn outcome(result: rustix::io::Result<()>) -> Option<i32> {
    result.err().map(rustix::io::Errno::raw_os_error)
}

/// Read a probe's outcome.
///
/// Separated from the call so `Theirs` is reachable without another
/// user's process to point at, which a test has no way to arrange.
pub(crate) fn probe_from(errno: Option<i32>) -> Probe {
    match errno {
        None => Probe::Ours,
        Some(e) if e == libc::ESRCH => Probe::Gone,
        Some(e) if e == libc::EPERM => Probe::Theirs,
        Some(e) => Probe::Unknown(e),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_groups_members_are_listed() {
        use std::os::unix::process::CommandExt;
        let mut leader = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = super::Pgid::new(leader.id() as i32).unwrap();
        let mut members = Vec::new();
        for _ in 0..200 {
            members = super::group_members(pgid).unwrap();
            if members.len() >= 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        super::signal_group(pgid, libc::SIGKILL);
        let _ = leader.wait();
        assert!(members.contains(&(leader.id() as i32)), "{members:?}");
        assert_eq!(members.len(), 2, "the leader and its sleep: {members:?}");
    }

    use super::*;

    #[derive(Default)]
    struct Recorded(std::cell::RefCell<Vec<(Aim, &'static str, i32)>>);

    impl Deliver for Recorded {
        fn named(&self, aim: Aim, sig: rustix::process::Signal) {
            self.0.borrow_mut().push((aim, "named", sig.as_raw()));
        }
        fn raw(&self, aim: Aim, sig: libc::c_int) {
            self.0.borrow_mut().push((aim, "raw", sig));
        }
    }

    /// A number rustix cannot name still reaches `kill`, as it did before
    /// rustix: nothing is dropped on the way. Recorded, never sent.
    #[test]
    fn every_signal_number_is_delivered_named_or_raw() {
        let pid = Aim::Process(Pid::new(4242).unwrap());
        let group = Aim::Group(Pgid::new(4243).unwrap());
        #[cfg(target_os = "linux")]
        let realtime = libc::SIGRTMIN();
        #[cfg(not(target_os = "linux"))]
        let realtime = 64;
        let seen = Recorded::default();
        for (aim, sig) in [
            (pid, libc::SIGTERM),
            (group, libc::SIGKILL),
            (pid, realtime),
            (group, realtime),
            (group, 0),
        ] {
            deliver(aim, sig, &seen);
        }
        assert_eq!(
            seen.0.into_inner(),
            vec![
                (pid, "named", libc::SIGTERM),
                (group, "named", libc::SIGKILL),
                (pid, "raw", realtime),
                (group, "raw", realtime),
                (group, "raw", 0),
            ]
        );
    }

    /// The ids that mean something other than one target are refused by
    /// the only constructor, so `signal_process(1, …)` and
    /// `signal_group(1, …)` cannot be written.
    ///
    /// Asserted on CONSTRUCTION. The test this replaces proved the old
    /// statement guard by issuing the real `kill`, so its safety
    /// depended on the very code it was checking — which is how the
    /// guard's deletion became a catastrophe instead of a red test.
    /// Nothing is sent here.
    #[test]
    fn no_catastrophic_target_can_be_constructed() {
        for id in [-1, 0, 1] {
            assert!(Pid::new(id).is_none(), "pid {id} is not one process");
            assert!(Pgid::new(id).is_none(), "pgid {id} is not one group");
        }
        assert_eq!(Pid::new(4242).map(Pid::get), Some(4242));
        assert_eq!(Pgid::new(4242).map(Pgid::get), Some(4242));
    }

    #[test]
    fn a_probe_reads_its_errno() {
        assert_eq!(probe_from(None), Probe::Ours);
        assert_eq!(probe_from(Some(libc::ESRCH)), Probe::Gone);
        assert_eq!(
            probe_from(Some(libc::EPERM)),
            Probe::Theirs,
            "it exists and is somebody else's, which is not Gone"
        );
        assert_eq!(
            probe_from(Some(libc::EINVAL)),
            Probe::Unknown(libc::EINVAL),
            "an unexpected errno is not an answer"
        );
    }

    /// A probe may ask about an id a signal may not target.
    ///
    /// Init is the one process certain to exist on both platforms, and
    /// `Pid::new(1)` is `None`, which is the separation this module
    /// exists to make: existence is answerable, delivery is not
    /// offered.
    ///
    /// Asserted as "not `Gone`" rather than as `Theirs`, because which
    /// of the two a non-`Gone` answer is depends on who runs the
    /// suite: a non-root run takes the `EPERM` branch and a root run
    /// takes the success branch. Both must read as ALIVE, since the
    /// question is existence.
    #[test]
    fn a_probe_answers_for_ids_no_signal_may_take() {
        assert_eq!(probe_process(std::process::id() as i32), Probe::Ours);
        assert_ne!(probe_process(1), Probe::Gone, "init exists");
        assert!(alive(1), "whoever owns it");
        assert!(Pid::new(1).is_none(), "and still cannot be signalled");
        assert_eq!(probe_process(0), Probe::Gone, "0 is a group, not a process");
        assert_eq!(probe_process(-1), Probe::Gone, "-1 is every process");
        assert!(!alive(i32::MAX), "and that cannot be a live pid");
    }
}
