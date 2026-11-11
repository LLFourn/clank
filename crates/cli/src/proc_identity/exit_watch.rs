//! The kernel's word that a recorded process has exited, and that one
//! directory's entries changed, delivered from one blocked thread.
//!
//! No polling: the thread sleeps in kqueue (macOS) or epoll over pidfds
//! (Linux) until the kernel reports something. Elsewhere, construction
//! fails and callers observe nothing.

use super::ProcToken;
use super::inspect::Inspection;
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
    /// The kernel wait failed: nothing more will be reported, so the
    /// owner stops or recovers, saying so.
    Failed(String),
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
                    loop {
                        let events = match sys.wait() {
                            Ok(Some(events)) => events,
                            Ok(None) => return,
                            Err(e) => return sink(Wake::Failed(e.to_string())),
                        };
                        for event in events {
                            match event {
                                platform::Event::Exited(reg) => {
                                    let key = ledger.lock().unwrap().take(reg);
                                    if let Some(key) = key {
                                        sink(Wake::Exited(key));
                                    }
                                }
                                platform::Event::Dir => sink(Wake::Dir),
                                platform::Event::Path(_) | platform::Event::Lost => {}
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

    /// Stand in for the kernel wait failing, as a test drives an owner.
    #[cfg(test)]
    pub fn fail_for_test(&self) {
        self.sys.fail();
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
pub(in crate::proc_identity) enum Registered {
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
                if super::inspect::life_from(&details.state) == super::inspect::Life::Gone =>
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
const STOPPING: u8 = 1;
#[cfg(test)]
const FAILING: u8 = 2;

/// What a wait woken by its owner returns: nothing more once stopped,
/// and in a test, the failure standing in for the kernel's.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn ended<E>(ending: u8) -> Option<io::Result<Option<E>>> {
    match ending {
        0 => None,
        STOPPING => Some(Ok(None)),
        _ => Some(Err(io::Error::other("the kernel wait failed"))),
    }
}

/// What one queued inotify record says of the watched paths.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathRecord {
    Changed(u64),
    /// The queue overflowed and records were dropped.
    Lost,
}

/// inotify's `IN_Q_OVERFLOW`, spelled here so the decoder builds, and is
/// tested, where inotify does not exist.
const IN_Q_OVERFLOW: u32 = 0x0000_4000;

/// An overflow belongs to the whole instance and names no watch, so it is
/// read before the descriptor is looked up.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn decode_path_record(
    mask: u32,
    wd: i32,
    watches: &std::collections::HashMap<i32, u64>,
) -> Option<PathRecord> {
    if mask & IN_Q_OVERFLOW != 0 {
        return Some(PathRecord::Lost);
    }
    watches.get(&wd).copied().map(PathRecord::Changed)
}

#[cfg(target_os = "macos")]
pub(in crate::proc_identity) mod platform {
    use super::Registered;
    use std::io;
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::path::Path;
    use std::sync::atomic::{AtomicU8, Ordering};

    /// A descriptor for watching `path` that does not keep its volume
    /// from being unmounted, and reads and writes nothing.
    fn open_to_watch(path: &Path) -> io::Result<OwnedFd> {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_EVTONLY)
            .open(path)?
            .into())
    }

    /// Field order is drop order: the queue closes before the directory
    /// and paths it watches, so a registered descriptor outlives the
    /// queue.
    pub(in crate::proc_identity) struct Sys {
        kq: OwnedFd,
        _dir: Option<OwnedFd>,
        /// The descriptor of each watched path, by registration. One is
        /// closed only by its removal from here, which is also what
        /// removes its registration.
        paths: std::sync::Mutex<std::collections::HashMap<u64, OwnedFd>>,
        /// [`STOPPING`] once stopped; in a test, [`FAILING`] to stand in
        /// for a kernel wait that failed.
        ending: AtomicU8,
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

    pub(in crate::proc_identity) enum Event {
        Exited(u64),
        Dir,
        /// A path watched under this registration changed.
        Path(u64),
        /// Path events were dropped: any watched path may have changed.
        // kqueue keeps each registration's pending state on the
        // registration itself, coalesced, so it has no queue to overflow.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        Lost,
    }

    impl Sys {
        /// Unsafe because `kevent` registers descriptors by number: each
        /// must outlive its registration, which no type can say.
        #[allow(unsafe_code)]
        fn apply(&self, ev: libc::kevent) -> io::Result<()> {
            // SAFETY: one initialised change and no event buffer. The
            // descriptors ever registered are `_dir` and those in `paths`,
            // all owned by `Sys`: `_dir` and `paths` drop after `kq`, and a
            // path's descriptor is closed only on its removal from
            // `paths`, which removes its registration with it. The other
            // filters name a pid or a user event, not a descriptor.
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

        pub(in crate::proc_identity) fn new(dir: Option<&Path>) -> io::Result<Self> {
            let kq = rustix::event::kqueue::kqueue()?;
            let dir = dir.map(open_to_watch).transpose()?;
            let sys = Self {
                kq,
                _dir: None,
                paths: Default::default(),
                ending: AtomicU8::new(0),
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
        pub(in crate::proc_identity) fn register(&self, reg: u64, pid: i32) -> Registered {
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
        pub(in crate::proc_identity) fn deregister(&self, _reg: u64, pid: i32) {
            let _ = self.apply(change(pid as usize, libc::EVFILT_PROC, libc::EV_DELETE, 0));
        }

        /// Watch `path`: a directory's entries, or with `content` a
        /// file's writes as well. Opening is for events only, and the
        /// filters below report no reads.
        pub(in crate::proc_identity) fn register_path(
            &self,
            reg: u64,
            path: &Path,
            content: bool,
        ) -> io::Result<()> {
            let fd = open_to_watch(path)?;
            let mut fflags = libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME;
            if content {
                fflags |= libc::NOTE_EXTEND;
            }
            let mut ev = change(
                fd.as_raw_fd() as usize,
                libc::EVFILT_VNODE,
                libc::EV_ADD | libc::EV_CLEAR,
                fflags,
            );
            ev.udata = reg as usize as *mut libc::c_void;
            let mut paths = self.paths.lock().unwrap();
            self.apply(ev)?;
            paths.insert(reg, fd);
            Ok(())
        }

        /// Closing the descriptor removes its registration.
        pub(in crate::proc_identity) fn deregister_path(&self, reg: u64) {
            self.paths.lock().unwrap().remove(&reg);
        }

        pub(in crate::proc_identity) fn stop(&self) {
            self.end(super::STOPPING);
        }

        #[cfg(test)]
        pub(in crate::proc_identity) fn fail(&self) {
            self.end(super::FAILING);
        }

        fn end(&self, how: u8) {
            self.ending.store(how, Ordering::Release);
            let _ = self.apply(change(WAKE, libc::EVFILT_USER, 0, libc::NOTE_TRIGGER));
        }

        fn ended(&self) -> Option<io::Result<Option<Vec<Event>>>> {
            super::ended(self.ending.load(Ordering::Acquire))
        }

        /// Blocks until the kernel reports something; `None` once stopped.
        ///
        /// Unsafe for the same reason as [`Sys::apply`]: `kevent` has no
        /// safe form, even with no changes to register.
        #[allow(unsafe_code)]
        pub(in crate::proc_identity) fn wait(&self) -> io::Result<Option<Vec<Event>>> {
            loop {
                let mut out = [change(0, 0, 0, 0); 16];
                // SAFETY: no changes are registered, and `out` is a valid
                // buffer of the stated length.
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
                if let Some(ended) = self.ended() {
                    return ended;
                }
                if n < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                let events: Vec<Event> = out[..n as usize]
                    .iter()
                    .filter_map(|ev| match ev.filter {
                        libc::EVFILT_PROC if ev.fflags & libc::NOTE_EXIT != 0 => {
                            Some(Event::Exited(ev.udata as usize as u64))
                        }
                        libc::EVFILT_VNODE if ev.udata.is_null() => Some(Event::Dir),
                        libc::EVFILT_VNODE => Some(Event::Path(ev.udata as usize as u64)),
                        _ => None,
                    })
                    .collect();
                if !events.is_empty() {
                    return Ok(Some(events));
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(in crate::proc_identity) mod platform {
    use super::Registered;
    use rustix::event::epoll;
    use std::collections::HashMap;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU8, Ordering};

    pub(in crate::proc_identity) enum Event {
        Exited(u64),
        Dir,
        /// A path watched under this registration changed.
        Path(u64),
        /// Path events were dropped: any watched path may have changed.
        // kqueue keeps each registration's pending state on the
        // registration itself, coalesced, so it has no queue to overflow.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        Lost,
    }

    pub(in crate::proc_identity) struct Sys {
        epoll: OwnedFd,
        wake: OwnedFd,
        dir: Option<OwnedFd>,
        pidfds: Mutex<HashMap<u64, OwnedFd>>,
        /// One inotify instance for every watched path, and each watch's
        /// registration by its descriptor.
        paths: OwnedFd,
        watches: Mutex<HashMap<i32, u64>>,
        /// [`STOPPING`] once stopped; in a test, [`FAILING`] to stand in
        /// for a kernel wait that failed.
        ending: AtomicU8,
    }

    const WAKE: u64 = u64::MAX;
    const DIR: u64 = u64::MAX - 1;
    const PATHS: u64 = u64::MAX - 2;

    impl Sys {
        fn add(&self, fd: &OwnedFd, data: u64) -> io::Result<()> {
            epoll::add(
                &self.epoll,
                fd,
                epoll::EventData::new_u64(data),
                epoll::EventFlags::IN,
            )?;
            Ok(())
        }

        pub(in crate::proc_identity) fn new(dir: Option<&Path>) -> io::Result<Self> {
            use rustix::event::{EventfdFlags, eventfd};
            use rustix::fs::inotify;
            let epoll = epoll::create(epoll::CreateFlags::CLOEXEC)?;
            let wake = eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?;
            let dir = dir
                .map(|dir| -> io::Result<OwnedFd> {
                    let fd = inotify::init(
                        inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK,
                    )?;
                    let mask = inotify::WatchFlags::CREATE
                        | inotify::WatchFlags::DELETE
                        | inotify::WatchFlags::MOVED_TO
                        | inotify::WatchFlags::MOVED_FROM
                        | inotify::WatchFlags::CLOSE_WRITE;
                    inotify::add_watch(&fd, dir, mask)?;
                    Ok(fd)
                })
                .transpose()?;
            let paths =
                inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?;
            let sys = Self {
                epoll,
                wake,
                dir,
                pidfds: Mutex::default(),
                paths,
                watches: Mutex::default(),
                ending: AtomicU8::new(0),
            };
            sys.add(&sys.wake, WAKE)?;
            sys.add(&sys.paths, PATHS)?;
            if let Some(fd) = &sys.dir {
                sys.add(fd, DIR)?;
            }
            Ok(sys)
        }

        /// Watch `path`: a directory's entries, or with `content` a
        /// file's writes as well. The mask reports no reads or opens.
        pub(in crate::proc_identity) fn register_path(
            &self,
            reg: u64,
            path: &Path,
            content: bool,
        ) -> io::Result<()> {
            use rustix::fs::inotify::WatchFlags;
            let mut mask = WatchFlags::CREATE
                | WatchFlags::DELETE
                | WatchFlags::MOVED_TO
                | WatchFlags::MOVED_FROM
                | WatchFlags::DELETE_SELF
                | WatchFlags::MOVE_SELF;
            if content {
                mask |= WatchFlags::MODIFY | WatchFlags::CLOSE_WRITE;
            }
            let mut watches = self.watches.lock().unwrap();
            let wd = rustix::fs::inotify::add_watch(&self.paths, path, mask)?;
            watches.insert(wd, reg);
            Ok(())
        }

        pub(in crate::proc_identity) fn deregister_path(&self, reg: u64) {
            let mut watches = self.watches.lock().unwrap();
            let wds: Vec<i32> = watches
                .iter()
                .filter(|(_, r)| **r == reg)
                .map(|(wd, _)| *wd)
                .collect();
            for wd in wds {
                watches.remove(&wd);
                let _ = rustix::fs::inotify::remove_watch(&self.paths, wd);
            }
        }

        /// What the queued path events say, each once.
        fn path_events(&self) -> io::Result<Vec<Event>> {
            let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 4096];
            let mut reader = rustix::fs::inotify::Reader::new(&self.paths, &mut buf);
            let mut seen = Vec::new();
            loop {
                match reader.next() {
                    Ok(event) => {
                        let watches = self.watches.lock().unwrap();
                        let decoded =
                            super::decode_path_record(event.events().bits(), event.wd(), &watches);
                        if let Some(decoded) = decoded
                            && !seen.contains(&decoded)
                        {
                            seen.push(decoded);
                        }
                    }
                    Err(rustix::io::Errno::AGAIN) => {
                        return Ok(seen
                            .into_iter()
                            .map(|seen| match seen {
                                super::PathRecord::Changed(reg) => Event::Path(reg),
                                super::PathRecord::Lost => Event::Lost,
                            })
                            .collect());
                    }
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }

        /// `reg` is the epoll data and comes back with the event.
        pub(in crate::proc_identity) fn register(&self, reg: u64, pid: i32) -> Registered {
            use rustix::process::{Pid, PidfdFlags, pidfd_open};
            let Some(pid) = (pid > 0).then(|| Pid::from_raw(pid)).flatten() else {
                return Registered::Failed;
            };
            let fd = match pidfd_open(pid, PidfdFlags::empty()) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::SRCH) => return Registered::Gone,
                Err(_) => return Registered::Failed,
            };
            if self.add(&fd, reg).is_err() {
                return Registered::Failed;
            }
            self.pidfds.lock().unwrap().insert(reg, fd);
            Registered::Ok
        }

        /// Closing the pidfd removes it from the epoll set.
        pub(in crate::proc_identity) fn deregister(&self, reg: u64, _pid: i32) {
            self.pidfds.lock().unwrap().remove(&reg);
        }

        pub(in crate::proc_identity) fn stop(&self) {
            self.end(super::STOPPING);
        }

        #[cfg(test)]
        pub(in crate::proc_identity) fn fail(&self) {
            self.end(super::FAILING);
        }

        fn end(&self, how: u8) {
            self.ending.store(how, Ordering::Release);
            let _ = rustix::io::write(&self.wake, &1u64.to_ne_bytes());
        }

        fn ended(&self) -> Option<io::Result<Option<Vec<Event>>>> {
            super::ended(self.ending.load(Ordering::Acquire))
        }

        fn drain(fd: &OwnedFd) {
            let mut buf = [0u8; 4096];
            while matches!(rustix::io::read(fd, &mut buf), Ok(n) if n > 0) {}
        }

        pub(in crate::proc_identity) fn wait(&self) -> io::Result<Option<Vec<Event>>> {
            let mut out = Vec::with_capacity(16);
            loop {
                out.clear();
                let waited =
                    epoll::wait(&self.epoll, rustix::buffer::spare_capacity(&mut out), None);
                if let Some(ended) = self.ended() {
                    return ended;
                }
                match waited {
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => return Err(e.into()),
                }
                let mut events = Vec::new();
                for ev in &out {
                    match ev.data.u64() {
                        WAKE => Self::drain(&self.wake),
                        DIR => {
                            if let Some(fd) = &self.dir {
                                Self::drain(fd);
                            }
                            events.push(Event::Dir);
                        }
                        PATHS => events.extend(self.path_events()?),
                        reg => {
                            // A pidfd stays readable after exit; drop it so it
                            // reports once.
                            self.pidfds.lock().unwrap().remove(&reg);
                            events.push(Event::Exited(reg));
                        }
                    }
                }
                if !events.is_empty() {
                    return Ok(Some(events));
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(in crate::proc_identity) mod platform {
    use super::Registered;
    use std::io;
    use std::path::Path;

    pub(in crate::proc_identity) enum Event {
        Exited(u64),
        Dir,
        Path(u64),
        #[allow(dead_code)]
        Lost,
    }

    pub(in crate::proc_identity) struct Sys;

    impl Sys {
        pub(in crate::proc_identity) fn new(_dir: Option<&Path>) -> io::Result<Self> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub(in crate::proc_identity) fn register(&self, _reg: u64, _pid: i32) -> Registered {
            Registered::Failed
        }
        pub(in crate::proc_identity) fn deregister(&self, _reg: u64, _pid: i32) {}
        pub(in crate::proc_identity) fn register_path(
            &self,
            _reg: u64,
            _path: &Path,
            _content: bool,
        ) -> io::Result<()> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub(in crate::proc_identity) fn deregister_path(&self, _reg: u64) {}
        pub(in crate::proc_identity) fn stop(&self) {}
        pub(in crate::proc_identity) fn wait(&self) -> io::Result<Option<Vec<Event>>> {
            Ok(None)
        }
        #[cfg(test)]
        pub(in crate::proc_identity) fn fail(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn an_overflow_is_a_loss_whatever_descriptor_it_carries() {
        let watches = std::collections::HashMap::from([(3, 30u64)]);
        assert_eq!(
            decode_path_record(0x100, 3, &watches),
            Some(PathRecord::Changed(30))
        );
        assert_eq!(decode_path_record(0x100, 9, &watches), None);
        for wd in [-1, 3, 9] {
            assert_eq!(
                decode_path_record(IN_Q_OVERFLOW, wd, &watches),
                Some(PathRecord::Lost),
                "wd {wd}"
            );
        }
    }

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
