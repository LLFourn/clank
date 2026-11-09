//! The one owner of process mechanics.
//!
//! Nothing else in production names `Command`, `Child` or a raw spawn
//! (`tests/it/process_boundary.rs` enforces it). Every child goes
//! through [`Spec`], and every spawn marks each descriptor above 2
//! close-on-exec before the program starts, so no child inherits a pipe
//! it was not given. A child that inherited another call's output pipe
//! once wedged the status TUI's reconcile worker forever: Rust std on
//! macOS sets `FD_CLOEXEC` on new pipes in a separate step, and a fork
//! on another thread inside that window kept the write end alive in a
//! long-lived monitor.
//!
//! macOS spawns with `posix_spawn` and `POSIX_SPAWN_CLOEXEC_DEFAULT`,
//! which closes inherited descriptors atomically in the kernel, with no
//! pre-exec pass and no descriptor listing.
//!
//! Nothing stops a spawn in flight, on any thread and either platform,
//! briefly holding a copy of every descriptor this process has open
//! until its exec closes them: flock leases, and the write end of
//! another run's output pipe. `clank_user_config::lease::take` outwaits
//! that for a lease, and [`HELD_PATIENCE`] for a run's output.
//!
//! Lifetimes, one entry point each: [`run`] / [`run_blocking`] for a
//! bounded query or action, [`start`] for a child that is owned while
//! it runs, [`interactive`] for the terminal, [`exec`] to become the
//! program.

use rustix::event::{PollFd, PollFlags};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
#[cfg(not(target_os = "macos"))]
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::signal::{Pgid, Pid};

/// Whose agent identity the child may see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// The environment as inherited, plus the spec's changes.
    Inherit,
    /// The inherited session identity removed first, then the spec's
    /// env applied, so an identity the spec sets survives.
    ScrubInherited,
    /// No session identity at all, even one the spec's env sets.
    Forbid,
}

/// The child's standard input.
#[derive(Debug)]
pub enum Stdin {
    Null,
    Inherit,
    /// A pipe whose write end the caller takes from the [`Managed`].
    Piped,
    /// This descriptor, mapped onto 0.
    Fd(OwnedFd),
}

/// The child's standard output or error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Out {
    Null,
    Inherit,
    /// Kept whole, up to `cap` bytes; more is [`RunError::Overflow`].
    Capture {
        cap: usize,
    },
    /// A pipe the caller takes from the [`Managed`].
    Piped,
    /// Only the last `bytes` are kept.
    Ring {
        bytes: usize,
    },
}

/// Which process group the child runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// A group of its own, led by the child; ending the child ends it.
    New,
    /// A group someone else owns, who also owns signalling and
    /// collecting its members.
    Join(Pgid),
    /// The caller's group, which keeps the terminal's signals reaching it.
    Inherit,
    /// [`Group::New`] in a session of its own, off the controlling
    /// terminal, so neither its signals nor its hangup reach the child.
    Session,
}

/// A command to run. Defaults: null stdio, the caller's group and
/// environment.
#[derive(Debug)]
pub struct Spec {
    program: OsString,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
    env_remove: Vec<OsString>,
    identity: Identity,
    stdin: Stdin,
    stdout: Out,
    stderr: Out,
    group: Group,
    keep_fds: Vec<RawFd>,
    #[cfg(test)]
    pub(crate) inject: Inject,
}

/// Faults a test injects into one spawn.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub(crate) struct Inject {
    /// The errno the forked child's descriptor marking fails with.
    #[cfg(not(target_os = "macos"))]
    pub(crate) mark_errno: Option<i32>,
    /// How late a run's reader starts.
    pub(crate) reader_delay: Option<Duration>,
}

impl Spec {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            identity: Identity::Inherit,
            stdin: Stdin::Null,
            stdout: Out::Null,
            stderr: Out::Null,
            group: Group::Inherit,
            keep_fds: Vec::new(),
            #[cfg(test)]
            inject: Inject::default(),
        }
    }

    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_owned());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: AsRef<OsStr>>(mut self, args: I) -> Self {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self
    }

    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env
            .push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (k, v) in vars {
            self = self.env(k, v);
        }
        self
    }

    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        self.env_remove.push(key.as_ref().to_owned());
        self
    }

    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = identity;
        self
    }

    pub fn stdin(mut self, stdin: Stdin) -> Self {
        self.stdin = stdin;
        self
    }

    pub fn stdout(mut self, out: Out) -> Self {
        self.stdout = out;
        self
    }

    pub fn stderr(mut self, out: Out) -> Self {
        self.stderr = out;
        self
    }

    pub fn group(mut self, group: Group) -> Self {
        self.group = group;
        self
    }

    /// Let the child inherit `fd`, which every spawn otherwise closes.
    pub fn keep_fd(mut self, fd: RawFd) -> Self {
        self.keep_fds.push(fd);
        self
    }

    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    pub fn get_args(&self) -> impl Iterator<Item = &OsStr> {
        self.args.iter().map(OsString::as_os_str)
    }

    pub fn get_stdin(&self) -> &Stdin {
        &self.stdin
    }

    pub fn get_stdout(&self) -> Out {
        self.stdout
    }

    pub fn get_stderr(&self) -> Out {
        self.stderr
    }

    pub fn get_group(&self) -> Group {
        self.group
    }

    /// The command line, shell-quoted, for messages.
    pub fn display(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|a| crate::shell_quote::shell_quote(&a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The environment the child would get.
    #[cfg(test)]
    pub(crate) fn get_environment(&self) -> std::collections::BTreeMap<OsString, OsString> {
        self.environment()
    }

    /// The child's environment, with the identity precedence applied.
    fn environment(&self) -> std::collections::BTreeMap<OsString, OsString> {
        let mut env: std::collections::BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        let identity = crate::agent_env::SESSION_IDENTITY_VARS;
        if self.identity == Identity::ScrubInherited {
            for var in identity {
                env.remove(OsStr::new(var));
            }
        }
        for key in &self.env_remove {
            env.remove(key);
        }
        for (k, v) in &self.env {
            env.insert(k.clone(), v.clone());
        }
        if self.identity == Identity::Forbid {
            for var in identity {
                env.remove(OsStr::new(var));
            }
        }
        env
    }

    fn spawn(&self) -> io::Result<Spawned> {
        #[cfg(target_os = "macos")]
        return apple::spawn(self, false);
        #[cfg(not(target_os = "macos"))]
        {
            let mut child = self.command()?.spawn()?;
            Ok(Spawned {
                pid: child.id() as i32,
                stdin: child.stdin.take().map(OwnedFd::from),
                stdout: child.stdout.take().map(OwnedFd::from),
                stderr: child.stderr.take().map(OwnedFd::from),
            })
        }
    }

    /// The `std` command, with a hook that marks every inherited
    /// descriptor above 2 close-on-exec in the forked child.
    ///
    /// Unsafe because `pre_exec` is: no safe API runs code in the child
    /// between fork and exec.
    #[cfg(not(target_os = "macos"))]
    #[allow(unsafe_code)]
    fn command(&self) -> io::Result<std::process::Command> {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        cmd.env_clear().envs(self.environment());
        cmd.stdin(match &self.stdin {
            Stdin::Null => std::process::Stdio::null(),
            Stdin::Inherit => std::process::Stdio::inherit(),
            Stdin::Piped => std::process::Stdio::piped(),
            Stdin::Fd(fd) => std::process::Stdio::from(fd.try_clone()?),
        });
        cmd.stdout(stdio(self.stdout));
        cmd.stderr(stdio(self.stderr));
        match self.group {
            Group::New => {
                cmd.process_group(0);
            }
            Group::Join(pgid) => {
                cmd.process_group(pgid.get());
            }
            Group::Inherit | Group::Session => {}
        }
        let mut keep = self.keep_fds.clone();
        keep.sort_unstable();
        keep.dedup();
        let mut shed = Shed {
            session: self.group == Group::Session,
            keep,
            #[cfg(test)]
            mark_errno: self.inject.mark_errno,
        };
        // SAFETY: `Shed::run` makes only async-signal-safe calls and
        // allocates nothing: `keep` was built before the fork.
        unsafe {
            cmd.pre_exec(move || shed.run());
        }
        Ok(cmd)
    }
}

/// A started child: its pid and the parent's ends of its pipes.
struct Spawned {
    pid: i32,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
}

#[cfg(not(target_os = "macos"))]
fn stdio(out: Out) -> std::process::Stdio {
    match out {
        Out::Null => std::process::Stdio::null(),
        Out::Inherit => std::process::Stdio::inherit(),
        Out::Capture { .. } | Out::Piped | Out::Ring { .. } => std::process::Stdio::piped(),
    }
}

/// What the forked child does before exec: mark every descriptor above
/// 2 close-on-exec, except the ones the spec passes on.
#[cfg(not(target_os = "macos"))]
struct Shed {
    session: bool,
    keep: Vec<RawFd>,
    #[cfg(test)]
    mark_errno: Option<i32>,
}

#[cfg(not(target_os = "macos"))]
impl Shed {
    /// Unsafe because the descriptors to pass on are numbers the spec
    /// names, which no type can vouch for in the forked child.
    #[allow(unsafe_code)]
    fn run(&mut self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(errno) = self.mark_errno.take() {
            return Err(io::Error::from_raw_os_error(errno));
        }
        if self.session {
            rustix::process::setsid()?;
        }
        self.mark()?;
        for &fd in &self.keep {
            // SAFETY: F_SETFD only clears a descriptor flag, and a number
            // that is not open fails with EBADF rather than touching
            // memory.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } == -1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// `close_range` with `CLOSE_RANGE_CLOEXEC` covers every number in
    /// a range whatever the limit; a kernel without it (before 5.11)
    /// fails the spawn rather than fall back to a partial loop.
    #[cfg(target_os = "linux")]
    fn mark(&mut self) -> io::Result<()> {
        let mut lo: libc::c_uint = 3;
        for &fd in &self.keep {
            let Ok(fd) = libc::c_uint::try_from(fd) else {
                continue;
            };
            if fd < lo {
                continue;
            }
            if fd > lo {
                cloexec_range(lo, fd - 1)?;
            }
            lo = fd + 1;
        }
        cloexec_range(lo, libc::c_uint::MAX)
    }

    #[cfg(not(target_os = "linux"))]
    fn mark(&mut self) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }
}

/// Unsafe because neither std nor rustix wraps `close_range`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn cloexec_range(lo: libc::c_uint, hi: libc::c_uint) -> io::Result<()> {
    // SAFETY: with CLOSE_RANGE_CLOEXEC, close_range only sets a descriptor
    // flag and takes no pointers.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, lo, hi, libc::CLOSE_RANGE_CLOEXEC) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// macOS spawns with `posix_spawn` and `POSIX_SPAWN_CLOEXEC_DEFAULT`,
/// which closes every descriptor the spawn does not name inside the
/// kernel, with no user-space fork window.
#[cfg(target_os = "macos")]
mod apple {
    use super::{Group, Out, Spawned, Spec, Stdin};
    use std::ffi::{CString, OsStr};
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const POSIX_SPAWN_SETSID: libc::c_int = 0x0400;

    // SAFETY: Apple's spawn.h declares both with these C types.
    #[allow(unsafe_code)]
    unsafe extern "C" {
        fn posix_spawn_file_actions_addinherit_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            fd: libc::c_int,
        ) -> libc::c_int;
        fn posix_spawn_file_actions_addchdir_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            path: *const libc::c_char,
        ) -> libc::c_int;
    }

    fn check(rc: libc::c_int) -> io::Result<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc))
        }
    }

    fn cstring(s: &OsStr) -> io::Result<CString> {
        CString::new(s.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    struct Actions(libc::posix_spawn_file_actions_t);

    impl Drop for Actions {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: initialised in `spawn` before this value exists, and
            // destroyed only here.
            unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
        }
    }

    struct Attr(libc::posix_spawnattr_t);

    impl Drop for Attr {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            // SAFETY: initialised in `spawn` before this value exists, and
            // destroyed only here.
            unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
        }
    }

    /// A pipe, both ends close-on-exec.
    fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let (r, w) = rustix::pipe::pipe()?;
        for fd in [&r, &w] {
            rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
        }
        Ok((r, w))
    }

    /// `program` as execvp in the child would find it: from the child's
    /// directory `cwd`, on the child's own `PATH`, where a relative or
    /// empty entry is read from `cwd` and a match it may not execute is
    /// passed over. `cwd` is relative to this process's directory, which
    /// is asked for only when the child changes directory and the match
    /// is still relative: that directory may have been removed.
    fn resolve(program: &OsStr, path: Option<&OsStr>, cwd: &Path) -> io::Result<CString> {
        let chdir = !cwd.as_os_str().is_empty();
        let pinned = |p: std::path::PathBuf| -> io::Result<CString> {
            if chdir && p.is_relative() {
                return cstring(std::env::current_dir()?.join(p).as_os_str());
            }
            cstring(p.as_os_str())
        };
        if program.as_bytes().contains(&b'/') {
            return pinned(cwd.join(program));
        }
        let mut denied = false;
        for dir in std::env::split_paths(path.unwrap_or(OsStr::new("/usr/bin:/bin"))) {
            let candidate = cwd.join(dir).join(program);
            let Ok(meta) = std::fs::metadata(&candidate) else {
                continue;
            };
            let c = cstring(candidate.as_os_str())?;
            if meta.is_file()
                && rustix::fs::access(c.as_c_str(), rustix::fs::Access::EXEC_OK).is_ok()
            {
                return pinned(candidate);
            }
            denied = true;
        }
        Err(io::Error::from(if denied {
            io::ErrorKind::PermissionDenied
        } else {
            io::ErrorKind::NotFound
        }))
    }

    /// Spawn `spec`, or with `exec` become it; only an error returns then.
    ///
    /// Unsafe because posix_spawn's attributes and file actions are C
    /// objects built through out-pointers, and nothing safe wraps them.
    #[allow(unsafe_code)]
    pub(super) fn spawn(spec: &Spec, exec: bool) -> io::Result<Spawned> {
        let env = spec.environment();
        let path = resolve(
            &spec.program,
            env.get(OsStr::new("PATH")).map(|p| p.as_os_str()),
            spec.cwd.as_deref().unwrap_or(Path::new("")),
        )?;
        let argv: Vec<CString> = std::iter::once(&spec.program)
            .chain(&spec.args)
            .map(|a| cstring(a))
            .collect::<io::Result<_>>()?;
        let envp: Vec<CString> = env
            .iter()
            .map(|(k, v)| {
                let mut kv = k.as_bytes().to_vec();
                kv.push(b'=');
                kv.extend_from_slice(v.as_bytes());
                CString::new(kv).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
            })
            .collect::<io::Result<_>>()?;
        let mut argv_ptrs: Vec<*mut libc::c_char> =
            argv.iter().map(|a| a.as_ptr() as *mut _).collect();
        argv_ptrs.push(std::ptr::null_mut());
        let mut envp_ptrs: Vec<*mut libc::c_char> =
            envp.iter().map(|e| e.as_ptr() as *mut _).collect();
        envp_ptrs.push(std::ptr::null_mut());

        let mut actions = std::mem::MaybeUninit::uninit();
        // SAFETY: initialises the value it is given.
        check(unsafe { libc::posix_spawn_file_actions_init(actions.as_mut_ptr()) })?;
        // SAFETY: initialised just above, which returned success.
        let mut actions = Actions(unsafe { actions.assume_init() });
        let mut attr = std::mem::MaybeUninit::uninit();
        // SAFETY: initialises the value it is given.
        check(unsafe { libc::posix_spawnattr_init(attr.as_mut_ptr()) })?;
        // SAFETY: initialised just above, which returned success.
        let mut attr = Attr(unsafe { attr.assume_init() });

        let a = &mut actions.0;
        let null = c"/dev/null";
        // The child's ends, held open until the spawn has copied them.
        let mut theirs: Vec<OwnedFd> = Vec::new();
        let mut spawned = Spawned {
            pid: 0,
            stdin: None,
            stdout: None,
            stderr: None,
        };
        match &spec.stdin {
            // SAFETY: `a` is initialised and `null` is a C string that
            // outlives the spawn.
            Stdin::Null => check(unsafe {
                libc::posix_spawn_file_actions_addopen(a, 0, null.as_ptr(), libc::O_RDONLY, 0)
            })?,
            // SAFETY: `a` is initialised; the action records a number.
            Stdin::Inherit => check(unsafe { posix_spawn_file_actions_addinherit_np(a, 0) })?,
            Stdin::Piped => {
                let (r, w) = pipe()?;
                // SAFETY: `a` is initialised and `r` stays open in
                // `theirs` until the spawn has copied it.
                check(unsafe { libc::posix_spawn_file_actions_adddup2(a, r.as_raw_fd(), 0) })?;
                theirs.push(r);
                spawned.stdin = Some(w);
            }
            Stdin::Fd(fd) => {
                // SAFETY: `a` is initialised and `spec` holds `fd` open
                // until the spawn.
                check(unsafe { libc::posix_spawn_file_actions_adddup2(a, fd.as_raw_fd(), 0) })?
            }
        }
        for (target, out) in [(1, spec.stdout), (2, spec.stderr)] {
            match out {
                // SAFETY: `a` is initialised and `null` is a C string that
                // outlives the spawn.
                Out::Null => check(unsafe {
                    libc::posix_spawn_file_actions_addopen(
                        a,
                        target,
                        null.as_ptr(),
                        libc::O_WRONLY,
                        0,
                    )
                })?,
                Out::Inherit => {
                    // SAFETY: `a` is initialised; the action records a
                    // number.
                    check(unsafe { posix_spawn_file_actions_addinherit_np(a, target) })?
                }
                Out::Capture { .. } | Out::Piped | Out::Ring { .. } => {
                    let (r, w) = pipe()?;
                    // SAFETY: `a` is initialised and `w` stays open in
                    // `theirs` until the spawn has copied it.
                    check(unsafe {
                        libc::posix_spawn_file_actions_adddup2(a, w.as_raw_fd(), target)
                    })?;
                    theirs.push(w);
                    *(if target == 1 {
                        &mut spawned.stdout
                    } else {
                        &mut spawned.stderr
                    }) = Some(r);
                }
            }
        }
        for &fd in &spec.keep_fds {
            // SAFETY: `a` is initialised; the action records a number, and
            // one that is not open fails the spawn with EBADF.
            check(unsafe { posix_spawn_file_actions_addinherit_np(a, fd) })?;
        }
        let cwd = spec
            .cwd
            .as_ref()
            .map(|d| cstring(d.as_os_str()))
            .transpose()?;
        if let Some(cwd) = &cwd {
            // SAFETY: `a` is initialised and `cwd` is a C string that
            // outlives the spawn.
            check(unsafe { posix_spawn_file_actions_addchdir_np(a, cwd.as_ptr()) })?;
        }

        // Matching std's spawn: an empty mask, and SIGPIPE back at its
        // default, which this process's runtime may have ignored.
        let mut flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
            | libc::POSIX_SPAWN_SETSIGMASK
            | libc::POSIX_SPAWN_SETSIGDEF;
        // SAFETY: `attr` is initialised, and `set` is initialised by
        // `sigemptyset` before anything reads it.
        unsafe {
            let mut set = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            libc::sigemptyset(set.as_mut_ptr());
            check(libc::posix_spawnattr_setsigmask(&mut attr.0, set.as_ptr()))?;
            libc::sigaddset(set.as_mut_ptr(), libc::SIGPIPE);
            check(libc::posix_spawnattr_setsigdefault(
                &mut attr.0,
                set.as_ptr(),
            ))?;
        }
        match spec.group {
            Group::New => {
                flags |= libc::POSIX_SPAWN_SETPGROUP;
                // SAFETY: `attr` is initialised.
                check(unsafe { libc::posix_spawnattr_setpgroup(&mut attr.0, 0) })?;
            }
            Group::Join(pgid) => {
                flags |= libc::POSIX_SPAWN_SETPGROUP;
                // SAFETY: `attr` is initialised.
                check(unsafe { libc::posix_spawnattr_setpgroup(&mut attr.0, pgid.get()) })?;
            }
            Group::Session => flags |= POSIX_SPAWN_SETSID,
            Group::Inherit => {}
        }
        if exec {
            flags |= libc::POSIX_SPAWN_SETEXEC;
        }
        // SAFETY: `attr` is initialised.
        check(unsafe { libc::posix_spawnattr_setflags(&mut attr.0, flags as libc::c_short) })?;

        let mut pid = 0;
        // SAFETY: every pointer is valid for the call and the arrays are
        // null-terminated.
        check(unsafe {
            libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions.0,
                &attr.0,
                argv_ptrs.as_ptr(),
                envp_ptrs.as_ptr(),
            )
        })?;
        drop(theirs);
        spawned.pid = pid;
        Ok(spawned)
    }
}

// ── exit notification ─────────────────────────────────────────────

/// A descriptor that turns readable when a child exits: a pidfd on
/// Linux, a kqueue holding the pid's `NOTE_EXIT` on macOS.
enum ExitFd {
    Watch(OwnedFd),
    /// It had already exited when asked.
    Exited,
}

impl ExitFd {
    #[cfg(target_os = "linux")]
    fn of(pid: i32) -> io::Result<Self> {
        use rustix::process::{Pid, PidfdFlags, pidfd_open};
        let pid = Pid::from_raw(pid).ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        match pidfd_open(pid, PidfdFlags::empty()) {
            Ok(fd) => Ok(Self::Watch(fd)),
            Err(rustix::io::Errno::SRCH) => Ok(Self::Exited),
            Err(e) => Err(e.into()),
        }
    }

    /// Unsafe because `kevent` has no safe form; the change it registers
    /// names a pid, not a descriptor, so nothing has to outlive the
    /// queue.
    #[cfg(target_os = "macos")]
    #[allow(unsafe_code)]
    fn of(pid: i32) -> io::Result<Self> {
        // A kqueue is never inherited across fork.
        let kq = rustix::event::kqueue::kqueue()?;
        let change = libc::kevent {
            ident: pid as usize,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one initialised change naming a pid, and no event buffer.
        let rc = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if rc < 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::ESRCH) => Ok(Self::Exited),
                _ => Err(e),
            };
        }
        Ok(Self::Watch(kq))
    }
}

/// Wait up to `timeout` for any of `fds` to turn readable; `Ok(false)`
/// when the time ran out first.
fn poll_readable(fds: &mut [PollFd<'_>], until: Instant) -> io::Result<bool> {
    loop {
        let left = until
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(i32::MAX as u64));
        let timeout = rustix::event::Timespec {
            tv_sec: left.as_secs() as i64,
            tv_nsec: left.subsec_nanos().into(),
        };
        for p in fds.iter_mut() {
            p.clear_revents();
        }
        match rustix::event::poll(fds, Some(&timeout)) {
            Ok(n) => return Ok(n > 0),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

fn pollfd(fd: &OwnedFd) -> PollFd<'_> {
    PollFd::new(fd, PollFlags::IN)
}

// ── the reaper ────────────────────────────────────────────────────

/// Owns the wait of every child no caller is left to wait for. One
/// blocked thread hears exits from the kernel (`proc_identity`'s exit
/// watch) and only exited pids are then waited for, so a long-lived
/// child never holds up the reaping of a short-lived one. Its threads
/// are not the runtime's: children still registered at shutdown stay
/// registered until the process exits.
struct Reaper {
    watch: Option<crate::proc_identity::exit_watch::ExitWatch<i32>>,
    exited: std::sync::Mutex<std::sync::mpsc::Sender<i32>>,
}

fn reaper() -> &'static Reaper {
    static REAPER: std::sync::OnceLock<Reaper> = std::sync::OnceLock::new();
    REAPER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<i32>();
        let _ = std::thread::Builder::new()
            .name("proc-reaper".into())
            .spawn(move || {
                for pid in rx {
                    reap_blocking(pid);
                }
            });
        let sink = tx.clone();
        let watch = crate::proc_identity::exit_watch::ExitWatch::new(None, move |wake| {
            if let crate::proc_identity::exit_watch::Wake::Exited(pid) = wake {
                let _ = sink.send(pid);
            }
        })
        .ok();
        Reaper {
            watch,
            exited: std::sync::Mutex::new(tx),
        }
    })
}

fn reap_blocking(pid: i32) {
    // `None` or a negative pid would wait for ANY child (or a group),
    // stealing another owner's wait; `Pid::from_raw` only debug-asserts
    // against negatives.
    if pid <= 0 {
        return;
    }
    let Some(child) = rustix::process::Pid::from_raw(pid) else {
        return;
    };
    loop {
        let reaped = rustix::process::waitpid(Some(child), rustix::process::WaitOptions::empty());
        if matches!(reaped, Err(rustix::io::Errno::INTR)) {
            continue;
        }
        #[cfg(test)]
        tests::signals::reaped(pid);
        return;
    }
}

/// Hand `pid`'s wait to the reaper. The caller must own that wait and
/// give it up.
fn adopt(pid: i32) {
    use crate::proc_identity::exit_watch::Armed;
    let r = reaper();
    let armed = match (&r.watch, crate::proc_identity::token_for(pid)) {
        (Some(watch), Some(birth)) => watch.arm(pid, pid, &birth),
        _ => Armed::Unknown,
    };
    match armed {
        Armed::Watching => {}
        Armed::Ended => {
            let _ = r.exited.lock().unwrap_or_else(|e| e.into_inner()).send(pid);
        }
        // No kernel word is coming, so this pid gets a waiter of its
        // own rather than block the shared one.
        Armed::Unknown => {
            let _ = std::thread::Builder::new()
                .name("proc-reaper-one".into())
                .spawn(move || reap_blocking(pid));
        }
    }
}

// ── owned children ────────────────────────────────────────────────

/// How long dropping a killed child waits to reap it before the reaper
/// takes over.
const DROP_REAP: Duration = Duration::from_millis(100);

/// A child this process owns while it runs.
///
/// It signals only while its child is unreaped: until then the zombie
/// keeps the pid, and the group id it leads, from being reused, and
/// after it a signal could reach whoever took the number next.
///
/// Dropping it kills the group it leads ([`Group::New`],
/// [`Group::Session`]), or under [`Group::Inherit`] the child alone,
/// then reaps it within a short bound or hands it to the reaper. Under
/// [`Group::Join`] nothing is signalled and the group's owner collects
/// it.
pub struct Managed {
    pid: i32,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    group: Group,
    status: Option<ExitStatus>,
    /// Someone else owns the wait now.
    released: bool,
    exit_watch: Option<tokio::io::unix::AsyncFd<OwnedFd>>,
}

/// Start `spec` as an owned child.
pub fn start(spec: Spec) -> io::Result<Managed> {
    for out in [spec.stdout, spec.stderr] {
        if matches!(out, Out::Capture { .. } | Out::Ring { .. }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a started child's output is Piped, not captured",
            ));
        }
    }
    Ok(Managed::of(spec.spawn()?, spec.group))
}

impl Managed {
    fn of(spawned: Spawned, group: Group) -> Self {
        #[cfg(test)]
        tests::signals::spawned(spawned.pid);
        Self {
            pid: spawned.pid,
            stdin: spawned.stdin,
            stdout: spawned.stdout,
            stderr: spawned.stderr,
            group,
            status: None,
            released: false,
            exit_watch: None,
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// The group the child runs in, when it is not the caller's.
    pub fn pgid(&self) -> Option<Pgid> {
        match self.group {
            Group::New | Group::Session => Pgid::new(self.pid),
            Group::Join(pgid) => Some(pgid),
            Group::Inherit => None,
        }
    }

    pub fn take_stdin(&mut self) -> Option<std::process::ChildStdin> {
        self.stdin.take().map(Into::into)
    }

    pub fn take_stdout(&mut self) -> Option<std::process::ChildStdout> {
        self.stdout.take().map(Into::into)
    }

    pub fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.stderr.take().map(Into::into)
    }

    /// The piped stdin for async writing. Needs a tokio runtime.
    pub fn take_stdin_async(&mut self) -> Option<tokio::process::ChildStdin> {
        self.take_stdin()
            .and_then(|p| tokio::process::ChildStdin::from_std(p).ok())
    }

    /// The piped stdout for async reading. Needs a tokio runtime.
    pub fn take_stdout_async(&mut self) -> Option<tokio::process::ChildStdout> {
        self.take_stdout()
            .and_then(|p| tokio::process::ChildStdout::from_std(p).ok())
    }

    /// The piped stderr for async reading. Needs a tokio runtime.
    pub fn take_stderr_async(&mut self) -> Option<tokio::process::ChildStderr> {
        self.take_stderr()
            .and_then(|p| tokio::process::ChildStderr::from_std(p).ok())
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let status = self
            .waitpid(rustix::process::WaitOptions::empty())?
            .expect("a blocking wait returns a status");
        self.reaped(status);
        Ok(status)
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.status.is_none()
            && let Some(status) = self.waitpid(rustix::process::WaitOptions::NOHANG)?
        {
            self.reaped(status);
        }
        Ok(self.status)
    }

    fn waitpid(&self, options: rustix::process::WaitOptions) -> io::Result<Option<ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        let pid = rustix::process::Pid::from_raw(self.pid)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ECHILD))?;
        loop {
            match rustix::process::waitpid(Some(pid), options) {
                Ok(reaped) => {
                    return Ok(reaped.map(|(_, status)| ExitStatus::from_raw(status.as_raw())));
                }
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn reaped(&mut self, status: ExitStatus) {
        self.status = Some(status);
        #[cfg(test)]
        tests::signals::reaped(self.pid);
    }

    /// [`Managed::wait`] for at most `timeout`; `None` if it still runs.
    pub fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        if self.try_wait()?.is_some() {
            return Ok(self.status);
        }
        if let ExitFd::Watch(fd) = ExitFd::of(self.pid)?
            && !poll_readable(&mut [pollfd(&fd)], Instant::now() + timeout)?
        {
            return Ok(None);
        }
        self.wait().map(Some)
    }

    /// Wait without blocking the runtime. Cancel-safe.
    pub async fn wait_async(&mut self) -> io::Result<ExitStatus> {
        self.exited_async().await?;
        self.wait()
    }

    /// Resolve once the child has exited, without reaping it, so the
    /// group it leads can still be signalled safely. Cancel-safe.
    pub async fn exited_async(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        if self.exit_watch.is_none()
            && let ExitFd::Watch(fd) = ExitFd::of(self.pid)?
        {
            self.exit_watch = Some(tokio::io::unix::AsyncFd::with_interest(
                fd,
                tokio::io::Interest::READABLE,
            )?);
        }
        if let Some(watch) = &self.exit_watch {
            let _ = watch.readable().await?;
        }
        Ok(())
    }

    /// SIGKILL the group it leads, else the child alone; nothing once
    /// it is reaped.
    pub fn kill(&mut self) {
        if self.status.is_some() {
            return;
        }
        let target = match self.group {
            Group::New | Group::Session => Pgid::new(self.pid).map(Target::Group),
            Group::Join(_) | Group::Inherit => Pid::new(self.pid).map(Target::Process),
        };
        if let Some(target) = target {
            deliver_sigkill(target);
        }
    }

    /// Block until a child known to be exiting is a zombie, without
    /// reaping it: its descriptors are closed by then, and its pid and
    /// group id stay reserved.
    fn await_zombie(&self) -> io::Result<()> {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let pid =
            Pid::from_raw(self.pid).ok_or_else(|| io::Error::from_raw_os_error(libc::ECHILD))?;
        loop {
            match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT,
            ) {
                Ok(_) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// [`Managed::kill`], then reap within `within`; whether it was.
    pub async fn terminate_async(&mut self, within: Duration) -> bool {
        self.kill();
        matches!(
            tokio::time::timeout(within, self.wait_async()).await,
            Ok(Ok(_))
        )
    }

    /// Let it run on past its owner; the reaper owns its wait.
    pub fn detach(mut self) {
        if self.try_wait().ok().flatten().is_none() {
            adopt(self.pid);
        }
        self.released = true;
    }

    /// Give up the wait to the owner of the group the child leads,
    /// which collects its members itself. Nothing is signalled.
    pub fn release_to_group_owner(&mut self) {
        self.released = true;
    }
}

impl Drop for Managed {
    fn drop(&mut self) {
        if self.released || self.status.is_some() || matches!(self.group, Group::Join(_)) {
            return;
        }
        self.kill();
        if !matches!(self.wait_timeout(DROP_REAP), Ok(Some(_))) {
            adopt(self.pid);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Group(Pgid),
    Process(Pid),
}

fn deliver_sigkill(target: Target) {
    #[cfg(test)]
    if !tests::signals::admit(target) {
        return;
    }
    match target {
        Target::Group(pgid) => {
            crate::cli::wait::GroupKill::over(Some(pgid.get() as u32)).terminate();
        }
        Target::Process(pid) => crate::signal::signal_process(pid, libc::SIGKILL),
    }
}

// ── bounded runs ──────────────────────────────────────────────────

/// What a completed run produced.
#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl std::fmt::Display for Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        })
    }
}

/// Why a run produced no [`Output`]. Every variant means the child is
/// gone: killed with its group where it had one, and reaped.
#[derive(Debug)]
pub enum RunError {
    Spawn(io::Error),
    Io(io::Error),
    /// A capture passed its cap; `kept` is its first `cap` bytes.
    Overflow {
        stream: Stream,
        cap: usize,
        kept: Vec<u8>,
    },
    /// It exited, but something still held an output pipe open
    /// [`HELD_PATIENCE`] later, so the output is not known to be whole.
    Incomplete {
        status: ExitStatus,
        stderr: Vec<u8>,
    },
    Timeout {
        budget: Duration,
        stderr: Vec<u8>,
    },
}

impl RunError {
    /// What the child said on stderr before this, lossily.
    pub fn stderr(&self) -> String {
        match self {
            RunError::Incomplete { stderr, .. } | RunError::Timeout { stderr, .. } => {
                String::from_utf8_lossy(stderr).trim().to_string()
            }
            _ => String::new(),
        }
    }

    /// Whether the program never started.
    pub fn not_started(&self) -> bool {
        matches!(self, RunError::Spawn(_))
    }
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(e) => write!(f, "could not start: {e}"),
            RunError::Io(e) => write!(f, "{e}"),
            RunError::Overflow { stream, cap, .. } => {
                write!(f, "its {stream} exceeded {cap} bytes")
            }
            RunError::Incomplete { status, .. } => write!(
                f,
                "exited {status}, but its output was still held open by another process, \
                 so it is not known to be whole"
            ),
            RunError::Timeout { budget, .. } => write!(f, "did not finish within {budget:?}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Where one captured stream goes.
struct Sink {
    stream: Stream,
    /// Held to the end of the run even past EOF, so a descriptor still
    /// registered for readiness is never closed under its registration.
    fd: OwnedFd,
    eof: bool,
    keep: Keep,
}

enum Keep {
    Whole { cap: usize, bytes: Vec<u8> },
    Tail(crate::cli::wait::RingTail),
}

enum Pumped {
    /// Drained for now: the pipe is empty and still held.
    Open,
    Eof,
    Overflow,
    /// This pass's share was read and there is more.
    More,
}

/// Reads one capture may take per pass, so a stream that never runs dry
/// cannot starve the other stream or the deadline.
const READS_PER_PASS: usize = 4;

/// How long after the child's exit a capture that is empty but not at
/// EOF is waited on before its writer is believed to be holding it.
///
/// A concurrent spawn on another thread holds a copy of the pipe's
/// write end from its fork until its exec, so an instant check reads
/// that passing copy as a holder (on Linux, std's fork path failed a
/// dozen tests a CI run that way). The same reasoning and bound as
/// `clank_user_config::lease::PATIENCE`: a copy lets go within it, a
/// descendant that kept the pipe does not. The run's deadline still
/// bounds it.
pub const HELD_PATIENCE: Duration = Duration::from_millis(250);

impl Sink {
    fn new(stream: Stream, out: Out, pipe: Option<OwnedFd>) -> io::Result<Option<Self>> {
        let keep = match out {
            Out::Capture { cap } => Keep::Whole {
                cap,
                bytes: Vec::new(),
            },
            Out::Ring { bytes } => Keep::Tail(crate::cli::wait::RingTail::new(bytes)),
            Out::Null | Out::Inherit => return Ok(None),
            Out::Piped => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a run captures its output; Piped is for a started child",
                ));
            }
        };
        let fd = pipe.ok_or_else(|| io::Error::other("captured output has no pipe"))?;
        set_nonblocking(&fd)?;
        Ok(Some(Self {
            stream,
            fd,
            eof: false,
            keep,
        }))
    }

    fn open(&self) -> bool {
        !self.eof
    }

    /// Read what is there now, without waiting for more, up to one
    /// pass's share.
    fn pump(&mut self) -> io::Result<Pumped> {
        if self.eof {
            return Ok(Pumped::Eof);
        }
        let mut buf = [0u8; 64 * 1024];
        let mut reads = 0;
        loop {
            if reads == READS_PER_PASS {
                return Ok(Pumped::More);
            }
            reads += 1;
            let n = match rustix::io::read(&self.fd, &mut buf) {
                Ok(n) => n,
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::AGAIN) => return Ok(Pumped::Open),
                Err(e) => return Err(e.into()),
            };
            if n == 0 {
                self.eof = true;
                return Ok(Pumped::Eof);
            }
            let got = &buf[..n];
            match &mut self.keep {
                Keep::Whole { cap, bytes } => {
                    let room = *cap - bytes.len();
                    bytes.extend_from_slice(&got[..got.len().min(room)]);
                    if got.len() > room {
                        return Ok(Pumped::Overflow);
                    }
                }
                Keep::Tail(ring) => ring.push(got),
            }
        }
    }

    fn bytes(self) -> Vec<u8> {
        match self.keep {
            Keep::Whole { bytes, .. } => bytes,
            Keep::Tail(ring) => ring.into_string().into_bytes(),
        }
    }

    fn overflow(self) -> RunError {
        let stream = self.stream;
        match self.keep {
            Keep::Whole { cap, bytes } => RunError::Overflow {
                stream,
                cap,
                kept: bytes,
            },
            Keep::Tail(_) => unreachable!("a ring never overflows"),
        }
    }
}

fn set_nonblocking(fd: &OwnedFd) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

/// A run in progress: the child and its two captures.
struct Running {
    child: Managed,
    sinks: [Option<Sink>; 2],
    budget: Duration,
    /// The whole run's one deadline: completion is not accepted past it.
    deadline: Instant,
    #[cfg(test)]
    reader_delay: Option<Duration>,
}

impl Running {
    fn start(spec: Spec, budget: Duration) -> Result<Self, RunError> {
        if matches!(spec.stdin, Stdin::Piped) {
            return Err(RunError::Spawn(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a run has no one to write its stdin",
            )));
        }
        let deadline = Instant::now() + budget;
        let (stdout, stderr) = (spec.stdout, spec.stderr);
        #[cfg(test)]
        let reader_delay = spec.inject.reader_delay;
        let mut child = Managed::of(spec.spawn().map_err(RunError::Spawn)?, spec.group);
        let out = child.stdout.take();
        let err = child.stderr.take();
        let sinks = [
            Sink::new(Stream::Stdout, stdout, out).map_err(RunError::Io)?,
            Sink::new(Stream::Stderr, stderr, err).map_err(RunError::Io)?,
        ];
        Ok(Self {
            child,
            sinks,
            budget,
            deadline,
            #[cfg(test)]
            reader_delay,
        })
    }

    fn reader_delay(&self) -> Option<Duration> {
        #[cfg(test)]
        return self.reader_delay;
        #[cfg(not(test))]
        None
    }

    fn expired(&mut self) -> Result<(), RunError> {
        if Instant::now() >= self.deadline {
            return Err(self.timeout());
        }
        Ok(())
    }

    /// One pass over every open capture; whether any has more to read
    /// right now. An overflow ends the run.
    fn pump_all(&mut self) -> Result<bool, RunError> {
        let mut more = false;
        for i in 0..2 {
            if let Some(sink) = self.sinks[i].as_mut() {
                match sink.pump().map_err(RunError::Io)? {
                    Pumped::Overflow => {
                        let sink = self.sinks[i].take().expect("present");
                        return Err(sink.overflow());
                    }
                    Pumped::More => more = true,
                    Pumped::Open | Pumped::Eof => {}
                }
            }
        }
        Ok(more)
    }

    /// The child has exited: wait until it is a zombie, so every byte
    /// it wrote is in the pipes and its pid and group stay reserved
    /// while completeness is decided. Returns the instant
    /// [`HELD_PATIENCE`] runs out.
    fn exited(&mut self) -> Result<Instant, RunError> {
        self.expired()?;
        self.child.await_zombie().map_err(RunError::Io)?;
        Ok(Instant::now() + HELD_PATIENCE)
    }

    /// Read every capture as far as it goes now. `Some(held)` once
    /// decided: all at EOF is whole, and one still empty and held at
    /// `patience` is held. `None` means wait for a capture or `patience`.
    fn settle(&mut self, patience: Instant) -> Result<Option<bool>, RunError> {
        loop {
            self.expired()?;
            if !self.pump_all()? {
                break;
            }
        }
        if !self.sinks.iter().flatten().any(Sink::open) {
            return Ok(Some(false));
        }
        Ok((Instant::now() >= patience).then_some(true))
    }

    /// Called once completeness is decided: end what is left of the
    /// group, then reap the child.
    fn reap(&mut self) -> Result<ExitStatus, RunError> {
        self.expired()?;
        self.child.kill();
        self.child.wait().map_err(RunError::Io)
    }

    fn finish(mut self, status: ExitStatus, held: bool) -> Result<Output, RunError> {
        let [out, err] = std::mem::take(&mut self.sinks);
        let stderr = err.map(Sink::bytes).unwrap_or_default();
        if held {
            return Err(RunError::Incomplete { status, stderr });
        }
        Ok(Output {
            status,
            stdout: out.map(Sink::bytes).unwrap_or_default(),
            stderr,
        })
    }

    fn timeout(&mut self) -> RunError {
        RunError::Timeout {
            budget: self.budget,
            stderr: self.sinks[1].take().map(Sink::bytes).unwrap_or_default(),
        }
    }
}

/// Run `spec` to completion within `budget`, reading its captures
/// while it runs. Complete means the child exited and every capture
/// then read to EOF within [`HELD_PATIENCE`]. The group it leads is
/// killed once that is decided, taking any descendant with it, and on
/// any error; the child is reaped.
pub async fn run(spec: Spec, budget: Duration) -> Result<Output, RunError> {
    use tokio::io::Interest;
    use tokio::io::unix::AsyncFd;
    let mut r = Running::start(spec, budget)?;
    let deadline = tokio::time::Instant::from_std(r.deadline);
    if let Some(delay) = r.reader_delay() {
        tokio::time::sleep(delay).await;
    }
    let exit = match ExitFd::of(r.child.pid).map_err(RunError::Io)? {
        ExitFd::Watch(fd) => {
            Some(AsyncFd::with_interest(fd, Interest::READABLE).map_err(RunError::Io)?)
        }
        ExitFd::Exited => None,
    };
    let mut watch: [Option<AsyncFd<RawFd>>; 2] = [None, None];
    for (i, sink) in r.sinks.iter().enumerate() {
        if let Some(sink) = sink {
            watch[i] = Some(
                AsyncFd::with_interest(sink.fd.as_raw_fd(), Interest::READABLE)
                    .map_err(RunError::Io)?,
            );
        }
    }
    let unwatch_closed = |watch: &mut [Option<AsyncFd<RawFd>>; 2], sinks: &[Option<Sink>; 2]| {
        for (watched, sink) in watch.iter_mut().zip(sinks) {
            if !sink.as_ref().is_some_and(Sink::open) {
                *watched = None;
            }
        }
    };
    let outcome = async {
        loop {
            r.expired()?;
            let more = r.pump_all()?;
            r.expired()?;
            unwatch_closed(&mut watch, &r.sinks);
            if more {
                tokio::task::yield_now().await;
                continue;
            }
            let Some(exit) = &exit else { break };
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {}
                ready = exit.readable() => {
                    ready.map_err(RunError::Io)?.retain_ready();
                    break;
                }
                ready = readable(&watch[0]) => ready.map_err(RunError::Io)?,
                ready = readable(&watch[1]) => ready.map_err(RunError::Io)?,
            }
        }
        let patience = r.exited()?;
        let held = loop {
            if let Some(held) = r.settle(patience)? {
                break held;
            }
            unwatch_closed(&mut watch, &r.sinks);
            let until = tokio::time::Instant::from_std(patience).min(deadline);
            tokio::select! {
                _ = tokio::time::sleep_until(until) => {}
                ready = readable(&watch[0]) => ready.map_err(RunError::Io)?,
                ready = readable(&watch[1]) => ready.map_err(RunError::Io)?,
            }
        };
        Ok((r.reap()?, held))
    }
    .await;
    drop(watch);
    match outcome {
        Ok((status, held)) => r.finish(status, held),
        Err(e) => {
            r.child.kill();
            let _ = tokio::time::timeout(DROP_REAP, r.child.wait_async()).await;
            Err(e)
        }
    }
}

/// Resolves once `fd` is readable, with its readiness cleared so the
/// caller's read to `EAGAIN` re-arms it; never, for no descriptor.
async fn readable(fd: &Option<tokio::io::unix::AsyncFd<RawFd>>) -> io::Result<()> {
    match fd {
        Some(fd) => {
            fd.readable().await?.clear_ready();
            Ok(())
        }
        None => std::future::pending().await,
    }
}

/// [`run`] for a caller that is not async.
pub fn run_blocking(spec: Spec, budget: Duration) -> Result<Output, RunError> {
    let mut r = Running::start(spec, budget)?;
    if let Some(delay) = r.reader_delay() {
        std::thread::sleep(delay);
    }
    let outcome = (|| {
        let exit = match ExitFd::of(r.child.pid).map_err(RunError::Io)? {
            ExitFd::Watch(fd) => Some(fd),
            ExitFd::Exited => None,
        };
        loop {
            r.expired()?;
            if r.pump_all()? {
                continue;
            }
            r.expired()?;
            let Some(exit) = &exit else { break };
            let mut fds = vec![pollfd(exit)];
            fds.extend(
                r.sinks
                    .iter()
                    .flatten()
                    .filter(|s| s.open())
                    .map(|s| pollfd(&s.fd)),
            );
            poll_readable(&mut fds, r.deadline).map_err(RunError::Io)?;
            if !fds[0].revents().is_empty() {
                break;
            }
        }
        let patience = r.exited()?;
        let held = loop {
            if let Some(held) = r.settle(patience)? {
                break held;
            }
            let mut fds: Vec<_> = r
                .sinks
                .iter()
                .flatten()
                .filter(|s| s.open())
                .map(|s| pollfd(&s.fd))
                .collect();
            poll_readable(&mut fds, patience.min(r.deadline)).map_err(RunError::Io)?;
        };
        Ok((r.reap()?, held))
    })();
    match outcome {
        Ok((status, held)) => r.finish(status, held),
        Err(e) => {
            r.child.kill();
            let _ = r.child.wait_timeout(DROP_REAP);
            Err(e)
        }
    }
}

// ── the terminal and exec ─────────────────────────────────────────

/// Run `spec` on the terminal and wait for it, with no budget: the
/// person at the terminal decides when it ends.
pub fn interactive(spec: Spec) -> io::Result<ExitStatus> {
    start(spec)?.wait()
}

/// Become `spec`'s program, on this process's own stdio. Returns only
/// if that failed, with why.
pub fn exec(spec: Spec) -> io::Error {
    // The redirection would happen to THIS process, which still has
    // to report a failed exec.
    if !matches!(spec.stdin, Stdin::Inherit)
        || spec.stdout != Out::Inherit
        || spec.stderr != Out::Inherit
    {
        return io::Error::new(
            io::ErrorKind::InvalidInput,
            "exec keeps this process's stdio: every stream must be Inherit",
        );
    }
    #[cfg(target_os = "macos")]
    return match apple::spawn(&spec, true) {
        Err(e) => e,
        Ok(_) => unreachable!("exec returned without an error"),
    };
    #[cfg(not(target_os = "macos"))]
    match spec.command() {
        Ok(mut cmd) => cmd.exec(),
        Err(e) => e,
    }
}

/// Starts the web host's tunnel command, which lives in a crate that
/// cannot depend on this one.
pub struct TunnelSpawn;

impl clank_web_cloudflare::tunnel::Spawn for TunnelSpawn {
    fn spawn(
        &self,
        program: &str,
        args: &[String],
    ) -> io::Result<clank_web_cloudflare::tunnel::Spawned> {
        let mut child = start(
            Spec::new(program)
                .args(args)
                .stdout(Out::Piped)
                .stderr(Out::Piped)
                .group(Group::New),
        )?;
        let piped = || io::Error::other("the tunnel command's output was not piped");
        Ok(clank_web_cloudflare::tunnel::Spawned {
            pid: child.pid(),
            stdout: Box::new(child.take_stdout_async().ok_or_else(piped)?),
            stderr: Box::new(child.take_stderr_async().ok_or_else(piped)?),
            child: Box::new(child),
        })
    }
}

impl clank_web_cloudflare::tunnel::Started for Managed {
    fn wait(&mut self) -> futures_util::future::BoxFuture<'_, io::Result<ExitStatus>> {
        Box::pin(self.wait_async())
    }
}

#[cfg(test)]
#[path = "proc_run_tests.rs"]
pub(crate) mod tests;
