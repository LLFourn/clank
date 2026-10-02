//! On-demand observations of one recorded process. Call off the UI loop.
//!
//! Identity brackets ALL detail reads. Success means the recorded birth was
//! observed on both sides, not that mutable argv/state/relationships are atomic
//! or still current. In particular exec can change argv without changing birth.
//! Results never authorize signalling; the existing confirmation path must
//! revalidate the attendance and token. No subprocesses, process-table scans,
//! environment lookups, timers, workers or persistent cache live in this layer.

use super::ProcToken;
use std::ffi::OsString;
use std::io;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
use std::os::unix::ffi::OsStringExt;

/// Why an explanatory field (or the entire identity) could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    InvalidPid,
    Unsupported,
    Malformed,
    TooLarge,
    Io(io::ErrorKind),
}

impl From<io::Error> for Unavailable {
    fn from(e: io::Error) -> Self {
        match e.kind() {
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::InvalidData => Self::Malformed,
            io::ErrorKind::FileTooLarge => Self::TooLarge,
            kind => Self::Io(kind),
        }
    }
}

pub type Field<T> = Result<T, Unavailable>;

/// OS state, not a liveness probe. Runnable includes queued CPU work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    Initializing,
    Runnable,
    Sleeping,
    UninterruptibleSleep,
    Stopped,
    TracingStop,
    Zombie,
    Dead,
    Idle,
    Parked,
    Other(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessDetails {
    pub pid: i32,
    /// The recorded AND checked birth. Render this, never a fresh timestamp
    /// or the attendance age. Linux wall-clock conversion is intentionally
    /// absent: boot-relative ticks and boot ID are the exact identity.
    pub birth: ProcToken,
    pub observed_at: time::OffsetDateTime,
    /// Raw OS arguments, preserving boundaries, empty arguments and bytes.
    /// An empty vector means no arguments were reported, not a failed read.
    /// Consumers must escape control characters before terminal rendering.
    pub argv: Field<Vec<OsString>>,
    pub state: Field<ProcessState>,
    /// Relationships at inspection time, not evidence of launch provenance.
    /// Zero can be legitimately reported for a kernel process's parent/group.
    pub parent: Field<i32>,
    pub group: Field<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    Verified(ProcessDetails),
    /// A birth was readable but did not match the recorded process.
    Mismatched,
    /// The kernel explicitly reported that the process no longer exists.
    Ended,
    /// No liveness or identity claim can be made, e.g. permission denied.
    Unavailable(Unavailable),
}

/// Inspect a positive PID against a known recorded identity. At most two
/// identity observations and a bounded set of native per-process reads; no
/// retry. A partial field error survives a successful identity check. If the
/// final identity read fails or changes, NONE of the details are returned.
pub fn inspect(pid: i32, expected: &ProcToken) -> Inspection {
    inspect_with(&Native, pid, expected)
}

struct Fields {
    state: Field<ProcessState>,
    parent: Field<i32>,
    group: Field<i32>,
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
impl Fields {
    fn unavailable(why: Unavailable) -> Self {
        Self {
            state: Err(why),
            parent: Err(why),
            group: Err(why),
        }
    }
}

trait Source {
    fn identity(&self, pid: i32) -> Field<ProcToken>;
    fn fields(&self, pid: i32) -> Fields;
    fn argv(&self, pid: i32) -> Field<Vec<OsString>>;
}

fn identity_failure(why: Unavailable) -> Inspection {
    if why == Unavailable::Io(io::ErrorKind::NotFound) {
        Inspection::Ended
    } else {
        Inspection::Unavailable(why)
    }
}

fn inspect_with(source: &impl Source, pid: i32, expected: &ProcToken) -> Inspection {
    if pid <= 0 {
        return Inspection::Unavailable(Unavailable::InvalidPid);
    }
    if matches!(expected, ProcToken::Unrecognised) {
        return Inspection::Unavailable(Unavailable::Unsupported);
    }
    match source.identity(pid) {
        Ok(token) if &token == expected => {}
        Ok(_) => return Inspection::Mismatched,
        Err(why) => return identity_failure(why),
    }
    let fields = source.fields(pid);
    let argv = source.argv(pid);
    match source.identity(pid) {
        Ok(token) if &token == expected => {}
        Ok(_) => return Inspection::Mismatched,
        Err(why) => return identity_failure(why),
    }
    Inspection::Verified(ProcessDetails {
        pid,
        birth: expected.clone(),
        observed_at: time::OffsetDateTime::now_utc(),
        argv,
        state: fields.state,
        parent: fields.parent,
        group: fields.group,
    })
}

struct Native;

impl Source for Native {
    fn identity(&self, pid: i32) -> Field<ProcToken> {
        super::read_token_for(pid).map_err(Unavailable::from)
    }

    fn fields(&self, pid: i32) -> Fields {
        platform::fields(pid)
    }

    fn argv(&self, pid: i32) -> Field<Vec<OsString>> {
        platform::argv(pid)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
const MAX_ARG_BYTES: usize = 1024 * 1024;
#[cfg(any(target_os = "macos", target_os = "linux", test))]
const MAX_ARGS: usize = 16 * 1024;

#[cfg(any(target_os = "linux", test))]
fn linux_argv(bytes: &[u8]) -> Field<Vec<OsString>> {
    if bytes.len() > MAX_ARG_BYTES {
        return Err(Unavailable::TooLarge);
    }
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    // cmdline is NUL separated and terminated. Refuse a torn read instead
    // of presenting a truncated argument as complete.
    if bytes.last() != Some(&0) {
        return Err(Unavailable::Malformed);
    }
    let mut args = Vec::new();
    for arg in bytes[..bytes.len() - 1].split(|b| *b == 0) {
        if args.len() == MAX_ARGS {
            return Err(Unavailable::TooLarge);
        }
        args.push(OsString::from_vec(arg.to_vec()));
    }
    Ok(args)
}

#[cfg(any(target_os = "linux", test))]
fn linux_state(state: u8) -> ProcessState {
    match state {
        b'R' => ProcessState::Runnable,
        b'S' => ProcessState::Sleeping,
        b'D' => ProcessState::UninterruptibleSleep,
        b'T' => ProcessState::Stopped,
        b't' => ProcessState::TracingStop,
        b'Z' => ProcessState::Zombie,
        b'X' | b'x' => ProcessState::Dead,
        b'I' => ProcessState::Idle,
        b'P' => ProcessState::Parked,
        other => ProcessState::Other(other.into()),
    }
}

#[cfg(any(target_os = "macos", test))]
fn mac_argv(bytes: &[u8]) -> Field<Vec<OsString>> {
    // KERN_PROCARGS2: native int argc, executable path + NUL padding,
    // then exactly argc NUL-terminated arguments. Trailing bytes may be
    // environment variables on some kernels: NEVER decode or retain them.
    // https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sysctl.c
    if bytes.len() > MAX_ARG_BYTES + 4 {
        return Err(Unavailable::TooLarge);
    }
    let header = bytes.get(..4).ok_or(Unavailable::Malformed)?;
    let argc = i32::from_ne_bytes(header.try_into().unwrap());
    if argc < 0 {
        return Err(Unavailable::Malformed);
    }
    if argc as usize > MAX_ARGS {
        return Err(Unavailable::TooLarge);
    }
    if argc == 0 {
        return Ok(Vec::new());
    }
    let body = &bytes[4..];
    let path_end = body
        .iter()
        .position(|b| *b == 0)
        .ok_or(Unavailable::Malformed)?;
    let mut pos = path_end + 1;
    while body.get(pos) == Some(&0) {
        pos += 1;
    }
    let mut args = Vec::new();
    for _ in 0..argc {
        let rest = body.get(pos..).ok_or(Unavailable::Malformed)?;
        let end = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or(Unavailable::Malformed)?;
        args.push(OsString::from_vec(rest[..end].to_vec()));
        pos += end + 1;
    }
    Ok(args)
}

#[cfg(any(target_os = "macos", test))]
fn mac_state(status: u32, running_threads: Field<i32>) -> Field<ProcessState> {
    // BSD p_stat SRUN is also reported for sleeping Mach tasks. Only the
    // native task's running-thread count distinguishes that case. Without
    // it, state is unknown; do not turn an unreadable task into "running".
    match status {
        1 => Ok(ProcessState::Initializing), // SIDL
        2 => match running_threads? {
            // SRUN
            0 => Ok(ProcessState::Sleeping),
            n if n > 0 => Ok(ProcessState::Runnable),
            _ => Err(Unavailable::Malformed),
        },
        3 => Ok(ProcessState::Sleeping), // SSLEEP
        4 => Ok(ProcessState::Stopped),  // SSTOP
        5 => Ok(ProcessState::Zombie),   // SZOMB
        other => Ok(ProcessState::Other(other)),
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    pub(super) fn fields(pid: i32) -> Fields {
        match super::super::linux::read_stat(pid) {
            Ok(stat) => Fields {
                state: stat.state.map(linux_state).ok_or(Unavailable::Malformed),
                parent: stat.parent.ok_or(Unavailable::Malformed),
                group: stat.group.ok_or(Unavailable::Malformed),
            },
            Err(e) => Fields::unavailable(e.into()),
        }
    }

    pub(super) fn argv(pid: i32) -> Field<Vec<OsString>> {
        let file =
            std::fs::File::open(format!("/proc/{pid}/cmdline")).map_err(Unavailable::from)?;
        let bytes =
            super::super::linux::read_bounded(file, MAX_ARG_BYTES).map_err(Unavailable::from)?;
        linux_argv(&bytes)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    pub(super) fn fields(pid: i32) -> Fields {
        let bsd = match super::super::platform::bsd_info(pid) {
            Ok(bsd) => bsd,
            Err(e) => return Fields::unavailable(e.into()),
        };
        let state = mac_state(
            bsd.pbi_status,
            if bsd.pbi_status == libc::SRUN {
                running_threads(pid)
            } else {
                Err(Unavailable::Unsupported)
            },
        );
        Fields {
            state,
            parent: i32::try_from(bsd.pbi_ppid).map_err(|_| Unavailable::Malformed),
            group: i32::try_from(bsd.pbi_pgid).map_err(|_| Unavailable::Malformed),
        }
    }

    fn running_threads(pid: i32) -> Field<i32> {
        let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info) as libc::c_int;
        // SAFETY: exact size of a valid writable proc_taskinfo; read-only API.
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut info as *mut libc::proc_taskinfo).cast(),
                size,
            )
        };
        if n == size {
            Ok(info.pti_numrunning)
        } else if n <= 0 {
            Err(super::super::platform::process_read_error(io::Error::last_os_error()).into())
        } else {
            Err(Unavailable::Malformed)
        }
    }

    pub(super) fn argv(pid: i32) -> Field<Vec<OsString>> {
        // Read the kernel maximum rather than the current argv size: a small
        // PROCARGS2 buffer can yield the TAIL of the argument block, not a
        // safely truncated prefix. One full-sized bounded request avoids it.
        let mut argmax: libc::c_int = 0;
        let mut size = std::mem::size_of_val(&argmax);
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: MIB and output pointers refer to correctly sized buffers;
        // newp is null, so this only reads the kernel's argument limit.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut argmax as *mut libc::c_int).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if size != std::mem::size_of_val(&argmax) || argmax <= 0 {
            return Err(Unavailable::Malformed);
        }
        if argmax as usize > MAX_ARG_BYTES {
            return Err(Unavailable::TooLarge);
        }
        let mut bytes = vec![0u8; argmax as usize + 4];
        size = bytes.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: output has `size` bytes; the MIB selects only this PID.
        // No kernel write is requested. The decoder retains only argv.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                bytes.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error().into());
        }
        if size > bytes.len() {
            return Err(Unavailable::TooLarge);
        }
        mac_argv(&bytes[..size])
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::*;
    pub(super) fn fields(_pid: i32) -> Fields {
        Fields {
            state: Err(Unavailable::Unsupported),
            parent: Err(Unavailable::Unsupported),
            group: Err(Unavailable::Unsupported),
        }
    }
    pub(super) fn argv(_pid: i32) -> Field<Vec<OsString>> {
        Err(Unavailable::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::os::unix::ffi::OsStrExt;

    fn birth(usec: u64) -> ProcToken {
        ProcToken::MacStart { sec: 100, usec }
    }

    struct Fake {
        identities: RefCell<VecDeque<Field<ProcToken>>>,
        calls: RefCell<Vec<&'static str>>,
        argv: Field<Vec<OsString>>,
        fields_error: Option<Unavailable>,
    }

    impl Fake {
        fn new(identities: impl IntoIterator<Item = Field<ProcToken>>) -> Self {
            Self {
                identities: RefCell::new(identities.into_iter().collect()),
                calls: RefCell::new(Vec::new()),
                argv: Ok(vec![
                    OsString::from("codex"),
                    OsString::from("--agent"),
                    OsString::from("reviewer"),
                ]),
                fields_error: None,
            }
        }
    }

    impl Source for Fake {
        fn identity(&self, _: i32) -> Field<ProcToken> {
            self.calls.borrow_mut().push("identity");
            self.identities
                .borrow_mut()
                .pop_front()
                .expect("no retry or extra identity reads")
        }
        fn fields(&self, _: i32) -> Fields {
            self.calls.borrow_mut().push("fields");
            match self.fields_error {
                Some(why) => Fields::unavailable(why),
                None => Fields {
                    state: Ok(ProcessState::Sleeping),
                    parent: Ok(7),
                    group: Ok(8),
                },
            }
        }
        fn argv(&self, _: i32) -> Field<Vec<OsString>> {
            self.calls.borrow_mut().push("argv");
            self.argv.clone()
        }
    }

    #[test]
    fn all_details_are_bracketed_by_the_recorded_birth() {
        let s = Fake::new([Ok(birth(1)), Ok(birth(1))]);
        let Inspection::Verified(details) = inspect_with(&s, 42, &birth(1)) else {
            panic!("verified");
        };
        assert_eq!(details.pid, 42);
        assert_eq!(details.birth, birth(1));
        assert_eq!(details.argv.unwrap(), ["codex", "--agent", "reviewer"]);
        assert_eq!(details.state, Ok(ProcessState::Sleeping));
        assert_eq!((details.parent, details.group), (Ok(7), Ok(8)));
        assert_eq!(
            *s.calls.borrow(),
            ["identity", "fields", "argv", "identity"]
        );
    }

    #[test]
    fn a_changed_birth_discards_every_field() {
        for identities in [vec![Ok(birth(2))], vec![Ok(birth(1)), Ok(birth(2))]] {
            let s = Fake::new(identities);
            assert_eq!(inspect_with(&s, 42, &birth(1)), Inspection::Mismatched);
        }
        let s = Fake::new([Ok(ProcToken::LinuxStart {
            ticks: 1,
            boot: "new".into(),
        })]);
        assert_eq!(
            inspect_with(
                &s,
                42,
                &ProcToken::LinuxStart {
                    ticks: 1,
                    boot: "old".into()
                }
            ),
            Inspection::Mismatched
        );
    }

    #[test]
    fn unreadable_or_ended_identity_discards_details_without_guessing_liveness() {
        for (error, outcome) in [
            (Unavailable::Io(io::ErrorKind::NotFound), Inspection::Ended),
            (
                Unavailable::Io(io::ErrorKind::PermissionDenied),
                Inspection::Unavailable(Unavailable::Io(io::ErrorKind::PermissionDenied)),
            ),
            (
                Unavailable::Unsupported,
                Inspection::Unavailable(Unavailable::Unsupported),
            ),
        ] {
            for identities in [vec![Err(error)], vec![Ok(birth(1)), Err(error)]] {
                let s = Fake::new(identities);
                assert_eq!(inspect_with(&s, 42, &birth(1)), outcome);
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_esrch_discards_details_on_either_birth_read() {
        let error = Unavailable::from(super::super::platform::process_read_error(
            io::Error::from_raw_os_error(libc::ESRCH),
        ));
        for identities in [vec![Err(error)], vec![Ok(birth(1)), Err(error)]] {
            let s = Fake::new(identities);
            assert_eq!(inspect_with(&s, 42, &birth(1)), Inspection::Ended);
        }
    }

    #[test]
    fn raw_enoent_discards_details_on_either_birth_read() {
        let error = Unavailable::from(io::Error::from_raw_os_error(libc::ENOENT));
        for identities in [vec![Err(error)], vec![Ok(birth(1)), Err(error)]] {
            let s = Fake::new(identities);
            assert_eq!(inspect_with(&s, 42, &birth(1)), Inspection::Ended);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_mac_inspection_of_a_nonexistent_pid_reports_ended() {
        // Darwin PIDs cannot reach i32::MAX. Exercise the real libproc
        // failure and adapter translation, not a synthetic ErrorKind.
        assert_eq!(inspect(i32::MAX, &birth(1)), Inspection::Ended);
        assert_eq!(super::super::token_for(i32::MAX), None);
    }

    #[test]
    fn unknown_fields_do_not_invalidate_a_verified_process() {
        let denied = Unavailable::Io(io::ErrorKind::PermissionDenied);
        let mut s = Fake::new([Ok(birth(1)), Ok(birth(1))]);
        s.argv = Err(denied);
        let Inspection::Verified(d) = inspect_with(&s, 42, &birth(1)) else {
            panic!("verified");
        };
        assert_eq!(d.argv, Err(denied));
        assert_eq!(d.parent, Ok(7));
        let mut s = Fake::new([Ok(birth(1)), Ok(birth(1))]);
        s.fields_error = Some(denied);
        let Inspection::Verified(d) = inspect_with(&s, 42, &birth(1)) else {
            panic!("verified");
        };
        assert!(d.argv.is_ok());
        assert_eq!(
            (d.state, d.parent, d.group),
            (Err(denied), Err(denied), Err(denied))
        );
    }

    #[test]
    fn invalid_pids_and_unknown_tokens_do_not_reach_the_os() {
        let s = Fake::new([]);
        for pid in [0, -1, i32::MIN] {
            assert_eq!(
                inspect_with(&s, pid, &birth(1)),
                Inspection::Unavailable(Unavailable::InvalidPid)
            );
        }
        assert_eq!(
            inspect_with(&s, 42, &ProcToken::Unrecognised),
            Inspection::Unavailable(Unavailable::Unsupported)
        );
        assert!(s.calls.borrow().is_empty());
    }

    #[test]
    fn linux_arguments_preserve_empty_arguments_and_non_utf8_bytes() {
        let args = linux_argv(b"tool\0\0a b\0\xff\0").unwrap();
        let bytes: Vec<_> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(bytes, [b"tool".as_slice(), b"", b"a b", b"\xff"]);
        assert_eq!(linux_argv(b"\0").unwrap(), [OsString::new()]);
        assert!(linux_argv(b"").unwrap().is_empty());
        assert_eq!(linux_argv(b"torn"), Err(Unavailable::Malformed));
        assert_eq!(
            linux_argv(&vec![0; MAX_ARG_BYTES + 1]),
            Err(Unavailable::TooLarge)
        );
        assert_eq!(
            linux_argv(&vec![0; MAX_ARGS + 1]),
            Err(Unavailable::TooLarge)
        );
    }

    fn mac_buffer(argc: i32, body: &[u8]) -> Vec<u8> {
        let mut bytes = argc.to_ne_bytes().to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn mac_arguments_decode_padding_and_stop_at_argc() {
        let args = mac_argv(&mac_buffer(
            4,
            b"/path/tool\0\0\0tool\0\0a b\0\xff\0ENV=must-not-be-decoded\0",
        ))
        .unwrap();
        let bytes: Vec<_> = args.iter().map(|a| a.as_bytes()).collect();
        assert_eq!(bytes, [b"tool".as_slice(), b"", b"a b", b"\xff"]);
        // Neither trailing data nor its encoding influences the result.
        assert_eq!(
            mac_argv(&mac_buffer(1, b"/tool\0tool\0\xff\xfe")),
            Ok(vec![OsString::from("tool")])
        );
        assert!(mac_argv(&mac_buffer(0, b"anything")).unwrap().is_empty());
    }

    #[test]
    fn mac_argument_buffers_refuse_truncation_bad_counts_and_excessive_inputs() {
        for bytes in [
            vec![1, 0],
            mac_buffer(-1, b""),
            mac_buffer(1, b"no path terminator"),
            mac_buffer(1, b"/path\0\0"),
            mac_buffer(2, b"/path\0tool\0"),
            mac_buffer(1, b"/path\0torn"),
        ] {
            assert_eq!(mac_argv(&bytes), Err(Unavailable::Malformed));
        }
        assert_eq!(
            mac_argv(&mac_buffer(MAX_ARGS as i32 + 1, b"")),
            Err(Unavailable::TooLarge)
        );
        assert_eq!(
            mac_argv(&vec![0; MAX_ARG_BYTES + 5]),
            Err(Unavailable::TooLarge)
        );
    }

    #[test]
    fn linux_states_distinguish_sleep_stop_and_zombie_from_runnable() {
        for (code, expected) in [
            (b'R', ProcessState::Runnable),
            (b'S', ProcessState::Sleeping),
            (b'D', ProcessState::UninterruptibleSleep),
            (b'T', ProcessState::Stopped),
            (b't', ProcessState::TracingStop),
            (b'Z', ProcessState::Zombie),
            (b'X', ProcessState::Dead),
            (b'I', ProcessState::Idle),
            (b'P', ProcessState::Parked),
        ] {
            assert_eq!(linux_state(code), expected);
        }
        assert_eq!(linux_state(b'?'), ProcessState::Other(b'?' as u32));
    }

    #[test]
    fn mac_srun_without_task_information_makes_no_running_claim() {
        let denied = Err(Unavailable::Io(io::ErrorKind::PermissionDenied));
        assert_eq!(mac_state(2, denied), denied.map(|_| ProcessState::Runnable));
        assert_eq!(mac_state(2, Ok(0)), Ok(ProcessState::Sleeping));
        assert_eq!(mac_state(2, Ok(1)), Ok(ProcessState::Runnable));
        assert_eq!(mac_state(2, Ok(-1)), Err(Unavailable::Malformed));
        assert_eq!(mac_state(4, denied), Ok(ProcessState::Stopped));
        assert_eq!(mac_state(5, denied), Ok(ProcessState::Zombie));
        assert_eq!(mac_state(999, denied), Ok(ProcessState::Other(999)));
    }

    #[test]
    fn native_inspection_uses_the_existing_identity_and_a_bounded_command_read() {
        let pid = std::process::id() as i32;
        let token = super::super::token_for(pid).unwrap();
        let Inspection::Verified(d) = inspect(pid, &token) else {
            panic!("current process identity must verify");
        };
        assert_eq!(d.birth, token);
        assert!(d.parent.is_ok());
        assert!(d.group.is_ok());
        // No assertions about argv text, which belongs to the invoking harness.
        assert!(!d.argv.expect("our own arguments are readable").is_empty());
    }
}
