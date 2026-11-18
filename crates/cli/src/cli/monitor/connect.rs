use std::os::fd::{AsFd, BorrowedFd};
use std::time::Duration;

use rustix::event::PollFlags;

use super::lease::QUIESCE_BUDGET;
use super::published::{Published, published, published_in};
use super::retire::{LEASE_RETIREMENT, SETTLE_BUDGET};
use super::runtime::runtime_dir;
use super::wire::{Envelope, Hello, Note, PROTO, Welcome, Who, admits, obsolete_why, serves};

/// How long attaching may take, start to finish.
///
/// An OVERALL budget, not an idle timeout: a peer that dribbles a byte
/// at a time would extend an inactivity timeout for as long as it
/// liked, and a monitor that accepts and then says nothing would
/// otherwise strand the client forever — no deadline outside it can
/// help if `attach` never returns (codex on af1efd2).
pub(crate) const ATTACH_BUDGET: Duration = Duration::from_secs(5);

/// What the handoff adds to `ATTACH_BUDGET`: the one deadline over
/// finding, starting and attaching to a monitor is their sum.
pub(crate) const SPAWN_SETTLE: Duration = Duration::from_millis(600);

/// How many steps `SPAWN_SETTLE` is cut into: a client waiting on a
/// permit, a lease or a monitor it started looks again after each.
pub(crate) const SHARE_ATTEMPTS: u32 = 4;

/// A client's end of the connection.
///
/// Holds the BUFFERED reader for the whole life of the subscription.
/// An earlier version read the welcome through a throwaway `BufReader`
/// and returned the bare socket: `read_line` fills its buffer in
/// chunks, so a monitor that wrote the welcome and the first note in
/// one write had that note consumed into the buffer and dropped with
/// it. Bytes already read cannot be put back, so the reader has to
/// outlive the handshake.
/// For the same reason it holds the PART of a frame that has arrived.
/// A caller may give one read a short deadline — `forward_beats` does,
/// so it can notice it has been asked to stop — and a frame split
/// across two of those deadlines would otherwise lose its prefix: the
/// bytes are consumed from the socket and cannot be put back, so the
/// next read starts mid-frame and the decode fails for good (codex on
/// 697807a). A deadline must cost the caller time and never data.
#[derive(Debug)]
pub(crate) struct Conn {
    io: std::io::BufReader<std::os::unix::net::UnixStream>,
    partial: Vec<u8>,
}

impl Conn {
    pub(crate) fn cancellation_socket(&self) -> std::io::Result<std::os::unix::net::UnixStream> {
        self.io.get_ref().try_clone()
    }

    fn over_reader(io: std::io::BufReader<std::os::unix::net::UnixStream>) -> Self {
        Self {
            io,
            partial: Vec::new(),
        }
    }

    /// A connection over a socket a test already has, so the shared
    /// wait loop can be driven without a monitor process.
    #[cfg(test)]
    pub(crate) fn over(stream: std::os::unix::net::UnixStream) -> Self {
        Self::over_reader(std::io::BufReader::new(stream))
    }

    /// Read one `Note`, giving up at `deadline` — but keeping whatever
    /// of it had arrived, for the next call to finish.
    pub(crate) fn note(&mut self, deadline: std::time::Instant) -> std::io::Result<Note> {
        let line = read_frame(&mut self.io, &mut self.partial, deadline)?;
        serde_json::from_str(line.trim_end())
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Read the next `Note`, however long it is in coming. The socket
    /// closing, from either end, is what ends the wait.
    pub(crate) fn next_note(&mut self) -> std::io::Result<Note> {
        loop {
            // A bound only because `poll` takes one; it is not a look.
            let far = std::time::Instant::now() + Duration::from_secs(86_400);
            match self.note(far) {
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                read => return read,
            }
        }
    }
}

/// How a client got on, or did not.
///
/// There is no error variant on purpose: what having no monitor means is
/// each caller's to say, and going quiet is not among the answers. A
/// waiter that serves an agent does not take this at all. It keeps the
/// typed [`Attempt`] and follows ([`Attempt::served`]).
#[derive(Debug)]
pub(crate) enum Attached {
    /// Admitted, with the framing intact.
    Shared(Conn),
    /// Not admitted. The string says why, for the log.
    Alone(String),
}

/// How long is left, or `None` when the deadline has passed.
fn remaining(deadline: std::time::Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    (!left.is_zero()).then_some(left)
}

pub(super) fn expired() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "the monitor did not answer within the attach budget",
    )
}

/// Connect to a unix socket without blocking past `deadline`.
///
/// `UnixStream::connect` blocks and cannot be bounded afterwards: a
/// listener that has stopped accepting and filled its backlog makes the
/// connect itself wait — Linux waits in `unix_wait_for_peer` — so a
/// suspended monitor would strand the client before any socket timeout
/// existed to help (codex on 1f9c16f). No helper thread, which would
/// only move the strand somewhere it cannot be observed.
///
/// The fd stays non-blocking for its whole life, and every read and
/// write is bounded by [`wait_ready`]. Socket timeouts were the obvious
/// alternative and are unusable here: Darwin's `sosetopt` refuses EVERY
/// option with `EINVAL` once the peer has closed both directions, which
/// is exactly when a note the monitor already delivered is sitting in
/// the receive buffer waiting to be read.
pub(crate) fn connect_within(
    path: &std::path::Path,
    deadline: std::time::Instant,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    use std::os::unix::ffi::OsStrExt;

    let sun_path = std::mem::size_of::<libc::sockaddr_un>()
        - std::mem::offset_of!(libc::sockaddr_un, sun_path);
    if path.as_os_str().as_bytes().len() >= sun_path {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    let addr = SocketAddrUnix::new(path)?;

    // Close-on-exec atomically where the platform offers it. A raw
    // socket has none of the setup std's own constructor does, and
    // without this a command source clank spawns INHERITS the monitor
    // connection — keeping it open after the client exits, so the
    // monitor goes on believing someone is listening (codex on
    // f3d6ea9).
    #[cfg(target_os = "linux")]
    let flags = SocketFlags::CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = SocketFlags::empty();
    let owned = rustix::net::socket_with(AddressFamily::UNIX, SocketType::STREAM, flags, None)?;

    #[cfg(not(target_os = "linux"))]
    set_cloexec(&owned)?;
    // On Apple targets std sets this when it builds a socket, and
    // `main` resets SIGPIPE to SIG_DFL so ordinary pipes behave. Both
    // together mean a monitor that closes during a write would KILL the
    // client instead of returning EPIPE, and a client must outlive its
    // monitor to follow the next one.
    #[cfg(target_vendor = "apple")]
    set_nosigpipe(&owned)?;

    set_nonblocking(&owned, true)?;
    if let Err(err) = rustix::net::connect(&owned, &addr) {
        use rustix::io::Errno;
        if !matches!(err, Errno::AGAIN | Errno::INTR | Errno::INPROGRESS) {
            return Err(err.into());
        }
        wait_ready(owned.as_fd(), PollFlags::OUT, deadline)?;
        if let Some(code) = socket_error(&owned)? {
            return Err(std::io::Error::from_raw_os_error(code));
        }
    }
    Ok(std::os::unix::net::UnixStream::from(owned))
}

/// Mark an fd close-on-exec, for platforms with no `SOCK_CLOEXEC`.
#[cfg(not(target_os = "linux"))]
fn set_cloexec(fd: impl AsFd) -> std::io::Result<()> {
    let flags = rustix::io::fcntl_getfd(&fd)?;
    rustix::io::fcntl_setfd(&fd, flags | rustix::io::FdFlags::CLOEXEC)?;
    Ok(())
}

/// Ask the kernel for `EPIPE` instead of a signal on a dead peer.
#[cfg(target_vendor = "apple")]
pub(super) fn set_nosigpipe(fd: impl AsFd) -> std::io::Result<()> {
    rustix::net::sockopt::set_socket_nosigpipe(fd, true)?;
    Ok(())
}

pub(super) fn set_nonblocking(fd: impl AsFd, on: bool) -> std::io::Result<()> {
    use rustix::fs::OFlags;
    let flags = rustix::fs::fcntl_getfl(&fd)?;
    let next = if on {
        flags | OFlags::NONBLOCK
    } else {
        flags - OFlags::NONBLOCK
    };
    rustix::fs::fcntl_setfl(&fd, next)?;
    Ok(())
}

/// Wait for `fd` to be ready for `events`, never past `deadline`.
///
/// The one place the deadline is enforced, for the connect and for
/// every frame after it. `POLLHUP` is reported whether or not it was
/// asked for, so a peer that closes wakes this immediately and the
/// read that follows reports the buffered bytes or the EOF.
fn wait_ready(
    fd: BorrowedFd<'_>,
    events: PollFlags,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    loop {
        let Some(left) = remaining(deadline) else {
            return Err(expired());
        };
        let mut pfd = [rustix::event::PollFd::from_borrowed_fd(fd, events)];
        let left = left.min(Duration::from_millis(i32::MAX as u64));
        let timeout = rustix::event::Timespec {
            tv_sec: left.as_secs() as i64,
            tv_nsec: left.subsec_nanos().into(),
        };
        match rustix::event::poll(&mut pfd, Some(&timeout)) {
            Ok(0) => return Err(expired()),
            Ok(_) => return Ok(()),
            // A signal restarts the wait against the SAME deadline,
            // so an interrupted poll cannot extend the budget.
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// `SO_ERROR`, which is how a non-blocking connect reports its result.
///
/// Reached only after `EINPROGRESS`, which for AF_UNIX happens only
/// when the listener's backlog is full — every ordinary failure
/// (`ENOENT`, `ECONNREFUSED`) comes back from `connect` synchronously.
/// So the `Some` arm here is NOT exercised by the tests: provoking a
/// full backlog is the platform-specific condition the connect seam
/// exists to stand in for. The clean path is covered; the error arm is
/// defensive, and said so rather than claimed.
fn socket_error(fd: impl AsFd) -> std::io::Result<Option<i32>> {
    Ok(rustix::net::sockopt::socket_error(fd)?
        .err()
        .map(rustix::io::Errno::raw_os_error))
}

/// Write one frame, bounded by the SAME deadline as everything else.
///
/// `write_all` is no use on a non-blocking socket, so the partial write
/// is looped here and the waiting is left to [`wait_ready`].
///
/// The deadline is re-read at the top of every iteration, not only
/// where the socket makes us wait: a peer that keeps accepting a
/// little at a time never blocks, and a budget only enforced in
/// [`wait_ready`] would never bind on it (codex on 90985a6).
pub(crate) fn write_frame(
    stream: &std::os::unix::net::UnixStream,
    frame: &[u8],
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut rest = frame;
    while !rest.is_empty() {
        if remaining(deadline).is_none() {
            return Err(expired());
        }
        match (&mut &*stream).write(rest) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "the monitor stopped reading",
                ));
            }
            Ok(n) => rest = &rest[n..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                wait_ready(stream.as_fd(), PollFlags::OUT, deadline)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Read one newline-delimited frame, bounded by `deadline`.
///
/// A byte at a time out of the `BufReader`, so bytes the socket read
/// ahead of this frame stay buffered for the next one. Only an empty
/// buffer reaches the socket, so only an empty buffer can ask
/// [`wait_ready`] to wait — a frame already in hand is never waited on.
///
/// `line` carries the frame ACROSS calls. Giving up at the deadline
/// returns an error but leaves what had arrived in there, so a caller
/// with a short deadline resumes rather than restarting: the bytes are
/// already out of the socket and cannot be read again.
///
/// The deadline is re-read at the top of every iteration, not only
/// where the socket makes us wait: a peer streaming an unterminated
/// frame keeps every read ready, and a budget only enforced in
/// [`wait_ready`] would let it hold this call — and grow `line` —
/// without limit (codex on 90985a6).
pub(super) fn read_frame(
    reader: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
    line: &mut Vec<u8>,
    deadline: std::time::Instant,
) -> std::io::Result<String> {
    use std::io::Read;

    loop {
        if remaining(deadline).is_none() {
            return Err(expired());
        }
        let mut byte = [0u8; 1];
        match reader.read(&mut byte) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the monitor closed mid-frame",
                ));
            }
            Ok(_) if byte[0] == b'\n' => {
                let whole = String::from_utf8_lossy(line).to_string();
                line.clear();
                return Ok(whole);
            }
            Ok(_) => line.push(byte[0]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                wait_ready(reader.get_ref().as_fd(), PollFlags::IN, deadline)?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// What ONE attach attempt established, for a caller that has to
/// decide whether starting a monitor would help.
///
/// `Alone(String)` could not carry that. Every failure looked alike,
/// so the client forked a monitor at each retry — four doomed children
/// per park against a healthy newer owner, and four that had all lost
/// and exited before a legitimate retirement (`RETIRE_BUDGET` plus
/// settlement) could even finish, leaving nobody to start the
/// replacement after the lease freed (codex on 9413783).
#[derive(Debug)]
pub(crate) enum Seen {
    /// Compatible, and attached.
    Shared(Conn),
    /// A monitor that should KEEP the repo: it refused this client on
    /// its merits. Starting one would add a monitor that cannot own
    /// anything.
    Theirs(String),
    /// A newer monitor, which keeps the repo for the same reason.
    Newer(String),
    /// Nothing usable is published. Starting one is the answer.
    Vacant(String),
    /// The monitor at the published address hung up without answering:
    /// a process going down, or a retiring one dropping arrivals. Linux
    /// releases a dying process's files highest fd first, so its client
    /// connections close before its listener does, and a client that
    /// reconnects in that window is reset where macOS would refuse it.
    /// Repaired like a vacancy: the lease says when it has gone.
    HungUp(String),
    /// An older peer answered, and has now been greeted by a newer
    /// client. Retirement is UNCONFIRMED: proto 3 and 4 cannot
    /// self-exit, and the lease is what settles it. `obsolete` is
    /// whether its answer was, exactly, the one it gives when told it is
    /// obsolete, rather than one of its other refusals.
    OlderPeer { why: String, obsolete: bool },
    /// Nothing was established. Not evidence of anything, and never a
    /// reason to fork.
    Unproven(String),
}

impl From<Seen> for Attached {
    fn from(seen: Seen) -> Self {
        match seen {
            Seen::Shared(conn) => Attached::Shared(conn),
            Seen::Theirs(why)
            | Seen::Newer(why)
            | Seen::Vacant(why)
            | Seen::HungUp(why)
            | Seen::OlderPeer { why, .. }
            | Seen::Unproven(why) => Attached::Alone(why),
        }
    }
}

/// What one attach-or-spawn attempt ended as, kept typed for a caller
/// that schedules on it.
///
/// [`Attached::Alone`] says why in a sentence, which is fine for a log
/// and unusable as a control protocol: a keeper that read "newer" out
/// of it would be steering on diagnostic text.
#[derive(Debug)]
pub(crate) enum Attempt {
    /// Compatible, and attached.
    Shared(Conn),
    /// A newer monitor keeps the repo.
    Newer(String),
    /// A monitor of our version refused this client on its merits.
    Theirs(String),
    /// Nothing was established, including whether anybody is repairing.
    Unproven(String),
    /// Another client holds the repair permit.
    Busy(String),
    /// The repo was free to start a monitor in, and starting it failed.
    SpawnFailed(String),
    /// Nothing serves this client and nothing was started, because the
    /// repo's lease is still held: a monitor that no longer answers is on
    /// its way out, and its successor can start only once it has gone.
    LeaseHeld(String),
    /// Nothing serves the repo and nothing was started: the handoff ran
    /// out of time first.
    NotStarted(String),
    /// A monitor was started and had not admitted this client by the
    /// deadline.
    Unattached(String),
}

impl Attempt {
    /// Why it did not attach, for the log; `None` when it did.
    pub(crate) fn why(&self) -> Option<&str> {
        match self {
            Attempt::Shared(_) => None,
            Attempt::Newer(why)
            | Attempt::Theirs(why)
            | Attempt::Unproven(why)
            | Attempt::Busy(why)
            | Attempt::SpawnFailed(why)
            | Attempt::LeaseHeld(why)
            | Attempt::NotStarted(why)
            | Attempt::Unattached(why) => Some(why),
        }
    }
}

impl From<Attempt> for Attached {
    fn from(attempt: Attempt) -> Self {
        match attempt {
            Attempt::Shared(conn) => Attached::Shared(conn),
            other => Attached::Alone(other.why().unwrap_or_default().to_string()),
        }
    }
}

/// How long a waiter goes on for a monitor known to be on its way out
/// before it tells its agent: everything that monitor and its successor
/// are allowed. A stale monitor may hear out the presentations it handed
/// to hooks, then drain its producers and retire, then settle; and the
/// monitor that follows has to start and admit the waiter.
pub(crate) const RETIREMENT_BUDGET: Duration = QUIESCE_BUDGET
    .saturating_add(LEASE_RETIREMENT)
    .saturating_add(SETTLE_BUDGET)
    .saturating_add(SPAWN_SETTLE)
    .saturating_add(ATTACH_BUDGET);

/// Whether the monitor published for `repo` runs a build other than this
/// process's. To a process of the installed build that is a monitor on
/// its way out: it retires by itself once it has settled what it handed
/// out, and answers nothing new meanwhile.
pub(crate) fn outgoing(repo: &std::path::Path) -> bool {
    // A descriptor outlives a monitor that died: one whose process is
    // known to be gone is nobody going, and what follows is an outage.
    let there = |pid: i32| crate::signal::probe_process(pid) != crate::signal::Probe::Gone;
    match published(repo) {
        Published::Ours(descriptor) => {
            there(descriptor.pid)
                && descriptor
                    .build
                    .is_some_and(|build| build.running != crate::build_stamp::running().to_string())
        }
        Published::Other { pid, .. } => pid.is_none_or(there),
        _ => false,
    }
}

/// How long a waiter goes without a monitor before it tells its agent it
/// has none.
///
/// One attempt is bounded by `SPAWN_SETTLE + ATTACH_BUDGET`, and a monitor
/// retiring for an upgrade may hold its lease for `LEASE_RETIREMENT`, so a
/// minute covers a retirement and the attempts its events cause. Longer
/// would leave an agent parked behind a monitor that is not coming.
pub(crate) const OUTAGE_BUDGET: Duration = Duration::from_secs(60);

/// The attempt a classification that ends it is.
///
/// `Err`, with why, for the two that leave the repo to be repaired:
/// nothing usable, or an older peer whose lease decides.
pub(super) fn settled(seen: Seen) -> Result<Attempt, String> {
    match seen {
        Seen::Shared(conn) => Ok(Attempt::Shared(conn)),
        Seen::Newer(why) => Ok(Attempt::Newer(why)),
        Seen::Theirs(why) => Ok(Attempt::Theirs(why)),
        Seen::Unproven(why) => Ok(Attempt::Unproven(why)),
        Seen::Vacant(why) | Seen::HungUp(why) | Seen::OlderPeer { why, .. } => Err(why),
    }
}

/// Attach to this repo's monitor, or report why we are on our own.
///
/// Test-only: production always connects-or-SPAWNS, so attaching
/// without starting one is an affordance for tests that supply their
/// own monitor and must never reach [`spawn_monitor`].
#[cfg(test)]
pub(crate) fn attach(repo: &std::path::Path, who: Who) -> Attached {
    attach_within(repo, who, ATTACH_BUDGET)
}

/// [`attach`] with the budget supplied, so the strand case is testable
/// without waiting the production five seconds.
#[cfg(test)]
pub(crate) fn attach_within(repo: &std::path::Path, who: Who, budget: Duration) -> Attached {
    attach_by(repo, who, budget, connect_within)
}

/// [`attach_within`] with the connect supplied, so a connection that
/// cannot complete inside the budget is reachable deterministically —
/// a full backlog is platform-specific and cannot be provoked
/// portably.
pub(crate) fn attach_by(
    repo: &std::path::Path,
    who: Who,
    budget: Duration,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>,
) -> Attached {
    look_at(repo, who, budget, connect).into()
}

/// [`attach_by`], keeping what it ESTABLISHED rather than a sentence
/// about it.
pub(crate) fn look_at(
    repo: &std::path::Path,
    who: Who,
    budget: Duration,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>,
) -> Seen {
    look_at_in(
        &runtime_dir(rustix::process::getuid().as_raw()),
        repo,
        who,
        budget,
        connect,
    )
}

/// [`look_at`] with the namespace supplied, so a test can plant a
/// descriptor in one we must not read and watch `connect` stay
/// untouched.
pub(crate) fn look_at_in(
    namespace: &std::path::Path,
    repo: &std::path::Path,
    who: Who,
    budget: Duration,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>,
) -> Seen {
    let deadline = std::time::Instant::now() + budget;
    // The descriptor is a HINT about where to knock, not a gate.
    //
    // It used to decide the version too, and that is what stopped this
    // whole mechanism working: a newer client read an older
    // descriptor, returned without connecting, and the holder never
    // heard the greeting that would have told it to go (codex on
    // c4b5899). The repo still gates — a descriptor outlives the repo
    // it describes — and the VERSION is settled by the handshake,
    // where both ends can act on it.
    let socket = match published_in(namespace, repo, rustix::process::getuid().as_raw()) {
        Published::Nothing => {
            return Seen::Vacant(format!("no monitor published for {}", repo.display()));
        }
        // An address from a namespace we do not own is not an address.
        // Unproven rather than Vacant: starting a monitor would need
        // the lease, which lives in the same namespace and is refused
        // for the same reason, so the honest answer is to go alone.
        Published::Untrusted(why) => {
            return Seen::Unproven(format!(
                "this uid's runtime namespace is not ours to read: {why}"
            ));
        }
        Published::Ours(d) if !serves(repo, &d.repo) => {
            // A descriptor for a repo this is not. Nothing is serving
            // THIS one, so starting is right.
            return Seen::Vacant(format!(
                "the monitor at {} serves {}, not {}",
                d.socket,
                d.repo,
                repo.display()
            ));
        }
        Published::Ours(d) => d.socket,
        Published::Other {
            socket: Some(s), ..
        } => s,
        Published::Other {
            proto,
            socket: None,
            ..
        } => {
            // Something is there and cannot be addressed. Not an
            // absence, so not a reason to start a second one.
            return Seen::Unproven(format!(
                "a proto {proto} monitor is published for {} with no socket we can read",
                repo.display()
            ));
        }
    };
    greet_at(&socket, repo, who, deadline, connect)
}

/// Greet the monitor listening at `socket` on behalf of `who` in `repo`,
/// and classify its answer.
pub(crate) fn greet_at(
    socket: &str,
    repo: &std::path::Path,
    who: Who,
    deadline: std::time::Instant,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>,
) -> Seen {
    let stream = match connect(std::path::Path::new(socket), deadline) {
        Ok(s) => s,
        // A descriptor outlives its monitor: a refused connection is
        // the ordinary way a dead one is noticed, and a connect that
        // cannot finish in the budget is the same answer.
        // A descriptor outlives its monitor, so a refused connection
        // is the ordinary way a dead one is noticed — and then nothing
        // holds the repo.
        Err(e) => return Seen::Vacant(format!("cannot reach {}: {e}", socket)),
    };

    let hello = Hello {
        proto: PROTO,
        build: Some(crate::build_stamp::running().to_string()),
        repo: repo.to_string_lossy().to_string(),
        who,
    };
    let line = match serde_json::to_string(&hello) {
        Ok(line) => line,
        Err(e) => return Seen::Unproven(format!("cannot compose a hello: {e}")),
    };

    let mut conn = Conn::over_reader(std::io::BufReader::new(stream));
    if let Err(e) = write_frame(conn.io.get_ref(), format!("{line}\n").as_bytes(), deadline) {
        let why = format!("cannot greet {}: {e}", socket);
        return match hung_up(&e) {
            true => Seen::HungUp(why),
            false => Seen::Unproven(why),
        };
    }

    let answer = match read_frame(&mut conn.io, &mut conn.partial, deadline) {
        Ok(answer) => answer,
        Err(e) => {
            let why = format!("the handshake with {} failed: {e}", socket);
            return match hung_up(&e) {
                true => Seen::HungUp(why),
                false => Seen::Unproven(why),
            };
        }
    };
    // Version before body here too: the descriptor said what the
    // monitor WAS, and the process answering now is what matters.
    let Ok(envelope) = serde_json::from_str::<Envelope>(answer.trim_end()) else {
        return Seen::Unproven(format!("the monitor's answer did not parse: {answer:?}"));
    };
    if !admits(PROTO, envelope.proto) {
        let theirs = envelope.proto;
        let why = format!("the monitor answered on proto {theirs}, we speak {PROTO}");
        if theirs > PROTO {
            // Newer, and it keeps the repo. We are the stale client
            // and go it alone; starting one would only add a monitor
            // that cannot own anything.
            return Seen::Newer(why);
        }
        // Older. The REPLY still has to be one we understand before it
        // means anything: a bare `{"proto":4}`, an unknown body, or an
        // older peer refusing us for the repo or the roster is not
        // evidence about retirement, and the first version read all of
        // them as "standing down" (codex on 88b6234).
        //
        // And even a well-formed one does not establish it. Proto 3
        // and 4 cannot self-exit at all, and the wire does not
        // distinguish a refusal from retirement progress. What is
        // established is an older peer that has now been greeted; the
        // LEASE is what says whether it left.
        return match serde_json::from_str::<Welcome>(answer.trim_end()) {
            Ok(Welcome::Decline {
                proto,
                why: answered,
            }) => Seen::OlderPeer {
                why,
                obsolete: answered == obsolete_why(proto, PROTO),
            },
            _ => Seen::Unproven(format!(
                "a proto {theirs} monitor answered something this build cannot read"
            )),
        };
    }
    match serde_json::from_str::<Welcome>(answer.trim_end()) {
        Ok(Welcome::Ok { .. }) => Seen::Shared(conn),
        // Our own version, refusing us on our merits — the roster, the
        // repo. A second monitor would refuse us the same way.
        Ok(Welcome::Decline { proto, why }) => {
            Seen::Theirs(format!("the monitor declined (proto {proto}): {why}"))
        }
        Err(e) => Seen::Unproven(format!("the monitor's answer did not parse: {e}")),
    }
}

/// Whether a handshake error is the peer closing on us, as opposed to a
/// deadline or a fault on our side.
///
/// `NotConnected` is a close too: the socket was connected, and Darwin
/// answers a write that races the peer's close with `ENOTCONN` rather
/// than `EPIPE`.
pub(super) fn hung_up(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::NotConnected
    )
}

#[cfg(test)]
#[path = "connect_tests.rs"]
mod tests;
