//! The kernel's word that a recorded process has exited, and that one
//! directory's entries changed, delivered from one blocked thread.
//!
//! No polling: the thread sleeps in kqueue (macOS) or epoll over pidfds
//! (Linux) until the kernel reports something. Elsewhere, construction
//! fails and callers observe nothing.

use super::ProcToken;
use super::inspect::{Inspection, ProcessState};
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// What arming a watch established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Armed {
    /// The process is the recorded one and the kernel will report its exit.
    Watching,
    /// The recorded process is proven gone: no such pid, or the pid now
    /// belongs to a process with a different birth.
    Ended,
    /// Neither could be established. Never a claim that it exited.
    Unknown,
}

/// What the watch thread hands its sink.
#[derive(Debug, PartialEq, Eq)]
pub enum Wake<K> {
    /// The process armed under this key exited.
    Exited(K),
    /// An entry of the watched directory was added, removed or replaced.
    Dir,
}

/// One owned watcher thread. Dropping it stops and joins the thread.
pub struct ExitWatch<K> {
    sys: Arc<platform::Sys>,
    ledger: Arc<Mutex<Ledger<K>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl<K: Clone + PartialEq + Send + 'static> ExitWatch<K> {
    /// Starts the thread, watching `dir` if given. `sink` runs on that
    /// thread and must not block.
    pub fn new(
        dir: Option<&Path>,
        mut sink: impl FnMut(Wake<K>) + Send + 'static,
    ) -> io::Result<Self> {
        let sys = Arc::new(platform::Sys::new(dir)?);
        let ledger: Arc<Mutex<Ledger<K>>> = Arc::new(Mutex::new(Ledger::default()));
        let thread = std::thread::Builder::new()
            .name("exit-watch".into())
            .spawn({
                let sys = sys.clone();
                let ledger = ledger.clone();
                move || {
                    while let Some(events) = sys.wait() {
                        for event in events {
                            match event {
                                platform::Event::Exited(reg) => {
                                    let key = ledger.lock().unwrap().take(reg);
                                    if let Some(key) = key {
                                        sink(Wake::Exited(key));
                                    }
                                }
                                platform::Event::Dir => sink(Wake::Dir),
                            }
                        }
                    }
                }
            })?;
        Ok(Self {
            sys,
            ledger,
            thread: Some(thread),
        })
    }

    /// Watch `pid`, which must still be the process born at `birth`.
    /// On `Watching`, its exit is delivered once as `Wake::Exited(key)`.
    pub fn arm(&self, key: K, pid: i32, birth: &ProcToken) -> Armed {
        // Published before the kernel registration, so an exit reported
        // the moment it is registered still finds its key.
        let (reg, displaced) = self.ledger.lock().unwrap().publish(pid, key);
        if let Some(old) = displaced {
            self.sys.deregister(old, pid);
        }
        let armed = arm_with(
            &mut Native {
                sys: &self.sys,
                reg,
            },
            pid,
            birth,
        );
        if armed != Armed::Watching {
            self.ledger.lock().unwrap().take(reg);
        }
        armed
    }

    /// Stop watching under `key`. An exit already queued for it is
    /// discarded when it arrives.
    pub fn disarm(&self, key: &K) {
        let retired = self.ledger.lock().unwrap().retire(key);
        if let Some((reg, pid)) = retired {
            self.sys.deregister(reg, pid);
        }
    }
}

impl<K> Drop for ExitWatch<K> {
    fn drop(&mut self) {
        self.sys.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The live registration per pid, with the id the kernel hands back
/// with its event. A pid can be reused while an old registration's
/// exit is still in flight, so an event is matched by that id, never
/// by pid alone.
struct Ledger<K> {
    next: u64,
    live: HashMap<i32, (u64, K)>,
}

impl<K> Default for Ledger<K> {
    fn default() -> Self {
        Self {
            next: 0,
            live: HashMap::new(),
        }
    }
}

impl<K: PartialEq> Ledger<K> {
    /// A fresh registration id for `pid`, and the one it displaces,
    /// whose kernel registration the caller must remove first.
    fn publish(&mut self, pid: i32, key: K) -> (u64, Option<u64>) {
        self.next += 1;
        let reg = self.next;
        let displaced = self.live.insert(pid, (reg, key)).map(|(old, _)| old);
        (reg, displaced)
    }

    /// The key of `reg`, once, while it is still live.
    fn take(&mut self, reg: u64) -> Option<K> {
        let pid = *self.live.iter().find(|(_, (r, _))| *r == reg)?.0;
        self.live.remove(&pid).map(|(_, key)| key)
    }

    /// Retires the live registration under `key`.
    fn retire(&mut self, key: &K) -> Option<(u64, i32)> {
        let pid = *self.live.iter().find(|(_, (_, k))| k == key)?.0;
        self.live.remove(&pid).map(|(reg, _)| (reg, pid))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Same,
    Gone,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Registered {
    Ok,
    Gone,
    Failed,
}

trait Seam {
    fn seen(&mut self, pid: i32, birth: &ProcToken) -> Seen;
    fn register(&mut self, pid: i32) -> Registered;
    fn deregister(&mut self, pid: i32);
}

/// The identity is checked on both sides of the registration: a pid
/// reused between the first check and the registration would leave
/// the kernel watching the stranger, and the recorded process's exit
/// would never be reported.
fn arm_with(seam: &mut impl Seam, pid: i32, birth: &ProcToken) -> Armed {
    match seam.seen(pid, birth) {
        Seen::Same => {}
        Seen::Gone => return Armed::Ended,
        Seen::Unknown => return Armed::Unknown,
    }
    match seam.register(pid) {
        Registered::Ok => {}
        Registered::Gone => return Armed::Ended,
        Registered::Failed => return Armed::Unknown,
    }
    match seam.seen(pid, birth) {
        Seen::Same => Armed::Watching,
        Seen::Gone => {
            seam.deregister(pid);
            Armed::Ended
        }
        Seen::Unknown => {
            seam.deregister(pid);
            Armed::Unknown
        }
    }
}

struct Native<'a> {
    sys: &'a platform::Sys,
    reg: u64,
}

impl Seam for Native<'_> {
    fn seen(&mut self, pid: i32, birth: &ProcToken) -> Seen {
        match super::inspect::inspect(pid, birth) {
            Inspection::Verified(details)
                if matches!(details.state, Ok(ProcessState::Zombie | ProcessState::Dead)) =>
            {
                Seen::Gone
            }
            Inspection::Verified(_) => Seen::Same,
            Inspection::Mismatched | Inspection::Ended => Seen::Gone,
            Inspection::Unavailable(_) => Seen::Unknown,
        }
    }
    fn register(&mut self, pid: i32) -> Registered {
        self.sys.register(self.reg, pid)
    }
    fn deregister(&mut self, pid: i32) {
        self.sys.deregister(self.reg, pid)
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Registered;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(super) struct Sys {
        kq: OwnedFd,
        _dir: Option<OwnedFd>,
        stopping: AtomicBool,
    }

    const WAKE: usize = 0;

    fn change(ident: usize, filter: i16, flags: u16, fflags: u32) -> libc::kevent {
        libc::kevent {
            ident,
            filter,
            flags,
            fflags,
            data: 0,
            udata: std::ptr::null_mut(),
        }
    }

    pub(super) enum Event {
        Exited(u64),
        Dir,
    }

    impl Sys {
        fn apply(&self, ev: libc::kevent) -> io::Result<()> {
            // SAFETY: one valid change, no event buffer.
            let n = unsafe {
                libc::kevent(
                    self.kq.as_raw_fd(),
                    &ev,
                    1,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn new(dir: Option<&Path>) -> io::Result<Self> {
            // SAFETY: plain syscall; the fd is owned immediately.
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let kq = unsafe { OwnedFd::from_raw_fd(raw) };
            let dir = dir
                .map(|dir| {
                    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
                    // SAFETY: a valid C path; the fd is owned immediately.
                    let raw =
                        unsafe { libc::open(path.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
                    if raw < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
                })
                .transpose()?;
            let sys = Self {
                kq,
                _dir: None,
                stopping: AtomicBool::new(false),
            };
            sys.apply(change(
                WAKE,
                libc::EVFILT_USER,
                libc::EV_ADD | libc::EV_CLEAR,
                0,
            ))?;
            if let Some(fd) = &dir {
                sys.apply(change(
                    fd.as_raw_fd() as usize,
                    libc::EVFILT_VNODE,
                    libc::EV_ADD | libc::EV_CLEAR,
                    libc::NOTE_WRITE,
                ))?;
            }
            Ok(Self { _dir: dir, ..sys })
        }

        /// `reg` rides in `udata` and comes back with the event.
        pub(super) fn register(&self, reg: u64, pid: i32) -> Registered {
            let mut ev = change(
                pid as usize,
                libc::EVFILT_PROC,
                libc::EV_ADD | libc::EV_ONESHOT,
                libc::NOTE_EXIT,
            );
            ev.udata = reg as usize as *mut libc::c_void;
            match self.apply(ev) {
                Ok(()) => Registered::Ok,
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Registered::Gone,
                Err(_) => Registered::Failed,
            }
        }

        /// kqueue holds one registration per pid; the ledger calls this
        /// only for the registration that still owns it.
        pub(super) fn deregister(&self, _reg: u64, pid: i32) {
            let _ = self.apply(change(pid as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0));
        }

        pub(super) fn stop(&self) {
            self.stopping.store(true, Ordering::Release);
            let _ = self.apply(change(WAKE, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER));
        }

        /// Blocks until the kernel reports something; `None` once stopped.
        pub(super) fn wait(&self) -> Option<Vec<Event>> {
            loop {
                let mut out: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
                // SAFETY: `out` is a valid buffer of the stated length.
                let n = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        out.as_mut_ptr(),
                        out.len() as libc::c_int,
                        std::ptr::null(),
                    )
                };
                if self.stopping.load(Ordering::Acquire) {
                    return None;
                }
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return None;
                }
                let events: Vec<Event> = out[..n as usize]
                    .iter()
                    .filter_map(|ev| match ev.filter {
                        libc::EVFILT_PROC if ev.fflags & libc::NOTE_EXIT != 0 => {
                            Some(Event::Exited(ev.udata as usize as u64))
                        }
                        libc::EVFILT_VNODE => Some(Event::Dir),
                        _ => None,
                    })
                    .collect();
                if !events.is_empty() {
                    return Some(events);
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::Registered;
    use std::collections::HashMap;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(super) enum Event {
        Exited(u64),
        Dir,
    }

    pub(super) struct Sys {
        epoll: OwnedFd,
        wake: OwnedFd,
        dir: Option<OwnedFd>,
        pidfds: Mutex<HashMap<u64, OwnedFd>>,
        stopping: AtomicBool,
    }

    const WAKE: u64 = u64::MAX;
    const DIR: u64 = u64::MAX - 1;

    fn owned(raw: libc::c_int) -> io::Result<OwnedFd> {
        if raw < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: a fresh fd returned by the kernel to this process.
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    }

    impl Sys {
        fn add(&self, fd: libc::c_int, data: u64) -> io::Result<()> {
            let mut ev = libc::epoll_event {
                events: libc::EPOLLIN as u32,
                u64: data,
            };
            // SAFETY: valid fds and event.
            let n = unsafe {
                libc::epoll_ctl(self.epoll.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev)
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }

        pub(super) fn new(dir: Option<&Path>) -> io::Result<Self> {
            let epoll = owned(unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) })?;
            let wake = owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
            let dir = dir
                .map(|dir| -> io::Result<OwnedFd> {
                    let fd = owned(unsafe {
                        libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK)
                    })?;
                    let path = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())
                        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
                    let mask = libc::IN_CREATE
                        | libc::IN_DELETE
                        | libc::IN_MOVED_TO
                        | libc::IN_MOVED_FROM
                        | libc::IN_CLOSE_WRITE;
                    // SAFETY: a valid inotify fd and C path.
                    if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), path.as_ptr(), mask) } < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(fd)
                })
                .transpose()?;
            let sys = Self {
                epoll,
                wake,
                dir,
                pidfds: Mutex::default(),
                stopping: AtomicBool::new(false),
            };
            sys.add(sys.wake.as_raw_fd(), WAKE)?;
            if let Some(fd) = &sys.dir {
                sys.add(fd.as_raw_fd(), DIR)?;
            }
            Ok(sys)
        }

        /// `reg` is the epoll data and comes back with the event.
        pub(super) fn register(&self, reg: u64, pid: i32) -> Registered {
            // SAFETY: plain syscall.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as libc::c_int;
            let fd = match owned(raw) {
                Ok(fd) => fd,
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Registered::Gone,
                Err(_) => return Registered::Failed,
            };
            if self.add(fd.as_raw_fd(), reg).is_err() {
                return Registered::Failed;
            }
            self.pidfds.lock().unwrap().insert(reg, fd);
            Registered::Ok
        }

        /// Closing the pidfd removes it from the epoll set.
        pub(super) fn deregister(&self, reg: u64, _pid: i32) {
            self.pidfds.lock().unwrap().remove(&reg);
        }

        pub(super) fn stop(&self) {
            self.stopping.store(true, Ordering::Release);
            let one: u64 = 1;
            // SAFETY: an 8-byte write to an eventfd.
            unsafe {
                libc::write(
                    self.wake.as_raw_fd(),
                    &one as *const u64 as *const libc::c_void,
                    8,
                )
            };
        }

        fn drain(fd: &OwnedFd) {
            let mut buf = [0u8; 4096];
            // SAFETY: reads into a valid buffer; the fd is non-blocking.
            while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
        }

        pub(super) fn wait(&self) -> Option<Vec<Event>> {
            loop {
                let mut out: [libc::epoll_event; 16] = unsafe { std::mem::zeroed() };
                // SAFETY: `out` is a valid buffer of the stated length.
                let n = unsafe {
                    libc::epoll_wait(
                        self.epoll.as_raw_fd(),
                        out.as_mut_ptr(),
                        out.len() as i32,
                        -1,
                    )
                };
                if self.stopping.load(Ordering::Acquire) {
                    return None;
                }
                if n < 0 {
                    if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return None;
                }
                let mut events = Vec::new();
                for ev in &out[..n as usize] {
                    match ev.u64 {
                        WAKE => Self::drain(&self.wake),
                        DIR => {
                            if let Some(fd) = &self.dir {
                                Self::drain(fd);
                            }
                            events.push(Event::Dir);
                        }
                        reg => {
                            // A pidfd stays readable after exit; drop it so it
                            // reports once.
                            self.pidfds.lock().unwrap().remove(&reg);
                            events.push(Event::Exited(reg));
                        }
                    }
                }
                if !events.is_empty() {
                    return Some(events);
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod platform {
    use super::Registered;
    use std::io;
    use std::path::Path;

    pub(super) enum Event {
        Exited(u64),
        Dir,
    }

    pub(super) struct Sys;

    impl Sys {
        pub(super) fn new(_dir: Option<&Path>) -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub(super) fn register(&self, _reg: u64, _pid: i32) -> Registered {
            Registered::Failed
        }
        pub(super) fn deregister(&self, _reg: u64, _pid: i32) {}
        pub(super) fn stop(&self) {}
        pub(super) fn wait(&self) -> Option<Vec<Event>> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    struct Script {
        seen: Vec<Seen>,
        register: Registered,
        log: Vec<&'static str>,
    }

    impl Seam for Script {
        fn seen(&mut self, _pid: i32, _birth: &ProcToken) -> Seen {
            self.log.push("seen");
            self.seen.remove(0)
        }
        fn register(&mut self, _pid: i32) -> Registered {
            self.log.push("register");
            self.register
        }
        fn deregister(&mut self, _pid: i32) {
            self.log.push("deregister");
        }
    }

    fn script(seen: &[Seen], register: Registered) -> Script {
        Script {
            seen: seen.to_vec(),
            register,
            log: Vec::new(),
        }
    }

    fn birth() -> ProcToken {
        ProcToken::MacStart { sec: 1, usec: 2 }
    }

    #[test]
    fn a_pid_reused_during_registration_is_the_recorded_process_ended() {
        let mut s = script(&[Seen::Same, Seen::Gone], Registered::Ok);
        assert_eq!(arm_with(&mut s, 7, &birth()), Armed::Ended);
        assert_eq!(s.log, ["seen", "register", "seen", "deregister"]);
    }

    #[test]
    fn registration_ordering_outcomes() {
        let cases = [
            (
                vec![Seen::Same, Seen::Same],
                Registered::Ok,
                Armed::Watching,
            ),
            (vec![Seen::Gone], Registered::Ok, Armed::Ended),
            (vec![Seen::Unknown], Registered::Ok, Armed::Unknown),
            (vec![Seen::Same], Registered::Gone, Armed::Ended),
            (vec![Seen::Same], Registered::Failed, Armed::Unknown),
            (
                vec![Seen::Same, Seen::Unknown],
                Registered::Ok,
                Armed::Unknown,
            ),
        ];
        for (seen, register, want) in cases {
            let mut s = script(&seen, register);
            assert_eq!(arm_with(&mut s, 7, &birth()), want, "{seen:?} {register:?}");
            assert!(s.seen.is_empty(), "every scripted check was consulted");
        }
    }

    fn child() -> (std::process::Child, ProcToken) {
        let child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let birth = crate::proc_identity::token_for(child.id() as i32).unwrap();
        (child, birth)
    }

    #[test]
    fn a_real_childs_exit_is_delivered_once() {
        let (tx, rx) = mpsc::channel();
        let watch = ExitWatch::new(None, move |w| {
            let _ = tx.send(w);
        })
        .unwrap();
        let (mut child, birth) = child();
        let pid = child.id() as i32;
        assert_eq!(watch.arm("k", pid, &birth), Armed::Watching);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Wake::Exited("k")
        );
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "once");
    }

    #[test]
    fn a_gone_pid_or_a_wrong_birth_is_ended_at_once() {
        let watch = ExitWatch::<&str>::new(None, |_| {}).unwrap();
        let (mut child, birth) = child();
        let pid = child.id() as i32;
        let stranger = ProcToken::MacStart { sec: 1, usec: 1 };
        let stranger = match birth {
            ProcToken::LinuxStart { ref boot, .. } => ProcToken::LinuxStart {
                ticks: 1,
                boot: boot.clone(),
            },
            _ => stranger,
        };
        assert_eq!(watch.arm("k", pid, &stranger), Armed::Ended);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(watch.arm("k", pid, &birth), Armed::Ended);
    }

    #[test]
    fn a_disarmed_exit_is_not_delivered() {
        let (tx, rx) = mpsc::channel();
        let watch = ExitWatch::new(None, move |w| {
            let _ = tx.send(w);
        })
        .unwrap();
        let (mut child, birth) = child();
        assert_eq!(watch.arm("k", child.id() as i32, &birth), Armed::Watching);
        watch.disarm(&"k");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn a_directory_entry_change_is_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, rx) = mpsc::channel();
        let _watch = ExitWatch::<()>::new(Some(dir.path()), move |w| {
            let _ = tx.send(w);
        })
        .unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), Wake::Dir);
    }

    #[test]
    fn dropping_the_watch_ends_its_thread() {
        let watch = ExitWatch::<()>::new(None, |_| {}).unwrap();
        let thread = watch.thread.as_ref().unwrap().thread().id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            drop(watch);
            let _ = tx.send(thread);
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("drop joined the watch thread");
    }

    #[test]
    fn an_old_exit_dispatched_after_a_rearm_on_its_reused_pid_is_ignored() {
        let mut ledger = Ledger::default();
        let (old, _) = ledger.publish(7, "old");
        assert_eq!(ledger.retire(&"old"), Some((old, 7)));
        let (new, displaced) = ledger.publish(7, "new");
        assert_eq!(displaced, None);
        assert_eq!(ledger.take(old), None, "the old registration's event");
        assert_eq!(ledger.take(new), Some("new"), "the replacement survives");
        assert_eq!(ledger.take(new), None, "and is delivered once");
    }

    #[test]
    fn a_displaced_registration_neither_delivers_nor_tears_down_its_successor() {
        let mut ledger = Ledger::default();
        let (old, _) = ledger.publish(7, "old");
        let (new, displaced) = ledger.publish(7, "new");
        assert_eq!(displaced, Some(old));
        assert_eq!(ledger.take(old), None);
        assert_eq!(
            ledger.retire(&"old"),
            None,
            "no kernel registration to remove"
        );
        assert_eq!(ledger.retire(&"new"), Some((new, 7)));
    }

    #[test]
    fn exits_racing_registration_are_delivered_at_most_once_and_once_when_watching() {
        let (tx, rx) = mpsc::channel();
        let watch = ExitWatch::new(None, move |w| {
            let _ = tx.send(w);
        })
        .unwrap();
        let mut watching = Vec::new();
        for i in 0..40usize {
            let (mut child, birth) = child();
            let pid = child.id() as i32;
            let killer = std::thread::spawn(move || {
                child.kill().unwrap();
                child.wait().unwrap();
            });
            if watch.arm(i, pid, &birth) == Armed::Watching {
                watching.push(i);
            }
            killer.join().unwrap();
        }
        let mut delivered = std::collections::HashMap::<usize, usize>::new();
        while let Ok(Wake::Exited(i)) = rx.recv_timeout(Duration::from_millis(500)) {
            *delivered.entry(i).or_default() += 1;
        }
        assert!(
            delivered.values().all(|n| *n == 1),
            "at most once: {delivered:?}"
        );
        for i in watching {
            assert_eq!(delivered.get(&i), Some(&1), "a watched exit is delivered");
        }
    }
}
