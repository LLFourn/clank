//! `clank monitor`: one watcher per repo, and the registry waiters join.
//!
//! A repo was being watched once per WAITER. Every parked `clank wait`
//! attached its own `RepoStateWatcher` and refolded the whole repo on
//! each change, as did every `clank tui` — measured at ~170 watchers
//! for 71 agents, with one repo folding 8 times per commit. The fold is
//! per repo; only delivery is per session, because an adapter's wake is
//! its own process's exit or stdout.
//!
//! This module is the wire between them. It carries no delivery: a
//! client receives the same `WaitItem`s it would have derived itself
//! and then speaks its own adapter's protocol, unchanged.

use std::time::Duration;

use clank_core::ids::AgentLabel;
use clank_core::wait::WaitItem;

/// The wire version. Bumped for any change a previous build would
/// misread; a mismatch is not an error but a decline, after which the
/// client does its own local wait.
///
/// Skew is routine rather than exceptional here: monitors outlive
/// `cargo install`, so a fresh client meeting an old monitor is the
/// expected case and must degrade rather than fail.
pub(crate) const PROTO: u32 = 3;

/// The version, readable WITHOUT decoding the rest.
///
/// Negotiation must inspect this before the body, because a body
/// decoded under the wrong version fails in a way that cannot be told
/// from corruption: proto 1's `Hello` had no holder, so reading it as
/// proto 2 reports a missing field rather than a skew (codex on
/// 4915f33). Serde ignores unknown fields, so this reads any version's
/// frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct Envelope {
    pub(crate) proto: u32,
}

/// The holder a client speaks for: BOTH halves, or neither.
///
/// Mirrors `WaitBinding(Option<(Tool, SessionId)>)`, whose doc is the
/// requirement — *"a terminal wait with no session remains a direct,
/// unclaimed observer"*. So an unbound wait must be representable, and
/// a half-pair must not be: a tool without a session would invent an
/// identity for a terminal client, and a session without a tool is the
/// misattribution the previous commit was fixing (codex on 4915f33).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Holder {
    pub(crate) tool: clank_core::vocab::Tool,
    pub(crate) session: String,
}

/// Who is connecting, and therefore what may be sent to them.
///
/// An agent is told what IT is owed, which takes an identity the
/// roster recognises. An observer is told only that the fold moved:
/// `clank status --watch` and the TUI want a WAKE, not a projection,
/// and giving them an identity would have meant inventing a roster
/// entry for a panel. Two kinds of reader, so two kinds of hello —
/// the alternative was an author field that some clients leave empty,
/// which is the shape that let a missing `Option` serve a bound client
/// as an unbound one (codex on 4915f33).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub(crate) enum Who {
    Agent {
        author: String,
        /// `None` for a terminal wait that holds nothing. The pair
        /// travels whole because `WaitBinding` validates it whole and
        /// rechecks it immediately before delivery, and one monitor
        /// serves agents of different tools — deriving either half
        /// from the monitor's own environment would attribute one
        /// agent's binding to another (codex on adb5453).
        ///
        /// The ROLE is deliberately absent: a demotion mid-wait must
        /// drop what the old watch queued, so the monitor routes by
        /// the author's current role and never one captured here.
        holder: Option<Holder>,
        /// Whether this client declared its own `--event` sources.
        /// Those belong to ONE invocation and the monitor never owns
        /// them, so it is told only that they exist — a monitor that
        /// pooled them would deliver one invocation's events to
        /// another.
        has_cli_sources: bool,
    },
    /// A read-only fold subscription. Any number, no identity, and
    /// never any items.
    Observer,
}

/// What a client says on connecting.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Hello {
    pub(crate) proto: u32,
    /// The canonicalized repo this client speaks for, verified by
    /// [`serves`] — the filename is derived from it, and a derivation
    /// is not a guarantee.
    pub(crate) repo: String,
    pub(crate) who: Who,
}

/// What the monitor answers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum Welcome {
    Ok { proto: u32, fold: Fold },
    Decline { proto: u32, why: String },
}

/// Evidence that the FOLD is progressing, written by the fold loop.
///
/// Not by the accept loop. A stamp from the socket task proves only
/// that the process breathes, which is exactly what a wedged monitor
/// does — and a wedged monitor answering pings while folding nothing is
/// the failure this has to catch (codex on ca7a4c2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Fold {
    /// Advances on every completed loop tick, whether or not the fold
    /// changed. A quiet repo produces no new state, so the TICK is the
    /// evidence of life and the state is not.
    pub(crate) tick: u64,
    pub(crate) at_ms: u64,
}

/// What the monitor pushes to a client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum Note {
    /// What this client's author is owed. Empty is never sent; a client
    /// blocks until it has something, as a park does today.
    Items { items: Vec<WaitItem>, fold: Fold },
    /// Proof of life. Carries the fold so silence and staleness are the
    /// same signal to a client.
    Beat { fold: Fold },
}

/// Where monitor sockets live, in the order they are tried.
///
/// Copied from Claude Code's own arrangement, which solved this
/// problem first: a short flat directory, never under the repo.
/// `sun_path` is 104 bytes on macOS and clank nests worktrees — the
/// longest repo on the machine this was written for reaches 97 bytes
/// for `<repo>/.clank/monitor.sock`, seven short of unusable, and a
/// longer worktree name is all it takes. Claude Code's regexes name
/// `/tmp/cc-socks`, `/tmp/cc-socks-<uid>` and
/// `/run/user/<uid>/cc-socks`; this is the same shape.
///
/// The shared directory is tried first and the per-uid one is the
/// fallback for when it is not ours, mirroring the documented
/// behaviour: *"refuses to create the socket in a directory it can't
/// accept, for example one that another user owns, and uses a private
/// per-user directory … instead."*
pub(crate) fn socket_dirs(
    uid: u32,
    xdg_runtime: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(run) = xdg_runtime {
        out.push(run.join("clank-socks"));
    }
    out.push(std::path::PathBuf::from("/tmp/clank-socks"));
    out.push(std::path::PathBuf::from(format!("/tmp/clank-socks-{uid}")));
    out
}

/// The longest socket path the kernel will accept.
///
/// `sun_path` in `sockaddr_un` is 104 bytes on macOS and 108 on Linux.
/// The stricter one is used on both: a path that fits it fits
/// everywhere, and a monitor that binds on Linux but not on macOS is a
/// difference nobody notices until it bites someone else.
pub(crate) const SUN_PATH_MAX: usize = 104;

/// The socket path for `pid` in `dir`, or `None` when the kernel would
/// not take it.
///
/// Checked PER CANDIDATE, because a candidate can be arbitrarily long:
/// `$XDG_RUNTIME_DIR` comes from the environment, so the first
/// candidate tried is precisely the one whose length is not known in
/// advance (codex on 06ccd66). A candidate that cannot hold a socket is
/// skipped rather than fatal — the next one is shorter.
pub(crate) fn socket_path(dir: &std::path::Path, pid: i32) -> Option<std::path::PathBuf> {
    let path = dir.join(format!("{pid}.sock"));
    // The kernel copies the bytes and needs room for the terminator.
    (path.to_string_lossy().len() < SUN_PATH_MAX).then_some(path)
}

/// Whether a socket directory may be used.
///
/// `/tmp` is world-writable, so a directory we do not own — or one
/// anyone else can WRITE — would let them plant or replace a socket and
/// intercept or deny a monitor. Write is the whole threat: the socket
/// itself is 0600, so group or other READ on the directory grants
/// nothing, and refusing it would reject an ordinary `/tmp/clank-socks`
/// created under a 022 umask.
///
/// Pure over the stat results, so it is testable without chown.
pub(crate) fn dir_is_acceptable(owner_uid: u32, mode: u32, me: u32) -> bool {
    owner_uid == me && mode & 0o022 == 0
}

/// What a repo's monitor publishes about itself.
///
/// The descriptor is how a client FINDS the socket, which is the part
/// of Claude Code's design that matters most: it writes
/// `~/.claude/sessions/<pid>.json` carrying `messagingSocketPath`, so
/// the socket's own filename carries no meaning and there is no derived
/// name to collide. An earlier draft here hashed the repo path into the
/// filename; a descriptor is strictly better — no collisions, and it
/// can carry the version and the fold a client needs before it even
/// connects.
///
/// It lives in the repo (`.clank/monitor.json`), where PATH_MAX applies
/// rather than `sun_path`, so depth is not a problem for a plain file.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Descriptor {
    pub(crate) proto: u32,
    pub(crate) pid: i32,
    /// The canonicalized repo, so a descriptor left behind by a repo
    /// since moved is recognised rather than trusted.
    pub(crate) repo: String,
    pub(crate) socket: String,
    /// The last fold the monitor completed, so a client can see
    /// staleness without connecting — as `status` in Claude Code's
    /// session descriptor does.
    pub(crate) fold: Fold,
}

/// Whether a monitor serving `theirs` may answer a client from `ours`.
///
/// Checked even with a descriptor, because a descriptor can outlive the
/// repo it describes: a moved or recreated repo would otherwise have
/// its clients folding another repo's state.
pub(crate) fn serves(ours: &std::path::Path, theirs: &str) -> bool {
    ours.to_string_lossy() == theirs
}

/// Whether a monitor at `theirs` can serve a client built for `ours`.
pub(crate) fn admits(ours: u32, theirs: u32) -> bool {
    ours == theirs
}

/// Whether a client has waited too long for any `Note`.
///
/// The only liveness question a client needs to ask, because the beat
/// comes from the fold loop: a loop that is stuck — or stuck INSIDE a
/// fold — emits nothing, so a wedged monitor and a dead one both show
/// up here as silence. Comparing tick numbers would add nothing a
/// client can act on.
///
/// Takes the ELAPSED time, not two instants, so the caller must have
/// measured it monotonically. The first shape took two wall-clock
/// milliseconds and invited exactly the bug that shape invites: a
/// clock rollback postpones the timeout until the clock catches up,
/// and saturating arithmetic hides it rather than fixing it (codex on
/// adb5453). `Fold::at_ms` stays for diagnostics and is never the
/// basis of this decision.
pub(crate) fn beat_overdue(since_last: Duration, grace: Duration) -> bool {
    since_last > grace
}

/// Whether a client should stop trying to share and just do the work
/// itself.
///
/// The watchdog covers a stalled fold in a process that still runs its
/// threads. It does NOT cover a whole-process suspension or a watchdog
/// that itself died: the flock stays held, so spawning again cannot
/// acquire it, and no amount of retrying will (codex on adb5453). This
/// is the bounded fallback for that — the agent degrades to today's
/// self-contained wait rather than going silent, which is the one
/// outcome that must never follow from this plan.
pub(crate) fn give_up_sharing(attempts: u32, limit: u32) -> bool {
    attempts >= limit
}

/// What one look at the fold decides.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Watch {
    /// Carry on, remembering what was seen and how long it has been
    /// still.
    Keep { tick: u64, missed: u32 },
    /// Evict ourselves, so the lease frees.
    StandDown,
}

/// Fold the watchdog's state forward by one look.
///
/// A state machine rather than a predicate because the question is
/// about a SEQUENCE — how many looks in a row found the fold where it
/// was — and a predicate takes that count as an argument, which means
/// every test supplies by hand the very thing that can be wrong.
///
/// Why the monitor evicts itself at all: a lease serialises owners and
/// cannot evict a wedged one, and unlinking the lock to force it would
/// leave two processes holding flocks on two inodes — two owners. So
/// the monitor stands down on its own, from a thread that is not the
/// fold loop (the thing that may be stuck), and the lease frees
/// honestly for the next client to take.
pub(crate) fn watchdog_look(seen: u64, now: u64, missed: u32, limit: u32) -> Watch {
    if now != seen {
        return Watch::Keep {
            tick: now,
            missed: 0,
        };
    }
    let missed = missed + 1;
    if missed >= limit {
        return Watch::StandDown;
    }
    Watch::Keep { tick: now, missed }
}

/// Whether a mode keeps a socket to its owner.
fn mode_is_private(mode: u32) -> bool {
    mode & 0o177 == 0
}

/// Vet a directory we are about to put a socket in.
///
/// `md` must come from `symlink_metadata`, so the final component is
/// not followed: an attacker-owned symlink at the candidate pointing
/// into a directory we DO own would otherwise pass, because the uid and
/// mode read back belong to the target and not to the link. They could
/// then replace their own link — theirs to remove, even under a sticky
/// `/tmp` — and redirect every later `remove_file`, `bind`, `chmod` and
/// client `connect` through it (codex on 9228189).
///
/// The indirection is the thing refused. With a real directory we own,
/// `/tmp`'s sticky bit stops anyone else replacing it.
fn vet_dir(dir: &std::path::Path, md: &std::fs::Metadata, me: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if md.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink; a socket directory must be the real thing",
            dir.display()
        ));
    }
    if !md.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    if dir_is_acceptable(md.uid(), md.mode(), me) {
        return Ok(());
    }
    Err(format!(
        "{} is owned by uid {} with mode {:o}",
        dir.display(),
        md.uid(),
        md.mode() & 0o777
    ))
}

/// Create a socket directory, or accept an existing one.
///
/// `/tmp` is world-writable, so a directory we do not own, or one
/// anyone else can write, would let them plant or replace a socket.
///
/// The vet runs AFTER creation as well as before, because
/// `DirBuilder::recursive(true)` succeeds on a directory that already
/// exists: a racer who creates `/tmp/clank-socks` between our stat and
/// our create would otherwise have their directory accepted without
/// its owner or mode ever being looked at (codex on dab07ce). And only
/// `NotFound` permits creating — any other stat error is a reason to
/// stop, not a licence to proceed.
fn ensure_dir(dir: &std::path::Path, me: u32) -> Result<(), String> {
    ensure_dir_with(dir, me, |_| {})
}

/// [`ensure_dir`] with a hook between the stat and the create, so the
/// creation race is reachable deterministically instead of by luck.
fn ensure_dir_with(
    dir: &std::path::Path,
    me: u32,
    between: impl FnOnce(&std::path::Path),
) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;

    // `symlink_metadata`, not `metadata`: the final component must not
    // be followed, or a symlink is vetted by what it points at.
    match std::fs::symlink_metadata(dir) {
        Ok(md) => return vet_dir(dir, &md, me),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot stat {}: {e}", dir.display())),
    }

    between(dir);

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    // Also unfollowed: `recursive(true)` succeeds when the path is
    // already a symlink to a directory, so the creation race can plant
    // an indirection as easily as a loose directory.
    let md = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot stat {} after creating it: {e}", dir.display()))?;
    vet_dir(dir, &md, me)
}

/// Keep the socket to this user, and REPORT when that fails.
///
/// Not best-effort. An earlier version ignored the failure on the
/// grounds that the directory was 0700, which stopped being true once
/// existing 0755 directories were accepted — under a permissive umask
/// the socket itself could then be writable by others and would be
/// published anyway (codex on dab07ce). The achieved mode is read back
/// rather than assumed, because a chmod that reports success on an
/// unusual filesystem still has to produce the mode we need.
fn restrict(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot restrict {}: {e}", path.display()))?;
    verify_private(path)
}

/// Read back what a chmod actually achieved.
///
/// Split out so it is callable in a test: a `set_permissions` that
/// reports success without applying the mode is a filesystem quirk
/// rather than something that can be provoked here, but the CHECK can
/// be driven directly against a file we made permissive on purpose.
fn verify_private(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot stat {} after restricting it: {e}", path.display()))?
        .permissions()
        .mode();
    if mode_is_private(mode) {
        Ok(())
    } else {
        Err(format!(
            "{} stayed at mode {:o} after being restricted",
            path.display(),
            mode & 0o777
        ))
    }
}

/// Bind a listener in the first candidate directory that will take one.
///
/// A socket file left by a dead monitor is unlinked rather than
/// refused: binding over it would fail with `EADDRINUSE` forever, and
/// the caller holds the LEASE, which is what makes removing it safe —
/// no live owner can exist while that is held.
///
/// Returns a std listener so the async loop can adopt it; the setup
/// path stays synchronous because every failure here is a reason to
/// stop, not to await.
pub(crate) fn bind_listener(
    candidates: &[std::path::PathBuf],
    pid: i32,
    me: u32,
    lease: &MonitorLease,
) -> Result<(std::os::unix::net::UnixListener, std::path::PathBuf), String> {
    bind_listener_with(candidates, pid, me, lease, restrict)
}

/// [`bind_listener`] with the restriction step supplied, so its failure
/// path is reachable without an exotic filesystem.
fn bind_listener_with(
    candidates: &[std::path::PathBuf],
    pid: i32,
    me: u32,
    _lease: &MonitorLease,
    restrict: impl Fn(&std::path::Path) -> Result<(), String>,
) -> Result<(std::os::unix::net::UnixListener, std::path::PathBuf), String> {
    let mut why = Vec::new();
    for dir in candidates {
        let Some(path) = socket_path(dir, pid) else {
            why.push(format!("{} is too long for a socket path", dir.display()));
            continue;
        };
        if let Err(e) = ensure_dir(dir, me) {
            why.push(e);
            continue;
        }
        // Safe only because the lease is held: nothing live owns this.
        let _ = std::fs::remove_file(&path);
        sweep_dead_sockets(dir, pid, pid_is_live);
        let listener = match std::os::unix::net::UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(e) => {
                why.push(format!("cannot bind {}: {e}", path.display()));
                continue;
            }
        };
        // An unrestricted socket is worse than none: it is reachable by
        // any local user. Abandon the candidate rather than publish it.
        if let Err(e) = restrict(&path) {
            drop(listener);
            let _ = std::fs::remove_file(&path);
            why.push(e);
            continue;
        }
        return Ok((listener, path));
    }
    Err(format!("no usable socket directory: {}", why.join("; ")))
}

/// Remove sockets left behind by monitors that are gone.
///
/// A socket's name carries its monitor's pid, and a monitor killed with
/// SIGKILL cannot unlink its own. The directory is shared by every
/// repo's monitor, so nothing else is in a position to tidy up, and
/// without this it grows by one dead file per killed monitor forever.
/// The next monitor to bind is the one that can: `pid` is its own and
/// is never swept.
///
/// Conservative on purpose. An unparseable name is left alone — the
/// directory may hold something this version did not create — and a
/// pid that has been REUSED by an unrelated process reads as live, so
/// its socket survives. Leaving litter is the failure to prefer over
/// unlinking a socket somebody is serving on.
fn sweep_dead_sockets(dir: &std::path::Path, mine: i32, live: impl Fn(i32) -> bool) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut swept = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(stem) = std::path::Path::new(&name)
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".sock"))
        else {
            continue;
        };
        let Ok(pid) = stem.parse::<i32>() else {
            continue;
        };
        if pid == mine || live(pid) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            swept += 1;
        }
    }
    swept
}

/// Whether a pid still names a running process.
///
/// `EPERM` counts as live: it means the process exists but belongs to
/// somebody else, which is every reason to leave its socket alone.
fn pid_is_live(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 checks for the process without delivering one.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Read a client's opening frame, VERSION FIRST.
///
/// The order is the point. A body decoded under the wrong version can
/// succeed with wrong semantics — serde reads a missing `Option` as
/// `None`, which would serve a bound client as an unbound one — so the
/// envelope decides before the body is looked at, and every refusal is
/// a `Decline` the client can act on rather than a parse error it
/// cannot distinguish from corruption.
///
/// Pure over the line so the whole negotiation is testable without a
/// socket.
pub(crate) fn accept_hello(line: &str, repo: &std::path::Path) -> Result<Hello, Welcome> {
    let decline = |why: String| Err(Welcome::Decline { proto: PROTO, why });
    let Ok(envelope) = serde_json::from_str::<Envelope>(line) else {
        return decline("the opening frame is not a monitor hello".into());
    };
    if !admits(PROTO, envelope.proto) {
        return decline(format!(
            "this monitor speaks proto {PROTO}, the client speaks {}",
            envelope.proto
        ));
    }
    let Ok(hello) = serde_json::from_str::<Hello>(line) else {
        return decline(format!("a proto {PROTO} hello did not parse"));
    };
    if !serves(repo, &hello.repo) {
        return decline(format!(
            "this monitor serves {}, the client asked for {}",
            repo.display(),
            hello.repo
        ));
    }
    Ok(hello)
}

/// How long attaching may take, start to finish.
///
/// An OVERALL budget, not an idle timeout: a peer that dribbles a byte
/// at a time would extend an inactivity timeout for as long as it
/// liked, and a monitor that accepts and then says nothing would
/// otherwise strand the client forever — `give_up_sharing` cannot help
/// if `attach` never returns (codex on af1efd2).
pub(crate) const ATTACH_BUDGET: Duration = Duration::from_secs(5);

/// How long a client waits for any note before it stops sharing.
///
/// Comfortably more than the monitor's fold tick, so an ordinary quiet
/// repo never trips it: silence past this means the monitor is dead or
/// wedged, and both are the same answer to a client.
pub(crate) const BEAT_GRACE: Duration = Duration::from_secs(20);

/// How long a client gives a monitor it just started to come up.
///
/// Short on purpose. It is spent BEFORE the park, and a monitor that
/// is not listening by then costs nothing: this wait watches locally
/// and the process — which outlives every client — is there for the
/// next one.
const SPAWN_SETTLE: Duration = Duration::from_millis(600);

/// How many times a client tries to attach before watching for itself.
const SHARE_ATTEMPTS: u32 = 4;

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
}

/// How a client got on, or did not.
///
/// There is no error variant on purpose. Every way this can fail is a
/// reason to do the work alone, which is what a park does today —
/// going quiet is the one outcome this plan may not produce, so it is
/// not representable here.
#[derive(Debug)]
pub(crate) enum Attached {
    /// Admitted, with the framing intact.
    Shared(Conn),
    /// Watch the repo yourself. The string says why, for the log.
    Alone(String),
}

/// How long is left, or `None` when the deadline has passed.
fn remaining(deadline: std::time::Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    (!left.is_zero()).then_some(left)
}

fn expired() -> std::io::Error {
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
fn connect_within(
    path: &std::path::Path,
    deadline: std::time::Instant,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;

    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    if bytes.len() >= addr.sun_path.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket path too long",
        ));
    }
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as _;
    }

    // Close-on-exec atomically where the platform offers it. A raw
    // `libc::socket` has none of the setup std's own constructor does,
    // and without this a command source clank spawns INHERITS the
    // monitor connection — keeping it open after the client exits, so
    // the monitor goes on believing someone is listening (codex on
    // f3d6ea9).
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;

    // SAFETY: a fresh AF_UNIX stream socket; the fd is owned below so
    // every return path closes it.
    let raw = unsafe { libc::socket(libc::AF_UNIX, kind, 0) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };

    #[cfg(not(target_os = "linux"))]
    set_cloexec(raw)?;
    // On Apple targets std sets this when it builds a socket, and
    // `main` resets SIGPIPE to SIG_DFL so ordinary pipes behave. Both
    // together mean a monitor that closes during a write would KILL the
    // client instead of returning EPIPE — and the whole point of
    // `Attached` is that a failure degrades to a local wait rather than
    // taking the agent down.
    #[cfg(target_vendor = "apple")]
    set_nosigpipe(raw)?;

    set_nonblocking(raw, true)?;
    // SAFETY: `addr` is a fully initialised sockaddr_un and `raw` is a
    // valid socket fd.
    let rc = unsafe {
        libc::connect(
            raw,
            &addr as *const libc::sockaddr_un as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        let pending = matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) || err.raw_os_error() == Some(libc::EINPROGRESS);
        if !pending {
            return Err(err);
        }
        wait_ready(raw, libc::POLLOUT, deadline)?;
        if let Some(code) = socket_error(raw)? {
            return Err(std::io::Error::from_raw_os_error(code));
        }
    }
    Ok(std::os::unix::net::UnixStream::from(owned))
}

/// Mark an fd close-on-exec, for platforms with no `SOCK_CLOEXEC`.
#[cfg(not(target_os = "linux"))]
fn set_cloexec(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `fd` is a valid fd for the life of the call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Ask the kernel for `EPIPE` instead of a signal on a dead peer.
#[cfg(target_vendor = "apple")]
fn set_nosigpipe(fd: libc::c_int) -> std::io::Result<()> {
    let on: libc::c_int = 1;
    // SAFETY: `on` outlives the call and matches SO_NOSIGPIPE's shape.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            &on as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: libc::c_int, on: bool) -> std::io::Result<()> {
    // SAFETY: `fd` is a valid socket fd for the life of the call.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let next = if on {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: as above.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, next) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Wait for `fd` to be ready for `events`, never past `deadline`.
///
/// The one place the deadline is enforced, for the connect and for
/// every frame after it. `POLLHUP` is reported whether or not it was
/// asked for, so a peer that closes wakes this immediately and the
/// read that follows reports the buffered bytes or the EOF.
fn wait_ready(
    fd: libc::c_int,
    events: libc::c_short,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    loop {
        let Some(left) = remaining(deadline) else {
            return Err(expired());
        };
        let mut pfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
        // SAFETY: one initialised pollfd describing a valid fd.
        let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
        match rc {
            0 => return Err(expired()),
            r if r > 0 => return Ok(()),
            _ => {
                let err = std::io::Error::last_os_error();
                // A signal restarts the wait against the SAME deadline,
                // so an interrupted poll cannot extend the budget.
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(err);
                }
            }
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
fn socket_error(fd: libc::c_int) -> std::io::Result<Option<libc::c_int>> {
    let mut err: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `err`/`len` outlive the call and match SO_ERROR's shape.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            &mut err as *mut libc::c_int as *mut libc::c_void,
            &mut len,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((err != 0).then_some(err))
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
fn write_frame(
    stream: &std::os::unix::net::UnixStream,
    frame: &[u8],
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
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
                wait_ready(stream.as_raw_fd(), libc::POLLOUT, deadline)?;
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
fn read_frame(
    reader: &mut std::io::BufReader<std::os::unix::net::UnixStream>,
    line: &mut Vec<u8>,
    deadline: std::time::Instant,
) -> std::io::Result<String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

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
                wait_ready(reader.get_ref().as_raw_fd(), libc::POLLIN, deadline)?;
            }
            Err(e) => return Err(e),
        }
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
    let deadline = std::time::Instant::now() + budget;
    let Some(d) = read_published(repo) else {
        return Attached::Alone(format!("no monitor published for {}", repo.display()));
    };
    if !usable(&d, repo, PROTO) {
        return Attached::Alone(format!(
            "the monitor at {} serves {} on proto {} — not ours",
            d.socket, d.repo, d.proto
        ));
    }
    let stream = match connect(std::path::Path::new(&d.socket), deadline) {
        Ok(s) => s,
        // A descriptor outlives its monitor: a refused connection is
        // the ordinary way a dead one is noticed, and a connect that
        // cannot finish in the budget is the same answer.
        Err(e) => return Attached::Alone(format!("cannot reach {}: {e}", d.socket)),
    };

    let hello = Hello {
        proto: PROTO,
        repo: repo.to_string_lossy().to_string(),
        who,
    };
    let line = match serde_json::to_string(&hello) {
        Ok(line) => line,
        Err(e) => return Attached::Alone(format!("cannot compose a hello: {e}")),
    };

    let mut conn = Conn::over_reader(std::io::BufReader::new(stream));
    if let Err(e) = write_frame(conn.io.get_ref(), format!("{line}\n").as_bytes(), deadline) {
        return Attached::Alone(format!("cannot greet {}: {e}", d.socket));
    }

    let answer = match read_frame(&mut conn.io, &mut conn.partial, deadline) {
        Ok(answer) => answer,
        Err(e) => return Attached::Alone(format!("the handshake with {} failed: {e}", d.socket)),
    };
    // Version before body here too: the descriptor said what the
    // monitor WAS, and the process answering now is what matters.
    let Ok(envelope) = serde_json::from_str::<Envelope>(answer.trim_end()) else {
        return Attached::Alone(format!("the monitor's answer did not parse: {answer:?}"));
    };
    if !admits(PROTO, envelope.proto) {
        return Attached::Alone(format!(
            "the monitor answered on proto {}, we speak {PROTO}",
            envelope.proto
        ));
    }
    match serde_json::from_str::<Welcome>(answer.trim_end()) {
        Ok(Welcome::Ok { .. }) => Attached::Shared(conn),
        Ok(Welcome::Decline { proto, why }) => {
            Attached::Alone(format!("the monitor declined (proto {proto}): {why}"))
        }
        Err(e) => Attached::Alone(format!("the monitor's answer did not parse: {e}")),
    }
}

/// A monitor this client started, still to be reaped.
///
/// Reaping has to OUTLIVE the attachment. Dropping a `Child` does not
/// reap it and `process_group` does not reparent it, so a monitor that
/// exits while its spawning client is still parked stays a zombie for
/// the rest of that park — and for a repo full of parked waits that is
/// one zombie per wait (codex on fe22d39). The concrete case is two
/// clients starting at once: ours is still coming up, we attach to the
/// winner and return, and ours then loses the lease and exits with
/// nobody left watching for it.
///
/// A trait rather than the `Child` itself because no test here may
/// start a binary, and `std::process::Child` cannot be constructed
/// without one — so this is the only shape in which a test can own a
/// started monitor at all.
pub(crate) trait Started: Send + 'static {
    /// Wait for it, WITHOUT killing it: the monitor must outlive the
    /// client that started it. That is the whole point of spawning
    /// rather than binding.
    fn reap(self: Box<Self>);
}

struct Spawned(std::process::Child);

impl Started for Spawned {
    fn reap(mut self: Box<Self>) {
        let _ = self.0.wait();
    }
}

/// Hand a started monitor to a waiter that outlives this call.
///
/// A thread, because the wait must survive attachment success,
/// fallback and cancellation alike — anything owned by the retry loop
/// is released exactly when the monitor is least likely to have exited
/// yet. It blocks in `waitpid` for as long as the monitor lives, which
/// costs no CPU, and dies with the client, at which point the kernel
/// reparents the monitor to init and reaps it there.
fn supervise(started: Box<dyn Started>) {
    std::thread::spawn(move || started.reap());
}

/// Attach to this repo's monitor, starting one if none is running.
///
/// Connect-or-SPAWN, never connect-or-bind. A delivery client exits
/// when it delivers, so a client that became the monitor would tear
/// down the watcher and the source set on every wake and rebuild them
/// on the next — rebuilding exactly the duplication this plan exists
/// to remove (codex on a206455). The monitor is a process of its own,
/// and its ownership outlives any delivery.
/// The starting is a parameter so a caller's own tests can reach this
/// decision with no chance of starting a real monitor, which on a temp
/// repo would bind a socket under `/tmp` and take that repo's lease
/// behind the test's back. Production passes [`spawn_monitor`].
pub(crate) async fn attach_or_spawn_with(
    repo: &std::path::Path,
    who: Who,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
) -> Attached {
    attach_or_spawn_by(
        repo,
        who,
        ATTACH_BUDGET,
        SPAWN_SETTLE,
        SHARE_ATTEMPTS,
        spawn,
        connect_within,
    )
    .await
}

/// [`attach_or_spawn`] with the starting supplied, so the whole
/// decision is reachable without a process: no test here may start a
/// binary, and one that started the real monitor would leave it
/// holding this repo's lease after the test returned.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn attach_or_spawn_by(
    repo: &std::path::Path,
    who: Who,
    budget: Duration,
    settle: Duration,
    limit: u32,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>
    + Copy,
) -> Attached {
    let mut why = match attach_by(repo, who.clone(), budget, connect) {
        Attached::Shared(conn) => return Attached::Shared(conn),
        Attached::Alone(why) => why,
    };

    match spawn(repo) {
        // Handed over BEFORE any attach can return, so no exit path
        // below can be the one that drops it.
        Ok(started) => supervise(started),
        Err(e) => return Attached::Alone(format!("{why}, and starting one failed: {e}")),
    }

    // ONE spawn, then retries against it: a second would only lose the
    // lease race to the first. The attempts are bounded because a
    // holder that is merely suspended keeps its lease, so no amount of
    // retrying takes it (codex on ca7a4c2).
    let mut attempts = 1;
    while !give_up_sharing(attempts, limit) {
        // Async, so a client waiting for the monitor to come up does
        // not park the runtime thread it is waiting on.
        tokio::time::sleep(settle / limit).await;
        attempts += 1;
        match attach_by(repo, who.clone(), budget, connect) {
            Attached::Shared(conn) => return Attached::Shared(conn),
            Attached::Alone(next) => why = next,
        }
    }
    Attached::Alone(format!(
        "started a monitor for {}, but {why}",
        repo.display()
    ))
}

/// How often a fold subscriber looks up from its socket.
///
/// Only so it can notice it has been asked to stop; a quiet monitor
/// beats every `FOLD_TICK`, so a lapsed deadline here is not news and
/// is never read as one.
const SUBSCRIBE_POLL: Duration = Duration::from_millis(500);

/// Subscribe to this repo's fold, read-only, or report why not.
///
/// Attaches but never STARTS one. A panel is not a reason to put a
/// daemon on somebody's machine, and the fallback costs only what it
/// cost before this existed — its own watcher.
pub(crate) fn subscribe(repo: &std::path::Path) -> Attached {
    attach_by(repo, Who::Observer, ATTACH_BUDGET, connect_within)
}

/// Forward a monitor's beats onto `wake` until the sharing ends, then
/// hand the job back to a local watcher for good.
///
/// On its own thread: `Conn` reads a blocking socket, and the callers
/// are the status loop and the TUI, neither of which has a place to
/// await. `stop` is checked between reads, which is why the read has a
/// deadline at all.
///
/// A lapsed deadline is NOT loss — beats come every `FOLD_TICK` and
/// this looks more often than that — so staleness is judged by how
/// long it has been since the last beat, which is the one question a
/// client can answer about a monitor that has gone quiet.
fn forward_beats(
    mut conn: Conn,
    repo: std::path::PathBuf,
    wake: std::sync::mpsc::Sender<()>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut last = std::time::Instant::now();
        loop {
            if stop.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            match conn.note(std::time::Instant::now() + SUBSCRIBE_POLL) {
                Ok(_) => {
                    last = std::time::Instant::now();
                    if wake.send(()).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    if beat_overdue(last.elapsed(), BEAT_GRACE) {
                        eprintln!(
                            "clank: the monitor for {} went quiet; watching this repo directly",
                            repo.display()
                        );
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Losing the monitor must not leave a panel frozen on stale
        // state, so this thread takes over the watching itself rather
        // than returning and leaving nobody.
        let Some(watcher) = reconcile_after(
            &wake,
            crate::repo_watch::RepoStateWatcher::attach(&repo, false, wake.clone()),
        ) else {
            return;
        };
        while !stop.load(std::sync::atomic::Ordering::Acquire) {
            std::thread::sleep(SUBSCRIBE_POLL);
        }
        drop(watcher);
    });
}

/// Take over from the monitor, and reconcile the gap it leaves.
///
/// The wake is the point. A change that landed after the last beat and
/// before `attached` existed is BASELINED by that watcher rather than
/// reported, so the consumer would sit on stale output until some later
/// edit or its own backstop — and the handoff happens exactly when the
/// repo is most likely to have changed, since that is why the monitor
/// is gone (codex on 697807a).
///
/// Separated from the attaching because the wake is otherwise
/// indistinguishable from the watcher's own startup traffic: a test
/// that watched a real directory passed whether or not this sent
/// anything, which is a test that proves nothing. The ordering is
/// still guaranteed — `attached` is evaluated before the call.
fn reconcile_after<W>(
    wake: &std::sync::mpsc::Sender<()>,
    attached: anyhow::Result<W>,
) -> Option<W> {
    let watcher = match attached {
        Ok(watcher) => watcher,
        // Nothing is watching, so a wake would promise a freshness
        // that is not coming.
        Err(e) => {
            eprintln!("clank: cannot watch this repo directly either: {e}");
            return None;
        }
    };
    let _ = wake.send(());
    Some(watcher)
}

/// A subscription to the repo's fold, or a watcher of our own.
///
/// Dropping it stops whichever it turned out to be. The two cases are
/// behind one type because a caller has no decision to make: it wants
/// to be woken when the repo changes, and whether that comes from the
/// shared fold or a local watcher is this module's business.
pub(crate) struct CoreWakes {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Held only in the unshared case; the shared one keeps its
    /// watcher, if it ever needs one, inside its thread.
    _local: Option<crate::repo_watch::RepoStateWatcher>,
}

impl Drop for CoreWakes {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Be woken on repo-state changes, through the monitor if there is one.
///
/// This is the duplication the plan measured: every `clank tui` and
/// every `clank status --watch` attached a watcher of its own and
/// refolded on every change, alongside one per parked wait. A
/// subscriber computes nothing — it is told the fold moved and rebuilds
/// only what it displays.
pub(crate) fn core_wakes(
    repo: &std::path::Path,
    wake: std::sync::mpsc::Sender<()>,
) -> anyhow::Result<CoreWakes> {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    match subscribe(repo) {
        Attached::Shared(conn) => {
            forward_beats(conn, repo.to_path_buf(), wake, stop.clone());
            Ok(CoreWakes { stop, _local: None })
        }
        Attached::Alone(why) => {
            tracing::debug!("watching {} directly: {why}", repo.display());
            Ok(CoreWakes {
                stop,
                _local: Some(crate::repo_watch::RepoStateWatcher::attach(
                    repo, false, wake,
                )?),
            })
        }
    }
}

/// Start this repo's monitor as a process of its own.
///
/// Its own process group, so a tool that signals the agent's group
/// cannot take the repo's watcher down with it, and null stdio so it
/// neither writes into the client's terminal nor dies with its pipes.
/// A spawn that loses the lease race exits by itself, which is why
/// starting one needs no coordination with the other clients.
///
/// It is still this process's CHILD — `process_group` moves the group,
/// not the parent — so it needs [`supervise`] to reap it, and only the
/// client's own exit reparents it to init.
pub(crate) fn spawn_monitor(repo: &std::path::Path) -> std::io::Result<Box<dyn Started>> {
    use std::os::unix::process::CommandExt;

    std::process::Command::new(std::env::current_exe()?)
        .args(["monitor", "start", "--repo"])
        .arg(repo)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|child| Box::new(Spawned(child)) as Box<dyn Started>)
}

/// Where a repo's lease and descriptor live. Plain files, bound by
/// PATH_MAX rather than `sun_path`, so the repo's depth costs nothing.
fn lock_path(repo: &std::path::Path) -> std::path::PathBuf {
    repo.join(".clank").join("monitor.lock")
}

fn descriptor_path(repo: &std::path::Path) -> std::path::PathBuf {
    repo.join(".clank").join("monitor.json")
}

/// Held for the life of the monitor. Dropping it — including by dying —
/// frees it, so there is no stale-holder protocol to get wrong.
#[derive(Debug)]
pub(crate) struct MonitorLease {
    _file: std::fs::File,
}

/// Take a repo's monitor lease, or report who has it.
///
/// The lease IS the liveness, as it is for `StatusLease`: if it can be
/// taken, the previous owner is gone, whatever a descriptor still says.
/// The pid in the descriptor is for diagnostics and is never the
/// authority — a pid can be reused, a lease cannot.
pub(crate) fn acquire_lease(repo: &std::path::Path) -> Result<MonitorLease, String> {
    use std::os::fd::AsRawFd;

    let path = lock_path(repo);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| {
            format!(
                "cannot open this repo's monitor lease at {}: {e}",
                path.display()
            )
        })?;
    // SAFETY: valid owned fd; flock has no memory effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(format!(
            "another monitor already owns {}: its lease at {} is held",
            repo.display(),
            path.display()
        ));
    }
    Ok(MonitorLease { _file: file })
}

/// Publish a descriptor by RENAME, never by truncating in place.
///
/// A client reads this without the lease, so a partial write would be
/// read as a corrupt descriptor — and truncating in place can leave the
/// tail of a longer predecessor behind when the new one is shorter.
/// Rename is atomic within a directory, so a reader sees one whole
/// descriptor or the previous one (codex on 06ccd66).
pub(crate) fn publish(repo: &std::path::Path, d: &Descriptor) -> std::io::Result<()> {
    let final_path = descriptor_path(repo);
    if let Some(parent) = final_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Named for the writer so a stray temp from a dead monitor cannot
    // be mistaken for this one's.
    let tmp = final_path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(d)?)?;
    std::fs::rename(&tmp, &final_path)
}

/// What a repo's monitor published, if anything readable.
pub(crate) fn read_published(repo: &std::path::Path) -> Option<Descriptor> {
    let raw = std::fs::read(descriptor_path(repo)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Whether a client in `repo` may use what it found.
///
/// Both halves, because a descriptor outlives what it describes: the
/// repo may have moved under it, and the monitor may predate this
/// build. Either way the client does its own local wait rather than
/// trusting it.
pub(crate) fn usable(d: &Descriptor, repo: &std::path::Path, ours: u32) -> bool {
    serves(repo, &d.repo) && admits(ours, d.proto)
}

/// One connected waiter, as the fold owner sees it.
///
/// A QUEUE, never a socket. An earlier version held the stream and
/// awaited each write inside the owner's tick: a timeout there bounds
/// one write but not the loop, so three unread clients held the owner
/// for three budgets while it could not accept, drain arrivals or tick
/// — long enough for a healthy newcomer to exhaust its whole attach
/// budget unaccepted (codex on 38088ac). The owner now enqueues and
/// moves on; a writer task per client owns the socket.
/// What the monitor knows about a connected reader.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Served {
    Agent {
        author: AgentLabel,
        /// Declared, never owned: a `--event` source belongs to ONE
        /// invocation, so the monitor must not pool it.
        #[allow(dead_code)]
        has_cli_sources: bool,
    },
    Observer,
}

#[derive(Debug)]
struct Client {
    who: Served,
    out: tokio::sync::mpsc::Sender<String>,
}

/// How many frames a client may fall behind before it is retired.
///
/// Small on purpose. The queue absorbs a brief hiccup; a reader that
/// cannot keep up with a 5-second fold tick is not slow, it is gone,
/// and holding frames for it would turn one stalled agent into
/// unbounded memory in the owner.
const CLIENT_QUEUE: usize = 2;

/// `clank monitor start`.
pub async fn run(args: super::MonitorArgs) -> anyhow::Result<()> {
    match args.command {
        super::MonitorCmd::Start(a) => start(a).await,
    }
}

/// How often the fold loop ticks when nothing changes.
///
/// The tick is the evidence of life — a quiet repo produces no new
/// state, so a client watching for silence needs the loop to speak
/// even when the repo does not.
const FOLD_TICK: Duration = Duration::from_secs(5);

/// How often the watchdog looks, and how many still looks end it.
///
/// `FOLD_TICK` is the loop's own cadence, so four still looks at a
/// five-second interval are four chances the loop had to advance and
/// did not. Twenty seconds is also `BEAT_GRACE`: a client gives up on
/// a silent monitor at about the moment that monitor stands down, so
/// the fallback and the freed lease arrive together and the next park
/// starts a fresh one.
///
/// The tension is real and settled deliberately. Too short kills a
/// monitor whose fold is merely slow on a large repo; too long leaves
/// the lease held by a process nobody can use. A needless restart
/// costs one spawn — clients watch locally meanwhile — while a lease
/// held forever is the failure this whole mechanism exists to end.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);
const WATCHDOG_MISSES: u32 = 4;

/// Standing down is not success, and not the caller's failure either.
/// Nothing reads it — a spawned monitor's stdio is null and its
/// supervisor only reaps — so this is for a human running it by hand.
const STOOD_DOWN_EXIT: i32 = 75;

/// How long one client may take to accept a frame.
const WRITE_BUDGET: Duration = Duration::from_secs(2);

async fn start(args: super::MonitorStartArgs) -> anyhow::Result<()> {
    let repo = super::resolve_repo(args.repo.as_deref())?;
    let repo = std::sync::Arc::new(dunce::canonicalize(&repo).unwrap_or(repo));

    // The lease first: everything after it assumes no other owner,
    // including clearing a socket a dead one left behind.
    let lease = acquire_lease(&repo).map_err(|e| anyhow::anyhow!("{e}"))?;
    let me = unsafe { libc::getuid() };
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let pid = std::process::id() as i32;
    let (listener, socket) = bind_listener(&socket_dirs(me, xdg.as_deref()), pid, me, &lease)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Published with tick 0: nothing has been folded yet, and the
    // stamp may only ever claim a fold that COMPLETED.
    let mut fold = Fold {
        tick: 0,
        at_ms: now_ms(),
    };
    publish(&repo, &describe(&repo, pid, &socket, fold))?;
    println!(
        "monitor for {} listening on {}",
        repo.display(),
        socket.display()
    );

    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let (joined, mut arrivals) = tokio::sync::mpsc::unbounded_channel::<Client>();
    let mut clients: Vec<Client> = Vec::new();

    // THE watcher for this repo. One, here, instead of one per parked
    // wait and two per `clank tui` — the whole point of the plan: ~170
    // of them were attached on the machine this was measured on, each
    // refolding the repo on every change.
    let (fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
    let _watcher = crate::repo_watch::RepoStateWatcher::attach(&repo, false, fs_tx)?;
    let mut beats = beats(fs_rx, FOLD_TICK);

    // Stamped by the fold loop below and by nothing else. A stamp from
    // the accept loop would prove only that the process breathes —
    // which is precisely the wedged monitor it has to detect.
    let heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(fold.tick));
    watch_the_fold(heartbeat.clone(), WATCHDOG_TICK, WATCHDOG_MISSES);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let repo = repo.clone();
                let joined = joined.clone();
                // Handshaking OFF this loop is the point: a peer that
                // connects and says nothing must not stall folding or
                // anyone else's registration.
                tokio::spawn(async move {
                    match greet(stream, &repo).await {
                        Ok(client) => {
                            let _ = joined.send(client);
                        }
                        Err(e) => eprintln!("monitor: a client was not admitted: {e}"),
                    }
                });
            }
            Some(client) = arrivals.recv() => clients.push(client),
            // A beat: the FS event or the heartbeat, whichever landed.
            // Receiving from a channel is cancel-safe and carries no
            // state, which is the point — see `beats`.
            beat = beats.recv() => {
                if beat.is_none() {
                    anyhow::bail!("the repo watcher stopped");
                }
                // Fold BEFORE claiming one. The stamp means a fold
                // completed, so publishing it first and then failing
                // the rebuild would advertise progress that never
                // happened — the invariant this plan set for itself
                // (codex on 13e9fa7).
                match advance(&repo, pid, &socket, fold, &heartbeat).await {
                    Some((next, state)) => {
                        fold = next;
                        serve(&repo, &mut clients, fold, &state);
                    }
                    // The stamp stays where it is. A client reading a
                    // stale fold is exactly how it should notice.
                    None => eprintln!("monitor: fold failed, stamp held at {}", fold.tick),
                }
            }
        }
    }
}

/// Watch the fold from a thread of its own, and stand down if it stops.
///
/// Not a task: the runtime is what may be wedged, and a task on a
/// stalled executor is exactly as stuck as the loop it is meant to
/// judge. A thread is scheduled by the kernel regardless.
///
/// It reads the loop's OWN tick, not the published descriptor — the
/// stamp on disk is written by the fold loop too, but going through
/// the filesystem would add a way for this to be wrong (an unwritable
/// `.clank/` would read as a stall) without adding anything it can
/// learn.
fn watch_the_fold(tick: std::sync::Arc<std::sync::atomic::AtomicU64>, every: Duration, limit: u32) {
    std::thread::spawn(move || {
        let mut seen = tick.load(std::sync::atomic::Ordering::Acquire);
        let mut missed = 0;
        loop {
            std::thread::sleep(every);
            let now = tick.load(std::sync::atomic::Ordering::Acquire);
            match watchdog_look(seen, now, missed, limit) {
                Watch::Keep { tick, missed: m } => {
                    seen = tick;
                    missed = m;
                }
                Watch::StandDown => {
                    eprintln!(
                        "monitor: the fold has not advanced past {seen} in {:?}; standing down so the lease frees",
                        every * limit
                    );
                    std::process::exit(STOOD_DOWN_EXIT);
                }
            }
        }
    });
}

/// Beats, produced independently of client traffic.
///
/// `Beat::next` is a fresh `sleep(tick)` racing the watcher channel and
/// then a 200ms debounce — THREE awaits. Rebuilding it inside the
/// owner's select was wrong twice (codex on 24ea491):
///
/// - every accepted connection or arrival dropped the future and
///   restarted the sleep, so client traffic faster than `tick` could
///   postpone the heartbeat forever and a newly admitted client never
///   got its first items;
/// - and it could consume a filesystem wake and then be cancelled
///   during the debounce, so the channel item was gone and the owner
///   never saw the beat. A dropped repo change is a silent agent, which
///   is the failure this plan exists to prevent.
///
/// A dedicated task owns the `Beat` instead. Nothing cancels it, so its
/// state cannot be discarded, and the owner only ever awaits a channel
/// receive — which holds no state to lose. Capacity 1 coalesces beats
/// for a busy owner rather than queueing them, and the watcher's own
/// channel buffers the events meanwhile.
fn beats(fs_rx: std::sync::mpsc::Receiver<()>, tick: Duration) -> tokio::sync::mpsc::Receiver<()> {
    let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
    tokio::spawn(async move {
        let mut beat = crate::cli::wait::Beat::bridge(fs_rx);
        loop {
            if beat.next(tick).await.is_err() {
                break;
            }
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
    rx
}

fn describe(repo: &std::path::Path, pid: i32, socket: &std::path::Path, fold: Fold) -> Descriptor {
    Descriptor {
        proto: PROTO,
        pid,
        repo: repo.to_string_lossy().to_string(),
        socket: socket.to_string_lossy().to_string(),
        fold,
    }
}

/// Fold, and publish the stamp ONLY if that succeeded.
///
/// The stamp means a fold completed. Advancing it first and then
/// failing the rebuild would publish progress that never happened, and
/// a loop that kept failing would keep advertising freshness — the
/// invariant this plan set for itself, inverted (codex on 13e9fa7).
async fn advance(
    repo: &std::path::Path,
    pid: i32,
    socket: &std::path::Path,
    fold: Fold,
    heartbeat: &std::sync::atomic::AtomicU64,
) -> Option<(Fold, crate::repo_state::RepoState)> {
    let outcome = folded(repo).await;
    advance_with(repo, pid, socket, fold, heartbeat, outcome)
}

/// The ORDERING, with the fold's outcome supplied.
///
/// Split out because the rebuild is fail-soft: probed here, it returns
/// `Ok` for a repo with no git, for a broken `.git` file and for an
/// empty `.git` directory, so a failing fold cannot be provoked
/// through it and the invariant would go untested. What matters is the
/// order — nothing is published unless a fold completed — and that is
/// decided here.
///
/// The watchdog's `heartbeat` is stamped here too, and ONLY here. It
/// is the same claim as the descriptor's, so it belongs to the same
/// decision: a heartbeat the accept loop could touch would prove the
/// process breathes, which is exactly the wedged monitor the watchdog
/// exists to catch. The caller is given no opportunity to stamp it.
fn advance_with(
    repo: &std::path::Path,
    pid: i32,
    socket: &std::path::Path,
    fold: Fold,
    heartbeat: &std::sync::atomic::AtomicU64,
    outcome: anyhow::Result<crate::repo_state::RepoState>,
) -> Option<(Fold, crate::repo_state::RepoState)> {
    let state = match outcome {
        Ok(state) => state,
        Err(e) => {
            eprintln!("monitor: {e}");
            return None;
        }
    };
    let next = Fold {
        tick: fold.tick + 1,
        at_ms: now_ms(),
    };
    let _ = publish(repo, &describe(repo, pid, socket, next));
    heartbeat.store(next.tick, std::sync::atomic::Ordering::Release);
    Some((next, state))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// One completed fold of the repo.
async fn folded(repo: &std::path::Path) -> anyhow::Result<crate::repo_state::RepoState> {
    crate::rebuild::rebuild_repo_with_policy(repo, crate::rebuild::CachePolicy::Use)
        .await
        .map_err(|e| anyhow::anyhow!("failed to fold repo `{}`: {e}", repo.display()))
}

/// Whether an error means the author is GONE rather than unreadable.
///
/// `wait` treats `NotRegistered` as revocation and stops that waiter;
/// anything else is transient and fail-soft. A monitor that kept a
/// revoked client would hold a connection open forever without beats,
/// and — since that branch never writes — could not even notice the
/// client had disconnected.
fn revoked(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<crate::agent_store::RoleResolutionError>(),
        Some(crate::agent_store::RoleResolutionError::NotRegistered { .. })
    )
}

/// Read a client's hello and answer it, bounded and asynchronous.
async fn greet(stream: tokio::net::UnixStream, repo: &std::path::Path) -> anyhow::Result<Client> {
    greet_within(stream, repo, ATTACH_BUDGET).await
}

/// [`greet`] with the budget supplied, so the timeout is testable
/// without waiting the production five seconds.
async fn greet_within(
    stream: tokio::net::UnixStream,
    repo: &std::path::Path,
    budget: Duration,
) -> anyhow::Result<Client> {
    tokio::time::timeout(budget, admit(stream, repo))
        .await
        .map_err(|_| anyhow::anyhow!("a client did not finish its hello within {budget:?}"))?
}

async fn admit(stream: tokio::net::UnixStream, repo: &std::path::Path) -> anyhow::Result<Client> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mut io = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    if io.read_line(&mut line).await? == 0 {
        anyhow::bail!("a client closed before saying hello");
    }

    // An agent's author is resolved BEFORE admission: welcoming one the
    // roster does not know would register a connection nothing can ever
    // serve. An observer asks for no projection, so there is nothing to
    // resolve and nothing a roster could authorise — refusing it for
    // lacking an identity would only mean a panel could not subscribe.
    let admitted = accept_hello(line.trim_end(), repo).and_then(|hello| match hello.who {
        Who::Observer => Ok(Served::Observer),
        Who::Agent {
            author,
            has_cli_sources,
            ..
        } => {
            let author = AgentLabel::parse(&author).map_err(|e| Welcome::Decline {
                proto: PROTO,
                why: format!("invalid author `{author}`: {e}"),
            })?;
            match crate::agent_store::resolve_role(repo, &author) {
                Ok(_) => Ok(Served::Agent {
                    author,
                    has_cli_sources,
                }),
                Err(e) => Err(Welcome::Decline {
                    proto: PROTO,
                    why: format!("`{author}` is not on this repo's roster: {e}"),
                }),
            }
        }
    });

    let welcome = match &admitted {
        Ok(_) => Welcome::Ok {
            proto: PROTO,
            fold: Fold {
                tick: 0,
                at_ms: now_ms(),
            },
        },
        Err(decline) => decline.clone(),
    };
    io.get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&welcome)?).as_bytes())
        .await?;

    let who = admitted.map_err(|d| anyhow::anyhow!("{d:?}"))?;
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    tokio::spawn(write_to(io.into_inner(), queued, who.name()));
    Ok(Client { who, out })
}

impl Served {
    /// What to call this reader in a log line.
    fn name(&self) -> String {
        match self {
            Served::Agent { author, .. } => author.to_string(),
            Served::Observer => "an observer".to_string(),
        }
    }
}

/// Drive one client's socket, independently of the fold.
///
/// Owning the socket here is what makes the owner's enqueue
/// non-blocking. A write that times out or fails ends the task, which
/// closes the queue — and a closed queue is how the owner learns the
/// client is gone, without ever touching the socket itself.
async fn write_to(
    socket: tokio::net::UnixStream,
    queued: tokio::sync::mpsc::Receiver<String>,
    author: String,
) {
    write_to_within(socket, queued, author, WRITE_BUDGET).await
}

/// [`write_to`] with the budget supplied, so giving up is testable
/// without waiting the production two seconds.
async fn write_to_within(
    mut socket: tokio::net::UnixStream,
    mut queued: tokio::sync::mpsc::Receiver<String>,
    author: String,
    budget: Duration,
) {
    use tokio::io::AsyncWriteExt;

    while let Some(line) = queued.recv().await {
        match tokio::time::timeout(budget, socket.write_all(line.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("monitor: `{author}` closed: {e}");
                return;
            }
            Err(_) => {
                eprintln!("monitor: `{author}` stopped reading");
                return;
            }
        }
    }
}

/// Tell each client what ITS author is owed, from one shared fold.
///
/// The fold and the derived status are repo-level — `WorkPolicy` comes
/// from the repo's config and its roster tiers, not from the author —
/// so both are computed once by the caller. Only the role, and
/// therefore `items_for`, is per author, and the role is re-read every
/// tick so a roster change reaches a connected client rather than
/// stranding it.
fn serve(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    fold: Fold,
    state: &crate::repo_state::RepoState,
) {
    if clients.is_empty() {
        return;
    }
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);

    let mut keep = Vec::with_capacity(clients.len());
    for client in clients.drain(..) {
        let note = match &client.who {
            // No projection is derived for an observer, which is the
            // point: it asked to know the fold moved, and deriving
            // somebody's work to tell it that would be the duplicated
            // refold this plan exists to delete.
            Served::Observer => Note::Beat { fold },
            Served::Agent { author, .. } => {
                let inputs = match crate::cli::wait::derive_wait_inputs(repo, author) {
                    Ok(inputs) => inputs,
                    Err(e) if revoked(&e) => {
                        eprintln!("monitor: `{author}` left the roster; dropping it");
                        continue;
                    }
                    // Transient: keep the client, try again next tick.
                    Err(_) => {
                        keep.push(client);
                        continue;
                    }
                };
                let status = state
                    .fold
                    .derive_status(&reviews, &inputs.work_policy, head.as_ref());
                let items = crate::cli::wait::owed_for(repo, author, &inputs, &status, &state.fold);
                if items.is_empty() {
                    Note::Beat { fold }
                } else {
                    Note::Items { items, fold }
                }
            }
        };
        let Ok(line) = serde_json::to_string(&note) else {
            keep.push(client);
            continue;
        };
        // `try_send`, never `send`: the owner must not wait on a
        // client. A full queue means this reader has fallen a whole
        // `CLIENT_QUEUE` of ticks behind, and a closed one means its
        // writer task has already given up — both are retirement.
        match client.out.try_send(format!("{line}\n")) {
            Ok(()) => keep.push(client),
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                eprintln!(
                    "monitor: `{}` is {CLIENT_QUEUE} frames behind; dropping it",
                    client.who.name()
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                eprintln!("monitor: `{}` is gone; dropping it", client.who.name());
            }
        }
    }
    *clients = keep;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(tick: u64, at_ms: u64) -> Fold {
        Fold { tick, at_ms }
    }

    #[test]
    fn a_version_mismatch_declines_rather_than_fails() {
        assert!(admits(PROTO, PROTO));
        assert!(!admits(PROTO, PROTO + 1), "a newer monitor is not ours");
        assert!(!admits(PROTO, PROTO - 1), "nor the version before this");
        assert!(!admits(PROTO - 1, PROTO), "in either direction");
    }

    /// The ordering that makes a skew legible.
    ///
    /// The version is read FIRST, from a frame that decodes under any
    /// version, and a mismatch is a decline. What the body would do is
    /// deliberately not relied on: an older frame might fail to decode,
    /// or might decode with a missing `Option` silently read as `None`
    /// — a bound client served as an unbound one. Neither is a signal,
    /// so neither is consulted (codex on 4915f33).
    #[test]
    fn an_older_frame_is_declined_by_its_version() {
        let proto1 = r#"{"proto":1,"author":"codex","session":null,"has_cli_sources":false}"#;

        let envelope: Envelope = serde_json::from_str(proto1).expect("the version reads first");
        assert_eq!(envelope.proto, 1);
        assert!(!admits(PROTO, envelope.proto), "and that is the decline");
    }

    /// An unbound terminal wait must stay representable.
    #[test]
    fn a_wait_that_holds_nothing_says_so() {
        let hello = Hello {
            proto: PROTO,
            repo: "/w/proj".into(),
            who: agent("claude"),
        };
        let back: Hello = serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
        assert_eq!(back, hello, "no session is not an absent field");
    }

    /// And a half-pair must not be.
    #[test]
    fn half_a_holder_does_not_parse() {
        for raw in [r#"{"tool":"codex"}"#, r#"{"session":"s-1"}"#] {
            assert!(
                serde_json::from_str::<Holder>(raw).is_err(),
                "a half holder invents or loses an identity: {raw}"
            );
        }
    }

    /// The wedge case: the socket answers, the fold does not advance.
    /// A client sees it as silence because the beat is the loop's.
    #[test]
    fn silence_and_staleness_are_one_signal_to_a_client() {
        let grace = Duration::from_secs(30);
        assert!(!beat_overdue(Duration::from_secs(29), grace), "inside");
        assert!(!beat_overdue(grace, grace), "exactly at it");
        assert!(
            beat_overdue(grace + Duration::from_millis(1), grace),
            "past it"
        );
    }

    /// The reason the socket is not under the repo.
    #[test]
    fn a_socket_path_does_not_grow_with_the_repo() {
        // Under the repo, this one spends 95 of the 104 before the
        // filename; the candidates spend a constant 20-odd.
        let deep = "/Users/llfourn/src/frostsnap-ci/.clank/worktrees/nonce-aware-coin-selection";
        assert!(
            format!("{deep}/.clank/monitor.sock").len() > 90,
            "the case this avoids"
        );
        for dir in socket_dirs(501, None) {
            assert!(
                socket_path(&dir, 99999).is_some(),
                "{} must hold a socket",
                dir.display()
            );
        }
    }

    /// A candidate's own length is not known in advance.
    ///
    /// `$XDG_RUNTIME_DIR` is read from the environment and is tried
    /// FIRST, so it is the one candidate that can be too long. Skipping
    /// it must leave the shorter ones usable rather than failing the
    /// monitor.
    #[test]
    fn a_runtime_dir_too_long_for_a_socket_is_skipped_not_fatal() {
        let long = std::path::PathBuf::from(format!("/run/user/501/{}", "d".repeat(120)));
        let dirs = socket_dirs(501, Some(&long));

        assert!(
            socket_path(&dirs[0], 99999).is_none(),
            "the long candidate cannot hold one"
        );
        assert!(
            dirs[1..].iter().any(|d| socket_path(d, 99999).is_some()),
            "and a shorter candidate still can"
        );
    }

    /// The boundary itself, since a path that is exactly the limit does
    /// not fit once the kernel's terminator is counted.
    #[test]
    fn the_length_boundary_leaves_room_for_the_terminator() {
        // Pinned as a LITERAL, not through the constant: every other
        // assertion here is relative to it, so they all move with it
        // and a widening to Linux's 108 would pass them while failing
        // to bind on macOS.
        assert_eq!(
            SUN_PATH_MAX, 104,
            "macOS `sockaddr_un`, the stricter of the two platforms"
        );

        let pad = |n: usize| std::path::PathBuf::from(format!("/{}", "d".repeat(n)));
        let name = "/99999.sock".len();
        // dir + "/99999.sock" lands exactly on SUN_PATH_MAX.
        let exact = pad(SUN_PATH_MAX - name - 1);
        assert_eq!(
            exact.join("99999.sock").to_string_lossy().len(),
            SUN_PATH_MAX
        );
        assert!(socket_path(&exact, 99999).is_none(), "exactly at it is out");
        assert!(
            socket_path(&pad(SUN_PATH_MAX - name - 2), 99999).is_some(),
            "one under it is in"
        );
    }

    /// The shared directory first, the private one as the fallback.
    #[test]
    fn a_private_directory_is_the_fallback_not_the_default() {
        let dirs = socket_dirs(501, Some(std::path::Path::new("/run/user/501")));
        let shown: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
        assert_eq!(
            shown,
            vec![
                "/run/user/501/clank-socks".to_string(),
                "/tmp/clank-socks".to_string(),
                "/tmp/clank-socks-501".to_string(),
            ]
        );
        assert!(
            !socket_dirs(501, None).iter().any(|d| d.starts_with("/run")),
            "no XDG dir, no XDG candidate"
        );
    }

    /// `/tmp` is world-writable, so the directory has to be ours and
    /// ours alone before a socket goes in it.
    #[test]
    fn a_directory_someone_else_can_write_is_refused() {
        assert!(dir_is_acceptable(501, 0o700, 501));
        assert!(!dir_is_acceptable(502, 0o700, 501), "another user owns it");
        assert!(!dir_is_acceptable(501, 0o770, 501), "group-WRITABLE");
        assert!(!dir_is_acceptable(501, 0o777, 501), "world-writable");
        assert!(!dir_is_acceptable(501, 0o720, 501), "group-write alone");
        assert!(!dir_is_acceptable(501, 0o702, 501), "other-write alone");
        // Read and traverse are not the threat: the socket is 0600, so
        // listing the directory buys nothing, and a 022 umask makes
        // this the ordinary case for a dir we just created.
        assert!(dir_is_acceptable(501, 0o755, 501), "readable is fine");
        assert!(dir_is_acceptable(501, 0o500, 501), "tighter is fine");
    }

    /// A descriptor can outlive the repo it describes.
    #[test]
    fn a_descriptor_for_another_repo_declines() {
        let ours = std::path::Path::new("/w/proj");
        assert!(serves(ours, "/w/proj"));
        assert!(!serves(ours, "/w/other"));
        assert!(
            !serves(ours, "/w/proj-copy"),
            "a prefix is a different repo"
        );
    }

    #[test]
    fn a_descriptor_round_trips() {
        let d = Descriptor {
            proto: PROTO,
            pid: 4242,
            repo: "/w/proj".into(),
            socket: "/tmp/clank-socks-501/4242.sock".into(),
            fold: fold(9, 10),
        };
        let back: Descriptor = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }

    fn repo_dir() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".clank")).unwrap();
        d
    }

    fn descriptor_for(repo: &std::path::Path) -> Descriptor {
        Descriptor {
            proto: PROTO,
            pid: 4242,
            repo: repo.to_string_lossy().to_string(),
            socket: "/tmp/clank-socks-501/4242.sock".into(),
            fold: fold(1, 2),
        }
    }

    /// One monitor per repo, decided by the kernel.
    #[test]
    fn a_second_monitor_cannot_take_the_lease() {
        let repo = repo_dir();
        let first = acquire_lease(repo.path()).expect("the first takes it");

        let second = acquire_lease(repo.path());
        let why = second.expect_err("the second must not").to_string();
        assert!(why.contains("already owns"), "{why}");

        drop(first);
        acquire_lease(repo.path()).expect("and it frees on drop, with no cleanup protocol");
    }

    /// Two repos are two monitors.
    #[test]
    fn a_lease_is_per_repo() {
        let a = repo_dir();
        let b = repo_dir();
        let _held = acquire_lease(a.path()).unwrap();
        acquire_lease(b.path()).expect("another repo is not this one");
    }

    #[test]
    fn a_published_descriptor_reads_back() {
        let repo = repo_dir();
        assert_eq!(read_published(repo.path()), None, "nothing published yet");

        let d = descriptor_for(repo.path());
        publish(repo.path(), &d).unwrap();
        assert_eq!(read_published(repo.path()), Some(d));
    }

    /// Rename, not truncate-in-place.
    ///
    /// A shorter descriptor replacing a longer one is where an
    /// in-place write shows: it leaves the predecessor's tail behind
    /// and the reader gets trailing bytes after valid JSON. And the
    /// temp file must not survive, or a stale one accumulates per
    /// publish.
    #[test]
    fn publishing_replaces_a_longer_descriptor_wholly() {
        let repo = repo_dir();
        let mut long = descriptor_for(repo.path());
        long.socket = format!("/tmp/{}/4242.sock", "d".repeat(60));
        publish(repo.path(), &long).unwrap();

        let short = descriptor_for(repo.path());
        publish(repo.path(), &short).unwrap();

        assert_eq!(read_published(repo.path()), Some(short));
        let strays: Vec<_> = std::fs::read_dir(repo.path().join(".clank"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(strays.is_empty(), "a temp file survived: {strays:?}");
    }

    /// A descriptor outlives what it describes.
    #[test]
    fn a_descriptor_from_elsewhere_is_not_usable() {
        let repo = repo_dir();
        let mine = descriptor_for(repo.path());
        assert!(usable(&mine, repo.path(), PROTO));

        let moved = Descriptor {
            repo: "/w/somewhere-else".into(),
            ..mine.clone()
        };
        assert!(
            !usable(&moved, repo.path(), PROTO),
            "the repo moved under it"
        );

        let older = Descriptor {
            proto: PROTO - 1,
            ..mine
        };
        assert!(
            !usable(&older, repo.path(), PROTO),
            "and it predates this build"
        );
    }

    fn hello_line(proto: u32, repo: &str) -> String {
        serde_json::to_string(&serde_json::json!({
            "proto": proto,
            "repo": repo,
            "who": {"as": "agent", "author": "codex", "holder": null, "has_cli_sources": false},
        }))
        .unwrap()
    }

    #[test]
    fn a_matching_hello_is_accepted() {
        let repo = std::path::Path::new("/w/proj");
        let hello = accept_hello(&hello_line(PROTO, "/w/proj"), repo).expect("accepted");
        assert_eq!(hello.who, agent("codex"));
    }

    /// Every refusal is a Decline the client can act on, never a parse
    /// error it cannot tell from corruption.
    #[test]
    fn every_refusal_names_itself() {
        let repo = std::path::Path::new("/w/proj");
        let why = |line: &str| match accept_hello(line, repo) {
            Err(Welcome::Decline { why, proto }) => {
                assert_eq!(proto, PROTO, "a decline says what we speak");
                why
            }
            other => panic!("expected a decline, got {other:?}"),
        };

        assert!(why("not json at all").contains("not a monitor hello"));
        assert!(why(&hello_line(PROTO + 1, "/w/proj")).contains("client speaks"));
        assert!(why(&hello_line(PROTO, "/w/other")).contains("serves"));
        // Versioned as OURS — a literal here would start declining
        // for its version the next time PROTO moves, and would stop
        // testing the parse at all.
        assert!(
            why(&format!(
                r#"{{"proto":{PROTO},"who":{{"as":"agent","author":"codex"}}}}"#
            ))
            .contains("did not parse"),
            "a well-versioned frame missing the repo is still a decline"
        );
    }

    /// The version is consulted BEFORE the body, so a frame that is
    /// both older AND unparseable declines for its version.
    #[test]
    fn an_old_frame_declines_for_its_version_not_its_shape() {
        let repo = std::path::Path::new("/w/proj");
        // Proto 1: no repo, no holder. Unparseable as ours either way.
        let old = r#"{"proto":1,"author":"codex","session":null,"has_cli_sources":false}"#;
        match accept_hello(old, repo) {
            Err(Welcome::Decline { why, .. }) => assert!(
                why.contains("client speaks 1"),
                "the version decided, not the parse: {why}"
            ),
            other => panic!("expected a decline, got {other:?}"),
        }
    }

    /// The handshake over a real socket, in process — no binary is
    /// started, which this repo forbids in tests.
    #[test]
    fn the_handshake_crosses_a_real_socket() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::{UnixListener, UnixStream};

        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4242).expect("a tempdir holds a socket");
        let listener = UnixListener::bind(&path).expect("bind");

        let repo = std::path::PathBuf::from("/w/proj");
        let served = {
            let repo = repo.clone();
            std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let welcome = match accept_hello(line.trim_end(), &repo) {
                    Ok(_) => Welcome::Ok {
                        proto: PROTO,
                        fold: fold(1, 2),
                    },
                    Err(decline) => decline,
                };
                let mut out = reader.into_inner();
                writeln!(out, "{}", serde_json::to_string(&welcome).unwrap()).unwrap();
            })
        };

        let mut client = UnixStream::connect(&path).unwrap();
        writeln!(client, "{}", hello_line(PROTO, "/w/proj")).unwrap();
        let mut answer = String::new();
        BufReader::new(&client).read_line(&mut answer).unwrap();
        served.join().unwrap();

        let welcome: Welcome = serde_json::from_str(answer.trim_end()).unwrap();
        assert!(
            matches!(welcome, Welcome::Ok { proto, .. } if proto == PROTO),
            "{welcome:?}"
        );
    }

    /// The production bind path, not a harness around it.
    #[test]
    fn binding_takes_the_first_candidate_that_will_hold_a_socket() {
        let repo = repo_dir();
        let lease = acquire_lease(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();

        let too_long = home.path().join("d".repeat(120));
        let good = home.path().join("socks");
        let (_listener, path) = bind_listener(
            &[too_long.clone(), good.clone()],
            4242,
            unsafe { libc::getuid() },
            &lease,
        )
        .expect("the second candidate takes it");

        assert!(path.starts_with(&good), "{}", path.display());
        assert!(path.exists(), "the socket is there");
        assert!(!too_long.exists(), "and the long one was not created");

        // The directory keeps others out, and so does the socket: a
        // world-connectable one would let any local user talk to this
        // repo's monitor.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o177, 0, "socket mode {:o}", mode & 0o777);
    }

    /// A directory someone else could write is skipped, not used.
    #[test]
    fn a_loose_directory_is_skipped_for_the_next_candidate() {
        use std::os::unix::fs::PermissionsExt;
        let repo = repo_dir();
        let lease = acquire_lease(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();

        let loose = home.path().join("loose");
        std::fs::create_dir_all(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
        let tight = home.path().join("tight");

        let (_l, path) = bind_listener(
            &[loose.clone(), tight.clone()],
            4242,
            unsafe { libc::getuid() },
            &lease,
        )
        .expect("the tight one is used");
        assert!(path.starts_with(&tight), "{}", path.display());
    }

    /// A socket left by a dead monitor must not block the next one
    /// forever: binding over it is EADDRINUSE, and the lease is what
    /// makes removing it safe.
    #[test]
    fn a_stale_socket_file_does_not_block_a_new_monitor() {
        let repo = repo_dir();
        let lease = acquire_lease(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("socks");
        std::fs::create_dir_all(&dir).unwrap();

        let path = socket_path(&dir, 4242).unwrap();
        std::fs::write(&path, b"a corpse").unwrap();

        let (_l, bound) =
            bind_listener(&[dir], 4242, unsafe { libc::getuid() }, &lease).expect("binds anyway");
        assert_eq!(bound, path);
    }

    #[test]
    fn every_candidate_failing_says_why() {
        let repo = repo_dir();
        let lease = acquire_lease(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();
        let why = bind_listener(
            &[home.path().join("d".repeat(120))],
            4242,
            unsafe { libc::getuid() },
            &lease,
        )
        .expect_err("nothing usable")
        .to_string();
        assert!(why.contains("too long"), "{why}");
    }

    /// The creation race: `recursive(true)` succeeds on a directory
    /// that already exists, so a racer who creates it between our stat
    /// and our create would have theirs accepted unvetted.
    #[test]
    fn a_directory_created_under_us_is_still_vetted() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("socks");
        let me = unsafe { libc::getuid() };

        let planted = dir.clone();
        let why = ensure_dir_with(&dir, me, move |_| {
            // The racer wins: a world-writable directory now exists
            // where we were about to create our own 0700 one.
            std::fs::create_dir_all(&planted).unwrap();
            std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o777)).unwrap();
        })
        .expect_err("a planted directory must not be accepted");
        assert!(why.contains("mode 777"), "{why}");
    }

    /// And the ordinary path still creates and accepts its own.
    #[test]
    fn a_directory_we_create_is_accepted() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("socks");
        ensure_dir(&dir, unsafe { libc::getuid() }).expect("ours is fine");
        assert!(dir.is_dir());
    }

    /// A stat failure that is not NotFound is a reason to stop, not a
    /// licence to create: a file in the way must not be replaced.
    #[test]
    fn something_that_is_not_a_directory_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("socks");
        std::fs::write(&path, b"in the way").unwrap();
        let why = ensure_dir(&path, unsafe { libc::getuid() }).expect_err("refused");
        assert!(why.contains("not a directory"), "{why}");
    }

    /// A socket that cannot be restricted is abandoned, not published.
    #[test]
    fn a_socket_that_cannot_be_restricted_is_not_published() {
        let repo = repo_dir();
        let lease = acquire_lease(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();
        let first = home.path().join("first");
        let second = home.path().join("second");
        let me = unsafe { libc::getuid() };

        // Fails for the first candidate only, so the fallback is
        // exercised as well as the refusal.
        let first_for_hook = first.clone();
        let (_l, path) = bind_listener_with(
            &[first.clone(), second.clone()],
            4242,
            me,
            &lease,
            move |p: &std::path::Path| {
                if p.starts_with(&first_for_hook) {
                    Err("chmod refused".to_string())
                } else {
                    restrict(p)
                }
            },
        )
        .expect("the second candidate is used");

        assert!(path.starts_with(&second), "{}", path.display());
        assert!(
            !socket_path(&first, 4242).unwrap().exists(),
            "the abandoned socket was removed, not left reachable"
        );
    }

    #[test]
    fn restriction_failing_everywhere_is_an_error_not_a_socket() {
        let repo = repo_dir();
        let lease = acquire_lease(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();
        let only = home.path().join("only");
        let why = bind_listener_with(
            &[only.clone()],
            4242,
            unsafe { libc::getuid() },
            &lease,
            |_| Err("chmod refused".to_string()),
        )
        .expect_err("no socket may be returned");
        assert!(why.contains("chmod refused"), "{why}");
        assert!(
            !socket_path(&only, 4242).unwrap().exists(),
            "and none left behind"
        );
    }

    /// A symlink is refused for being a symlink, however good its
    /// target looks: the uid and mode would be the target's, and the
    /// link is replaceable by whoever owns it.
    #[test]
    fn a_symlinked_candidate_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let real = home.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let me = unsafe { libc::getuid() };

        // The target is ours and perfectly acceptable.
        ensure_dir(&real, me).expect("the target itself is fine");

        let why = ensure_dir(&link, me).expect_err("the link is not");
        assert!(why.contains("symlink"), "{why}");
    }

    /// And one planted in the creation gap, where `recursive(true)`
    /// succeeds because the path now exists.
    #[test]
    fn a_symlink_planted_under_us_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let real = home.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let dir = home.path().join("socks");

        let planted = dir.clone();
        let target = real.clone();
        let why = ensure_dir_with(&dir, unsafe { libc::getuid() }, move |_| {
            std::os::unix::fs::symlink(&target, &planted).unwrap();
        })
        .expect_err("a planted link must not be accepted");
        assert!(why.contains("symlink"), "{why}");
    }

    /// Refusing it lets the private fallback do its job.
    #[test]
    fn a_symlinked_candidate_falls_through_to_the_next() {
        let repo = repo_dir();
        let lease = acquire_lease(repo.path()).unwrap();
        let home = tempfile::tempdir().unwrap();

        let real = home.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let private = home.path().join("private");

        let (_l, path) = bind_listener(
            &[link.clone(), private.clone()],
            4242,
            unsafe { libc::getuid() },
            &lease,
        )
        .expect("the private candidate is used");
        assert!(path.starts_with(&private), "{}", path.display());
    }

    /// A stat that fails for a reason OTHER than NotFound must stop,
    /// not fall through to creating. Both outcomes are errors, so the
    /// message is what says which branch ran.
    #[test]
    fn a_stat_failure_that_is_not_absence_refuses_without_creating() {
        let home = tempfile::tempdir().unwrap();
        let blocker = home.path().join("blocker");
        std::fs::write(&blocker, b"a file, not a directory").unwrap();

        // Statting through a file gives ENOTDIR, not NotFound.
        let why =
            ensure_dir(&blocker.join("socks"), unsafe { libc::getuid() }).expect_err("refused");
        assert!(
            why.contains("cannot stat"),
            "the stat decided, not the create: {why}"
        );
    }

    /// The read-back, driven against a file made permissive on purpose.
    #[test]
    fn a_mode_that_did_not_take_is_reported() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("sock");
        std::fs::write(&path, b"").unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        verify_private(&path).expect("0600 is private");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let why = verify_private(&path).expect_err("0666 is not");
        assert!(why.contains("stayed at mode 666"), "{why}");
    }

    #[test]
    fn a_private_mode_is_owner_only() {
        assert!(mode_is_private(0o600));
        assert!(mode_is_private(0o400));
        assert!(!mode_is_private(0o660), "group");
        assert!(!mode_is_private(0o606), "other");
        assert!(!mode_is_private(0o666));
    }

    /// A monitor, for the length of one handshake. `reply` is written
    /// verbatim so a test can coalesce frames, or write nothing and
    /// stay open.
    fn fake_monitor(
        repo: &std::path::Path,
        reply: Option<String>,
    ) -> (tempfile::TempDir, std::thread::JoinHandle<Option<Hello>>) {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4242).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        publish(
            repo,
            &Descriptor {
                proto: PROTO,
                pid: 4242,
                repo: repo.to_string_lossy().to_string(),
                socket: path.to_string_lossy().to_string(),
                fold: fold(1, 2),
            },
        )
        .unwrap();

        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Read the hello a byte at a time: a BufReader here would
            // have the same read-ahead problem this test is about.
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            while stream.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
                line.push(byte[0]);
            }
            let heard = serde_json::from_slice::<Hello>(&line).ok();
            match reply {
                Some(reply) => {
                    stream.write_all(reply.as_bytes()).unwrap();
                    stream.flush().unwrap();
                    // Hold the connection open so the client reads from
                    // a live peer rather than racing an EOF.
                    std::thread::sleep(Duration::from_millis(200));
                }
                // Accept, then say nothing at all — and stay open
                // well past any budget a test sets, so a fallback is
                // evidence of the DEADLINE and not of an EOF.
                None => std::thread::sleep(Duration::from_millis(1500)),
            }
            heard
        });
        (dir, served)
    }

    fn welcome_line() -> String {
        format!(
            "{}\n",
            serde_json::to_string(&Welcome::Ok {
                proto: PROTO,
                fold: fold(3, 4),
            })
            .unwrap()
        )
    }

    #[test]
    fn a_client_attaches_and_the_monitor_hears_who_it_is() {
        let repo = repo_dir();
        let (_dir, served) = fake_monitor(repo.path(), Some(welcome_line()));

        let holder = Some(Holder {
            tool: clank_core::vocab::Tool::Codex,
            session: "s-9".into(),
        });
        let got = attach(
            repo.path(),
            Who::Agent {
                author: "codex".into(),
                holder: holder.clone(),
                has_cli_sources: true,
            },
        );
        assert!(matches!(got, Attached::Shared(_)), "{got:?}");

        let heard = served.join().unwrap().expect("a hello arrived");
        assert_eq!(
            heard.who,
            Who::Agent {
                author: "codex".into(),
                holder,
                has_cli_sources: true,
            },
            "the whole identity travelled, pair and sources alike"
        );
    }

    #[test]
    fn a_declining_monitor_sends_the_client_off_alone() {
        let repo = repo_dir();
        let decline = format!(
            "{}\n",
            serde_json::to_string(&Welcome::Decline {
                proto: PROTO,
                why: "serves somewhere else".into(),
            })
            .unwrap()
        );
        let (_dir, served) = fake_monitor(repo.path(), Some(decline));
        let got = attach(repo.path(), agent("codex"));
        served.join().unwrap();
        match got {
            Attached::Alone(why) => assert!(why.contains("declined"), "{why}"),
            other => panic!("expected Alone, got {other:?}"),
        }
    }

    /// Every way of failing is a reason to work alone, never to wait.
    #[test]
    fn nothing_about_a_missing_monitor_is_fatal() {
        let repo = repo_dir();

        let nothing = attach(repo.path(), agent("codex"));
        assert!(
            matches!(&nothing, Attached::Alone(why) if why.contains("no monitor published")),
            "{nothing:?}"
        );

        // A descriptor whose monitor is gone: the socket refuses.
        publish(
            repo.path(),
            &Descriptor {
                proto: PROTO,
                pid: 1,
                repo: repo.path().to_string_lossy().to_string(),
                socket: "/tmp/clank-socks-501/nobody-home.sock".into(),
                fold: fold(1, 2),
            },
        )
        .unwrap();
        let dead = attach(repo.path(), agent("codex"));
        assert!(
            matches!(&dead, Attached::Alone(why) if why.contains("cannot reach")),
            "{dead:?}"
        );

        // And one for a repo that is not this one.
        publish(
            repo.path(),
            &Descriptor {
                proto: PROTO,
                pid: 1,
                repo: "/w/elsewhere".into(),
                socket: "/tmp/clank-socks-501/elsewhere.sock".into(),
                fold: fold(1, 2),
            },
        )
        .unwrap();
        let foreign = attach(repo.path(), agent("codex"));
        assert!(
            matches!(&foreign, Attached::Alone(why) if why.contains("not ours")),
            "{foreign:?}"
        );
    }

    /// The read-ahead bug: a welcome and the first note in ONE write.
    ///
    /// `read_line` fills its buffer in chunks, so a throwaway reader
    /// swallows whatever followed the newline. The note must survive
    /// the handshake and be readable exactly once (codex on af1efd2).
    #[test]
    fn a_note_written_with_the_welcome_is_not_lost() {
        let repo = repo_dir();
        let note = Note::Items {
            items: vec![WaitItem::PromoteFromQueue {
                name: "next".into(),
                priority: 500,
            }],
            fold: fold(5, 6),
        };
        let coalesced = format!(
            "{}{}\n",
            welcome_line(),
            serde_json::to_string(&note).unwrap()
        );
        let (_dir, served) = fake_monitor(repo.path(), Some(coalesced));

        let mut conn = match attach(repo.path(), agent("codex")) {
            Attached::Shared(conn) => conn,
            other => panic!("expected Shared, got {other:?}"),
        };
        let got = conn
            .note(std::time::Instant::now() + Duration::from_secs(2))
            .expect("the note survived the handshake");
        served.join().unwrap();
        assert_eq!(got, note, "read exactly once, from the retained buffer");
    }

    /// Frames already delivered by a monitor that has since EXITED.
    ///
    /// The bytes are in the receive buffer and nothing is left to wait
    /// for, so a departed peer must cost the client nothing. Bounding
    /// the reads with `SO_RCVTIMEO` instead of `poll` fails here with
    /// `EINVAL` on a perfectly good five-second deadline: Darwin's
    /// `sosetopt` refuses every option once both directions are shut.
    /// That is the watchdog's own self-eviction — it would drop the
    /// wake it had just delivered. Every other fixture here keeps its
    /// peer alive, which is why none of them can see this.
    #[test]
    fn frames_from_a_peer_that_has_gone_are_still_read() {
        use std::io::Write;
        let (mine, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        // Non-blocking, as `connect_within` hands it back.
        mine.set_nonblocking(true).unwrap();
        theirs.write_all(b"first\nsecond\n").unwrap();
        drop(theirs);

        let mut reader = std::io::BufReader::new(mine);
        let mut partial = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        assert_eq!(
            read_frame(&mut reader, &mut partial, deadline).unwrap(),
            "first"
        );
        assert_eq!(
            read_frame(&mut reader, &mut partial, deadline).unwrap(),
            "second"
        );
        assert_eq!(
            read_frame(&mut reader, &mut partial, deadline)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::UnexpectedEof,
            "and the EOF after them is reported as one"
        );
    }

    /// A frame split across two short deadlines still arrives whole.
    ///
    /// `forward_beats` gives each read a short deadline so it can
    /// notice it has been asked to stop. The bytes it has already
    /// taken out of the socket cannot be read again, so losing them on
    /// that deadline leaves the next read starting mid-frame — and
    /// every decode after it fails, turning a healthy subscription
    /// into a permanent local watcher. A deadline may cost time, never
    /// data.
    #[test]
    fn a_frame_split_across_deadlines_is_not_lost() {
        use std::io::Write;
        let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        mine.set_nonblocking(true).unwrap();
        let mut conn = Conn::over(mine);

        let first = serde_json::to_string(&Note::Beat { fold: fold(7, 8) }).unwrap();
        let second = serde_json::to_string(&Note::Beat { fold: fold(9, 10) }).unwrap();
        let (head, tail) = first.split_at(first.len() / 2);

        // Half a frame, then a deadline passes with nothing more.
        (&mut &peer).write_all(head.as_bytes()).unwrap();
        let kind = conn
            .note(std::time::Instant::now() + Duration::from_millis(30))
            .map_err(|e| e.kind())
            .expect_err("half a frame is not a frame");
        assert_eq!(kind, std::io::ErrorKind::TimedOut);

        // The rest arrives, and so does another frame behind it.
        (&mut &peer)
            .write_all(format!("{tail}\n{second}\n").as_bytes())
            .unwrap();
        assert_eq!(
            conn.note(std::time::Instant::now() + Duration::from_secs(2))
                .expect("the frame resumed where it left off"),
            Note::Beat { fold: fold(7, 8) },
            "the prefix survived the deadline"
        );
        assert_eq!(
            conn.note(std::time::Instant::now() + Duration::from_secs(2))
                .expect("and the next frame is intact"),
            Note::Beat { fold: fold(9, 10) },
        );
    }

    /// A peer that never stops sending and never sends a newline.
    ///
    /// Every read is ready, so nothing ever waits — and the budget was
    /// enforced only where something waits. Bytes are buffered first
    /// and the peer then dropped, so no read in this test CAN block:
    /// a deadline honoured here is one honoured while making progress.
    /// Without that, the call runs to the EOF behind the junk and
    /// reports it, having bought the whole stream a place in `line`.
    #[test]
    fn the_deadline_binds_while_bytes_are_still_arriving() {
        use std::io::Write;
        let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        mine.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();

        let junk = vec![b'x'; 64 * 1024];
        let mut sent = 0usize;
        while let Ok(n) = (&mut &peer).write(&junk) {
            if n == 0 {
                break;
            }
            sent += n;
        }
        drop(peer);
        assert!(
            sent > 1024,
            "the fixture buffered an unterminated frame: {sent}"
        );

        let kind = read_frame(
            &mut std::io::BufReader::new(mine),
            &mut Vec::new(),
            std::time::Instant::now() + Duration::from_micros(1),
        )
        .map_err(|e| e.kind())
        .expect_err("an unterminated frame is not a frame");
        assert_eq!(
            kind,
            std::io::ErrorKind::TimedOut,
            "the budget must bind on a ready stream, not just a stalled one"
        );
    }

    /// A budget already spent writes nothing.
    ///
    /// `attach_by` shares ONE deadline across the hello and the
    /// welcome, so a connect that consumed it leaves this with nothing
    /// left. The socket here has room and would take the frame
    /// instantly, so no write can block: only a check on the PROGRESS
    /// path can stop it, and the one in `wait_ready` never gets a turn.
    #[test]
    fn a_spent_budget_writes_nothing() {
        use std::io::Read;
        let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        mine.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();

        let kind = write_frame(&mine, b"hello\n", std::time::Instant::now())
            .map_err(|e| e.kind())
            .expect_err("a spent budget is not a write");
        assert_eq!(kind, std::io::ErrorKind::TimedOut);

        let mut got = [0u8; 8];
        let read = (&mut &peer).read(&mut got).map_err(|e| e.kind());
        assert_eq!(
            read,
            Err(std::io::ErrorKind::WouldBlock),
            "nothing reached the peer"
        );
    }

    #[test]
    fn a_dead_monitors_socket_is_swept_and_a_live_one_is_not() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["11.sock", "22.sock", "33.sock", "notes.txt", "x.sock"] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        // 22 is live, 33 is ours, and the two non-pid names are not
        // this module's to judge.
        let swept = sweep_dead_sockets(dir.path(), 33, |pid| pid == 22);
        assert_eq!(swept, 1);
        let left: std::collections::BTreeSet<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            left,
            ["22.sock", "33.sock", "notes.txt", "x.sock"]
                .into_iter()
                .map(String::from)
                .collect()
        );
    }

    #[test]
    fn our_own_pid_is_live() {
        assert!(pid_is_live(std::process::id() as i32));
        assert!(!pid_is_live(0), "no process is pid 0");
        assert!(!pid_is_live(-1), "and -1 is every process, not one");
    }

    /// Binding clears the litter, which is the only place that can.
    #[test]
    fn binding_sweeps_what_dead_monitors_left() {
        let lease = acquire_lease(repo_dir().path()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("999999.sock"), b"").unwrap();
        let (_listener, path) = bind_listener(
            &[dir.path().to_path_buf()],
            std::process::id() as i32,
            unsafe { libc::getuid() },
            &lease,
        )
        .unwrap();
        assert!(path.is_file() || path.exists(), "ours is bound");
        assert!(
            !dir.path().join("999999.sock").exists(),
            "a dead monitor's socket is gone"
        );
    }

    /// A monitor listening but NOT yet announced, so a test can make
    /// it appear only once the client has asked for one to be started.
    fn unannounced_monitor(repo: &std::path::Path) -> (tempfile::TempDir, Descriptor) {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4343).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            // Answered streams are KEPT: dropping one closes the
            // connection the client is about to read its welcome from.
            let mut answered = Vec::new();
            while let Ok((mut stream, _)) = listener.accept() {
                let mut byte = [0u8; 1];
                while stream.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {}
                if stream.write_all(welcome_line().as_bytes()).is_err() {
                    return;
                }
                answered.push(stream);
            }
        });
        let d = Descriptor {
            proto: PROTO,
            pid: 4343,
            repo: repo.to_string_lossy().to_string(),
            socket: path.to_string_lossy().to_string(),
            fold: fold(1, 2),
        };
        (dir, d)
    }

    /// An agent client, for the tests whose subject is not identity.
    fn agent(author: &str) -> Who {
        Who::Agent {
            author: author.into(),
            holder: None,
            has_cli_sources: false,
        }
    }

    /// A spawn that must never be reached.
    fn forbidden(_: &std::path::Path) -> std::io::Result<Box<dyn Started>> {
        Err(std::io::Error::other("nothing should have been started"))
    }

    /// A started monitor that exits only when the test says so, and
    /// reports having been reaped.
    ///
    /// Models the case the retry loop could not cover: the monitor is
    /// still alive when the attach returns, so whatever reaps it has
    /// to outlive that return.
    struct ExitsLater {
        exit: std::sync::mpsc::Receiver<()>,
        reaped: std::sync::mpsc::Sender<()>,
    }

    impl Started for ExitsLater {
        fn reap(self: Box<Self>) {
            let _ = self.exit.recv();
            let _ = self.reaped.send(());
        }
    }

    /// Nothing to reap, for the tests whose subject is elsewhere.
    struct NotStarted;

    impl Started for NotStarted {
        fn reap(self: Box<Self>) {}
    }

    /// No monitor running: the client starts one and attaches to it.
    ///
    /// Connect-or-spawn's whole point — without it nothing ever starts
    /// a monitor, so every wait watches for itself and the plan buys
    /// nothing.
    #[tokio::test]
    async fn a_client_with_no_monitor_starts_one_and_attaches() {
        let repo = repo_dir();
        let (_dir, d) = unannounced_monitor(repo.path());
        let starts = std::sync::atomic::AtomicUsize::new(0);

        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            ATTACH_BUDGET,
            Duration::from_millis(200),
            SHARE_ATTEMPTS,
            |r| {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                publish(r, &d).map(|()| Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
        )
        .await;

        assert!(matches!(got, Attached::Shared(_)), "{got:?}");
        assert_eq!(
            starts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "ONE start, then retries against it: a second would only \
             lose the lease race to the first"
        );
    }

    /// A monitor that exits AFTER the attach returned is still reaped.
    ///
    /// The case ownership inside the retry loop cannot cover: two
    /// clients start at once, ours is still coming up when we attach
    /// to the winner and return, and ours then loses the lease and
    /// exits. Nothing in the loop is left to notice, so without
    /// supervision that outlives the return it is a zombie for the
    /// whole park — one per parked wait.
    #[tokio::test]
    async fn a_monitor_that_exits_after_the_attach_is_still_reaped() {
        let repo = repo_dir();
        let (_dir, d) = unannounced_monitor(repo.path());
        let (let_it_exit, exit) = std::sync::mpsc::channel();
        let (reaped, was_reaped) = std::sync::mpsc::channel();
        let held = std::sync::Mutex::new(Some(ExitsLater { exit, reaped }));

        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            ATTACH_BUDGET,
            Duration::from_millis(200),
            SHARE_ATTEMPTS,
            |r| {
                publish(r, &d)?;
                Ok(Box::new(held.lock().unwrap().take().expect("started once"))
                    as Box<dyn Started>)
            },
            connect_within,
        )
        .await;
        assert!(matches!(got, Attached::Shared(_)), "{got:?}");

        // Still alive at this point, so nothing can have reaped it yet.
        assert_eq!(
            was_reaped.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "a live monitor must NOT be waited out: it has to outlive this client"
        );

        // Now it exits — long after the helper returned.
        drop(let_it_exit);
        was_reaped
            .recv_timeout(Duration::from_secs(2))
            .expect("a monitor that exits later is still reaped");
    }

    /// A monitor already running is attached to, not duplicated.
    #[tokio::test]
    async fn a_client_that_finds_a_monitor_starts_nothing() {
        let repo = repo_dir();
        let (_dir, d) = unannounced_monitor(repo.path());
        publish(repo.path(), &d).unwrap();

        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            ATTACH_BUDGET,
            Duration::from_millis(200),
            SHARE_ATTEMPTS,
            forbidden,
            connect_within,
        )
        .await;
        assert!(matches!(got, Attached::Shared(_)), "{got:?}");
    }

    /// A start that fails is not fatal: the wait watches for itself.
    #[tokio::test]
    async fn a_start_that_fails_leaves_the_client_alone() {
        let repo = repo_dir();
        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            ATTACH_BUDGET,
            Duration::from_millis(200),
            SHARE_ATTEMPTS,
            |_| Err(std::io::Error::other("fork failed")),
            connect_within,
        )
        .await;
        match got {
            Attached::Alone(why) => {
                assert!(why.contains("no monitor published"), "{why}");
                assert!(
                    why.contains("fork failed"),
                    "and why starting failed: {why}"
                );
            }
            other => panic!("expected Alone, got {other:?}"),
        }
    }

    /// A monitor that never comes up bounds the attempts.
    ///
    /// A holder that is merely SUSPENDED keeps its lease, so retrying
    /// cannot take it — the client must stop and watch for itself
    /// rather than spend the park's budget asking again.
    #[tokio::test]
    async fn a_monitor_that_never_comes_up_bounds_the_attempts() {
        let repo = repo_dir();
        let dir = tempfile::tempdir().unwrap();
        // Published, but nothing is listening there: every attempt
        // reaches the connect and fails, so the attempts are counted.
        publish(
            repo.path(),
            &Descriptor {
                proto: PROTO,
                pid: 4344,
                repo: repo.path().to_string_lossy().to_string(),
                socket: socket_path(dir.path(), 4344)
                    .unwrap()
                    .to_string_lossy()
                    .to_string(),
                fold: fold(1, 2),
            },
        )
        .unwrap();
        let tries = std::sync::atomic::AtomicUsize::new(0);

        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            ATTACH_BUDGET,
            Duration::from_millis(80),
            SHARE_ATTEMPTS,
            |_| Ok(Box::new(NotStarted) as Box<dyn Started>),
            |path, deadline| {
                tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                connect_within(path, deadline)
            },
        )
        .await;

        assert!(matches!(got, Attached::Alone(_)), "{got:?}");
        assert_eq!(
            tries.load(std::sync::atomic::Ordering::SeqCst),
            SHARE_ATTEMPTS as usize,
            "bounded by the limit, not by the park's patience"
        );
    }

    /// A monitor that accepts and then says nothing must not strand the
    /// client: the budget is the whole handshake's.
    #[test]
    fn a_silent_monitor_falls_back_within_the_budget() {
        let repo = repo_dir();
        let (_dir, served) = fake_monitor(repo.path(), None);

        let budget = Duration::from_millis(150);
        let began = std::time::Instant::now();
        let got = attach_within(repo.path(), agent("codex"), budget);
        let took = began.elapsed();

        match got {
            Attached::Alone(why) => assert!(why.contains("handshake"), "{why}"),
            other => panic!("expected Alone, got {other:?}"),
        }
        // The peer stays open for 1500ms. Falling back anywhere near
        // the budget proves the DEADLINE ended it; waiting for the
        // peer would land an order of magnitude later.
        assert!(
            took < Duration::from_millis(800),
            "fell back in {took:?}, which is the peer closing and not the budget"
        );
        served.join().unwrap();
    }

    /// The descriptor says what the monitor WAS; the process answering
    /// now is what counts.
    #[test]
    fn a_welcome_on_another_proto_is_refused() {
        let repo = repo_dir();
        let skewed = format!(
            r#"{{"t":"ok","proto":{},"fold":{{"tick":1,"at_ms":2}}}}"#,
            PROTO + 1
        );
        let (_dir, served) = fake_monitor(repo.path(), Some(format!("{skewed}\n")));

        let got = attach(repo.path(), agent("codex"));
        served.join().unwrap();
        match got {
            Attached::Alone(why) => assert!(why.contains("answered on proto"), "{why}"),
            other => panic!("expected Alone, got {other:?}"),
        }
    }

    /// A connect that cannot finish inside the budget is a fallback,
    /// not a strand. A full backlog is platform-specific, so the
    /// connect is supplied instead of provoked.
    #[test]
    fn a_connect_that_cannot_finish_falls_back() {
        let repo = repo_dir();
        let (_dir, served) = fake_monitor(repo.path(), Some(welcome_line()));

        let budget = Duration::from_millis(150);
        let began = std::time::Instant::now();
        let got = attach_by(repo.path(), agent("codex"), budget, |_path, _deadline| {
            // Stands in for a connect against a full queue: waits
            // past any budget a test sets, then reports the
            // timeout. Deliberately does NOT call `remaining` — a
            // test must not depend on the function it covers for
            // its own termination, and an earlier version of this
            // spun forever under a mutation that made `remaining`
            // always return Some.
            std::thread::sleep(Duration::from_millis(400));
            Err(expired())
        });
        let took = began.elapsed();

        match got {
            Attached::Alone(why) => assert!(why.contains("cannot reach"), "{why}"),
            other => panic!("expected Alone, got {other:?}"),
        }
        assert!(took < Duration::from_millis(1500), "took {took:?}");
        drop(served);
    }

    /// A socket whose buffer is full, with a peer that never reads.
    ///
    /// The portable way to make a write block and a `POLLOUT` stay
    /// unready. A real backlog is platform-specific, which is why the
    /// connect itself takes a seam while its WAIT is tested for real.
    fn wedged_pair() -> (
        std::os::unix::net::UnixStream,
        std::os::unix::net::UnixStream,
    ) {
        use std::io::Write;
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        let junk = vec![0u8; 64 * 1024];
        loop {
            match (&mut &a).write(&junk) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        // Left non-blocking, which is what `connect_within` hands back.
        // `b` is returned unread on purpose: dropping it would turn a
        // blocked write into EPIPE and prove nothing about the bound.
        (a, b)
    }

    /// Bound the call itself, not just its result.
    ///
    /// If the deadline is ignored the call blocks for an hour, and an
    /// assertion after it never runs — the suite hangs instead of
    /// failing, which is how a real regression reaches CI unnoticed.
    /// Running it on a thread turns that into a clean failure.
    fn within<T: Send + 'static>(bound: Duration, work: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(bound)
            .expect("the deadline was not honoured: the call blocked instead")
    }

    #[test]
    fn waiting_for_readiness_gives_up_at_the_deadline() {
        let kind = within(Duration::from_millis(900), || {
            use std::os::fd::AsRawFd;
            let (full, _peer) = wedged_pair();
            wait_ready(
                full.as_raw_fd(),
                libc::POLLOUT,
                std::time::Instant::now() + Duration::from_millis(120),
            )
            .map_err(|e| e.kind())
        })
        .expect_err("a full buffer is not writable");
        assert_eq!(kind, std::io::ErrorKind::TimedOut);
    }

    /// A frame too big for the peer's buffer has to wait for drain,
    /// which a small frame to an attentive peer never exercises.
    #[test]
    fn a_write_that_cannot_drain_gives_up_at_the_deadline() {
        let kind = within(Duration::from_millis(900), || {
            let (full, _peer) = wedged_pair();
            write_frame(
                &full,
                &vec![b'x'; 256 * 1024],
                std::time::Instant::now() + Duration::from_millis(120),
            )
            .map_err(|e| e.kind())
        })
        .expect_err("a full buffer cannot take it");
        assert_eq!(kind, std::io::ErrorKind::TimedOut);
    }

    /// The clean half of `SO_ERROR`: a healthy socket reports nothing.
    /// Its error arm needs a full backlog and is documented as
    /// unexercised rather than covered.
    #[test]
    fn a_healthy_socket_reports_no_pending_error() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4242).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stream = std::os::unix::net::UnixStream::connect(&path).unwrap();

        assert_eq!(
            socket_error(stream.as_raw_fd()).expect("getsockopt works"),
            None
        );
    }

    /// A raw socket gets none of std's setup, so the protections it
    /// normally supplies are asserted on the real connect's result.
    /// A test pair made through std already has them and would mask
    /// the difference.
    #[test]
    fn a_connected_socket_carries_the_protections_std_would_have_given_it() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4242).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

        let stream = connect_within(&path, std::time::Instant::now() + Duration::from_secs(2))
            .expect("connects");
        let fd = stream.as_raw_fd();

        // SAFETY: `fd` is owned by `stream` for the rest of the test.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags >= 0, "F_GETFD failed");
        assert!(
            flags & libc::FD_CLOEXEC != 0,
            "not close-on-exec: a spawned command source would inherit this connection"
        );

        #[cfg(target_vendor = "apple")]
        {
            let mut on: libc::c_int = 0;
            let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: `on`/`len` outlive the call and match the option.
            let rc = unsafe {
                libc::getsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    &mut on as *mut libc::c_int as *mut libc::c_void,
                    &mut len,
                )
            };
            assert!(rc >= 0, "getsockopt failed");
            assert!(
                on != 0,
                "SO_NOSIGPIPE unset: with main's SIG_DFL, a dead monitor would kill the client"
            );
        }
    }

    const ROSTER: &str = r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#;

    fn repo_with_roster() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".clank")).unwrap();
        std::fs::write(d.path().join(".clank/config.json"), ROSTER).unwrap();
        d
    }

    fn hello_for(repo: &std::path::Path, author: &str) -> String {
        serde_json::to_string(&Hello {
            proto: PROTO,
            repo: repo.to_string_lossy().to_string(),
            who: agent(author),
        })
        .unwrap()
    }

    /// A hello that arrives AFTER the accept, in pieces.
    ///
    /// The production path used `into_std` and read synchronously.
    /// Tokio keeps `O_NONBLOCK` across that conversion and `read_frame`
    /// reads `WouldBlock` as the deadline, so a healthy client whose
    /// bytes had not landed yet was refused instantly (codex on
    /// 13e9fa7).
    #[tokio::test]
    async fn a_hello_arriving_late_and_in_fragments_is_admitted() {
        use tokio::io::AsyncWriteExt;

        let repo = repo_with_roster();
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4242).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let line = hello_for(repo.path(), "codex");
        let (first, second) = line.split_at(line.len() / 2);
        let first = first.to_string();
        let second = format!("{second}\n");
        tokio::spawn(async move {
            let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
            // Nothing at all for a moment, then half, then the rest.
            tokio::time::sleep(Duration::from_millis(60)).await;
            client.write_all(first.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;
            client.write_all(second.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let (stream, _) = listener.accept().await.unwrap();
        let client = greet(stream, repo.path())
            .await
            .expect("a fragmented hello is still a hello");
        assert_eq!(
            client.who,
            Served::Agent {
                author: AgentLabel::parse("codex").unwrap(),
                has_cli_sources: false,
            }
        );
    }

    /// One silent peer must not hold up anybody else.
    #[tokio::test]
    async fn a_silent_client_does_not_delay_a_healthy_one() {
        use tokio::io::AsyncWriteExt;

        let repo = repo_with_roster();
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4243).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        // Connects and says nothing, ever.
        let quiet_path = path.clone();
        let quiet = tokio::spawn(async move {
            let _s = tokio::net::UnixStream::connect(&quiet_path).await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let (silent, _) = listener.accept().await.unwrap();
        let repo_for_task = repo.path().to_path_buf();
        // As `start` does: the handshake goes off the accept loop.
        let stalled = tokio::spawn(async move { greet(silent, &repo_for_task).await });

        let line = format!("{}\n", hello_for(repo.path(), "claude"));
        tokio::spawn(async move {
            let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
            c.write_all(line.as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let began = std::time::Instant::now();
        let (healthy, _) = listener.accept().await.unwrap();
        let admitted = greet(healthy, repo.path()).await.expect("admitted");
        let took = began.elapsed();

        assert_eq!(admitted.who.name(), "claude");
        assert!(
            took < Duration::from_secs(1),
            "the healthy client waited {took:?} behind a silent one"
        );
        assert!(!stalled.is_finished(), "the silent one is still pending");
        stalled.abort();
        quiet.abort();
    }

    /// An author the roster does not know is declined BEFORE admission:
    /// welcoming one would register a connection nothing can serve.
    #[tokio::test]
    async fn an_author_off_the_roster_is_declined_not_registered() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let repo = repo_with_roster();
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4244).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let line = format!("{}\n", hello_for(repo.path(), "ghost"));
        let heard = tokio::spawn(async move {
            let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
            c.write_all(line.as_bytes()).await.unwrap();
            let mut answer = String::new();
            tokio::io::BufReader::new(c)
                .read_line(&mut answer)
                .await
                .unwrap();
            answer
        });

        let (stream, _) = listener.accept().await.unwrap();
        let refused = greet(stream, repo.path()).await;
        assert!(refused.is_err(), "`ghost` must not be registered");

        // And it is told why, rather than left hanging.
        let answer = heard.await.unwrap();
        let welcome: Welcome = serde_json::from_str(answer.trim_end()).unwrap();
        match welcome {
            Welcome::Decline { why, .. } => assert!(why.contains("roster"), "{why}"),
            other => panic!("expected a decline, got {other:?}"),
        }
    }

    /// `wait` stops a waiter whose author left the roster, and treats
    /// everything else as transient. The monitor must agree.
    #[test]
    fn only_a_missing_roster_entry_counts_as_revocation() {
        let repo = repo_with_roster();
        let gone =
            crate::agent_store::resolve_role(repo.path(), &AgentLabel::parse("ghost").unwrap())
                .expect_err("not on the roster");
        assert!(revoked(&gone), "a removed author is revocation");

        let other = anyhow::anyhow!("the config could not be read");
        assert!(!revoked(&other), "an unreadable config is transient");
    }

    /// A fold that fails may not publish a fresh completed-fold stamp.
    ///
    /// The outcome is supplied because the rebuild is fail-soft — it
    /// returns `Ok` for a repo with no git, a broken `.git` file and an
    /// empty `.git` directory, all three probed. The ordering is the
    /// invariant, not the rebuild's own failure modes.
    #[test]
    fn a_failed_fold_does_not_advertise_progress() {
        let repo = repo_with_roster();
        let socket = std::path::Path::new("/tmp/clank-socks-501/4242.sock");
        let at_zero = Fold { tick: 0, at_ms: 1 };
        publish(repo.path(), &describe(repo.path(), 4242, socket, at_zero)).unwrap();

        let heartbeat = std::sync::atomic::AtomicU64::new(0);
        let beat = || heartbeat.load(std::sync::atomic::Ordering::Acquire);

        let failed = advance_with(
            repo.path(),
            4242,
            socket,
            at_zero,
            &heartbeat,
            Err(anyhow::anyhow!("the fold did not complete")),
        );
        assert!(failed.is_none(), "a failed fold yields nothing to serve");
        assert_eq!(
            read_published(repo.path()).map(|d| d.fold.tick),
            Some(0),
            "and the stamp still claims only the fold that completed"
        );
        assert_eq!(
            beat(),
            0,
            "nor may the watchdog's heartbeat move: a failing fold that \
             kept it fresh is the wedge the watchdog cannot then see"
        );

        // Repeated failure must not creep the stamp either.
        for _ in 0..3 {
            let _ = advance_with(
                repo.path(),
                4242,
                socket,
                at_zero,
                &heartbeat,
                Err(anyhow::anyhow!("still failing")),
            );
        }
        assert_eq!(read_published(repo.path()).map(|d| d.fold.tick), Some(0));
        assert_eq!(beat(), 0, "repeated failure must not creep it either");

        // And a fold that DOES complete advances and publishes.
        let ok = advance_with(
            repo.path(),
            4242,
            socket,
            at_zero,
            &heartbeat,
            Ok(crate::repo_state::RepoState::empty(
                repo.path().to_path_buf(),
            )),
        )
        .expect("a completed fold serves");
        assert_eq!(ok.0.tick, 1);
        assert_eq!(read_published(repo.path()).map(|d| d.fold.tick), Some(1));
        assert_eq!(beat(), 1, "and the watchdog sees the fold that completed");
    }

    /// Fill a stream so a later `write_all` on it cannot drain, and
    /// report how much went in.
    ///
    /// Filled through STD, not tokio: `tokio::try_write` reports
    /// `WouldBlock` until readiness has been registered, so a fill loop
    /// over a fresh tokio stream exits having written nothing and every
    /// client drains happily. And filled from the WRITER's side — the
    /// writer task writes mine→peer, so filling the peer's send buffer
    /// would block nothing. Both mistakes were made before this comment
    /// existed.
    fn wedge(stream: &std::os::unix::net::UnixStream) -> usize {
        use std::io::Write;
        stream.set_nonblocking(true).unwrap();
        let junk = vec![0u8; 32 * 1024];
        let mut total = 0;
        loop {
            match (&mut &*stream).write(&junk) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(_) => break,
            }
        }
        stream.set_nonblocking(false).unwrap();
        total
    }

    /// Clients as the owner holds them. Peers are returned so a queue
    /// is not closed by a dropped socket, and are never read — a
    /// wedged one stays wedged.
    fn registry(authors: &[(&str, bool)]) -> (Vec<Client>, Vec<std::os::unix::net::UnixStream>) {
        let mut clients = Vec::new();
        let mut peers = Vec::new();
        for (author, wedged) in authors {
            let (mine, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
            if *wedged {
                assert!(wedge(&mine) > 0, "the wedge wrote nothing");
            }
            mine.set_nonblocking(true).unwrap();
            let mine = tokio::net::UnixStream::from_std(mine).unwrap();
            let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
            let label = AgentLabel::parse(author).unwrap();
            tokio::spawn(write_to(mine, queued, label.to_string()));
            clients.push(Client {
                who: Served::Agent {
                    author: label,
                    has_cli_sources: false,
                },
                out,
            });
            peers.push(theirs);
        }
        (clients, peers)
    }

    /// A writer that cannot drain gives up, closing its socket.
    ///
    /// Ignoring the timeout and looping instead leaves the task and its
    /// fd alive forever. The owner still retires the client, so the
    /// registry looks correct while a task and a socket leak — the
    /// accumulation this whole plan is about, in a new place.
    #[tokio::test]
    async fn a_writer_that_cannot_drain_gives_up_and_closes() {
        let (mine, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(wedge(&mine) > 0, "the wedge wrote nothing");
        mine.set_nonblocking(true).unwrap();
        let mine = tokio::net::UnixStream::from_std(mine).unwrap();

        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let task = tokio::spawn(write_to_within(
            mine,
            queued,
            "codex".to_string(),
            Duration::from_millis(80),
        ));
        out.try_send("one\n".to_string()).unwrap();

        // It must END, not retry forever.
        let ended = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(
            ended.is_ok(),
            "the writer is still trying; its socket will never close"
        );
    }

    /// And a writer whose peer has GONE gives up too.
    ///
    /// A separate path from the stall: this one fails rather than
    /// hangs, so a timeout never fires and ignoring the error would
    /// loop on a dead socket forever.
    #[tokio::test]
    async fn a_writer_whose_peer_closed_gives_up() {
        let (mine, peer) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(peer);
        mine.set_nonblocking(true).unwrap();
        let mine = tokio::net::UnixStream::from_std(mine).unwrap();

        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let task = tokio::spawn(write_to_within(
            mine,
            queued,
            "codex".to_string(),
            // Long on purpose: only an ERROR can end this, not the
            // budget.
            Duration::from_secs(30),
        ));
        // Enough to be certain of EPIPE rather than a short write.
        for _ in 0..CLIENT_QUEUE {
            let _ = out.try_send("x".repeat(4096));
        }

        let ended = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(
            ended.is_ok(),
            "the writer is still trying to reach a closed peer"
        );
    }

    /// The heartbeat is not postponed by client traffic.
    ///
    /// Rebuilding `Beat::next` inside the owner's select restarted its
    /// sleep on every accept or arrival, so traffic faster than the
    /// tick postponed the heartbeat forever and a newly admitted
    /// client never got its first items. A dedicated producer cannot
    /// be reset by anything the owner does (codex on 24ea491).
    #[tokio::test(start_paused = true)]
    async fn a_heartbeat_arrives_whatever_the_owner_is_doing() {
        let (_fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
        let mut beats = beats(fs_rx, FOLD_TICK);

        // Stand in for the owner being busy with clients: time moves,
        // nothing is received, and the producer is never polled by the
        // select that used to cancel it.
        for _ in 0..10 {
            tokio::time::advance(FOLD_TICK / 4).await;
        }

        let beat = tokio::time::timeout(FOLD_TICK * 2, beats.recv()).await;
        assert!(
            matches!(beat, Ok(Some(()))),
            "no heartbeat: client traffic postponed it"
        );
    }

    /// A filesystem wake survives the owner being busy.
    ///
    /// The old shape could consume the wake and then be cancelled
    /// during its 200ms debounce — the channel item gone, the beat
    /// never delivered. A dropped repo change is a silent agent.
    ///
    /// REAL time, not paused: `Beat::bridge` forwards from the std
    /// channel on an OS thread, and paused time does not wait for
    /// thread scheduling. Pausing made this pass alone and fail under
    /// the full suite's load, which is the worse of the two failures.
    #[tokio::test]
    async fn a_filesystem_wake_is_not_lost_while_the_owner_is_busy() {
        let (fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
        // An hour's heartbeat, so a beat can only be the wake.
        let mut beats = beats(fs_rx, Duration::from_secs(3600));

        fs_tx.send(()).unwrap();

        let beat = tokio::time::timeout(Duration::from_secs(5), beats.recv()).await;
        assert!(
            matches!(beat, Ok(Some(()))),
            "the wake was consumed and then discarded"
        );
    }

    /// A burst is one refold, and the debounce still belongs to `wait`.
    #[tokio::test]
    async fn a_burst_of_changes_is_one_beat() {
        let (fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
        let mut beats = beats(fs_rx, Duration::from_secs(3600));

        for _ in 0..20 {
            fs_tx.send(()).unwrap();
        }

        assert!(
            matches!(
                tokio::time::timeout(Duration::from_secs(5), beats.recv()).await,
                Ok(Some(()))
            ),
            "the burst produced no beat"
        );
        // A second would mean the burst was not coalesced, and the
        // heartbeat cannot supply one for an hour.
        assert!(
            tokio::time::timeout(Duration::from_millis(600), beats.recv())
                .await
                .is_err(),
            "the burst produced more than one refold"
        );
    }

    /// Beats COALESCE for a busy owner rather than queueing.
    ///
    /// A deep queue would let a busy owner accumulate a backlog of
    /// folds to grind through afterwards, each one already superseded.
    /// Capacity 1 makes the producer wait instead, so the owner does
    /// the current fold and not twenty stale ones.
    #[tokio::test(start_paused = true)]
    async fn beats_coalesce_for_a_busy_owner() {
        let tick = Duration::from_millis(100);
        let (_fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
        let mut beats = beats(fs_rx, tick);

        // Twenty heartbeats' worth of time, nothing received.
        for _ in 0..20 {
            tokio::time::advance(tick).await;
        }

        let mut drained = 0;
        while tokio::time::timeout(Duration::from_millis(1), beats.recv())
            .await
            .is_ok()
        {
            drained += 1;
            if drained > 5 {
                break;
            }
        }
        assert!(
            drained <= 2,
            "{drained} beats queued up: a busy owner would fold that many times over"
        );
    }

    /// The owner must not wait on anybody's socket.
    ///
    /// A timeout inside a sequential loop bounds one write, not the
    /// loop: unread clients used to hold the owner for a budget each,
    /// so it could not accept, drain arrivals or tick — long enough
    /// for a healthy newcomer to exhaust its attach budget unaccepted
    /// (codex on 38088ac). The owner enqueues now.
    #[tokio::test]
    async fn stalled_readers_do_not_hold_the_fold_owner() {
        let repo = repo_with_roster();
        let (mut clients, peers) = registry(&[("codex", true), ("codex", true), ("claude", false)]);

        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let mut fold = Fold { tick: 0, at_ms: 0 };

        // More ticks than the queue holds, so a reader that cannot keep
        // up is retired instead of buffered forever.
        let began = std::time::Instant::now();
        for _ in 0..(CLIENT_QUEUE + 2) {
            fold = Fold {
                tick: fold.tick + 1,
                at_ms: now_ms(),
            };
            serve(repo.path(), &mut clients, fold, &state);
            // Let the writer tasks run, as the owner's select would.
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let took = began.elapsed();

        // The sleeps are the only time this may spend: the owner's own
        // work is enqueueing, which never touches a socket. Awaiting
        // the two wedged writes would have cost 2 x WRITE_BUDGET per
        // tick.
        assert!(
            took < Duration::from_millis(600),
            "the owner spent {took:?} — it is waiting on client sockets"
        );
        assert!(
            clients.iter().any(|c| c.who.name() == "claude"),
            "the healthy client is still registered: {:?}",
            clients.iter().map(|c| c.who.name()).collect::<Vec<_>>()
        );
        assert!(
            clients.len() < 3,
            "a reader that cannot keep up is retired: {:?}",
            clients.iter().map(|c| c.who.name()).collect::<Vec<_>>()
        );
        // And the healthy client really was delivered to.
        peers[2]
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let mut got = [0u8; 1];
        use std::io::Read;
        assert!(
            (&mut &peers[2]).read(&mut got).is_ok(),
            "the healthy client has a frame waiting"
        );
    }

    /// A peer that says nothing is given up on at the budget, not kept
    /// forever: the monitor would otherwise accumulate half-open
    /// registrations nobody can serve.
    #[tokio::test]
    async fn a_hello_that_never_arrives_is_given_up_on() {
        let repo = repo_with_roster();
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4245).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();

        let held = tokio::spawn(async move {
            let _s = tokio::net::UnixStream::connect(&path).await.unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let (stream, _) = listener.accept().await.unwrap();
        let began = std::time::Instant::now();
        let why = greet_within(stream, repo.path(), Duration::from_millis(120))
            .await
            .expect_err("a client that never speaks is not admitted")
            .to_string();
        let took = began.elapsed();

        assert!(why.contains("did not finish its hello"), "{why}");
        assert!(
            took < Duration::from_secs(1),
            "waited {took:?} — the peer stays open, so this is the budget or nothing"
        );
        held.abort();
    }

    /// An author removed from the roster has its connection retired.
    ///
    /// Keeping it would hold a socket open forever with no beats — and
    /// because that branch never writes, the monitor could not even
    /// notice the client had gone (codex on 13e9fa7).
    #[tokio::test]
    async fn a_client_whose_author_left_the_roster_is_dropped() {
        let repo = repo_with_roster();
        let (mut clients, _keep) = registry(&[("codex", false)]);
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let fold = Fold { tick: 1, at_ms: 2 };

        serve(repo.path(), &mut clients, fold, &state);
        assert_eq!(clients.len(), 1, "a registered author is served");

        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        serve(repo.path(), &mut clients, fold, &state);
        assert!(
            clients.is_empty(),
            "a revoked author must not keep a connection"
        );
    }

    /// An observer needs no roster entry; an agent still does.
    ///
    /// A panel is not an agent. Requiring an identity would have meant
    /// inventing a roster entry for `clank tui`, and refusing one
    /// would have meant the panel keeps its own watcher forever —
    /// which is the duplication this plan is here to delete.
    ///
    /// Through `greet`, because the roster check lives in `admit` and
    /// not in `accept_hello`: asserting on the parsed frame proved
    /// only that the frame parses, which the mutation check caught.
    #[tokio::test]
    async fn an_observer_is_admitted_without_being_on_the_roster() {
        async fn admitted(repo: &std::path::Path, who: Who, pid: i32) -> anyhow::Result<Client> {
            use tokio::io::AsyncWriteExt;
            let dir = tempfile::tempdir().unwrap();
            let path = socket_path(dir.path(), pid).unwrap();
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let line = serde_json::to_string(&Hello {
                proto: PROTO,
                repo: repo.to_string_lossy().to_string(),
                who,
            })
            .unwrap();
            tokio::spawn(async move {
                let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
                client
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(200)).await;
            });
            let (stream, _) = listener.accept().await.unwrap();
            greet(stream, repo).await
        }

        let repo = repo_with_roster();
        let client = admitted(repo.path(), Who::Observer, 4401)
            .await
            .expect("an observer is welcome");
        assert_eq!(client.who, Served::Observer);

        // `nobody` is emphatically not on that roster, and the same
        // handshake must still refuse it: having no identity is not
        // the same as claiming one nobody knows.
        let why = admitted(repo.path(), agent("nobody"), 4402)
            .await
            .expect_err("an unknown agent is not admitted")
            .to_string();
        assert!(why.contains("not on this repo's roster"), "{why}");
    }

    /// An observer is told the fold MOVED and never what anyone owes.
    ///
    /// Items are a projection of one author's work; an observer has no
    /// author, and sending it somebody's items would leak one agent's
    /// work to a panel — besides costing the refold the plan deletes.
    #[tokio::test]
    async fn an_observer_gets_beats_and_never_items() {
        let repo = repo_with_roster();
        let (out, mut queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            who: Served::Observer,
            out,
        }];
        // A repo with work to do: a queued plan is something an AGENT
        // would be handed, so an observer seeing a Beat here is the
        // routing and not an empty repo.
        std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
        std::fs::write(
            repo.path().join(".clank/queue/next.md"),
            "# next

something to promote
",
        )
        .unwrap();
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let fold = Fold { tick: 9, at_ms: 2 };

        serve(repo.path(), &mut clients, fold, &state);
        assert_eq!(clients.len(), 1, "an observer stays registered");
        let line = queued.recv().await.expect("it was sent something");
        assert_eq!(
            serde_json::from_str::<Note>(line.trim_end()).unwrap(),
            Note::Beat { fold },
            "the fold moved, and that is all an observer is told"
        );
    }

    /// With no monitor, a subscriber watches the repo itself — the
    /// panel must not go blind just because nothing is running.
    #[test]
    fn a_subscriber_with_no_monitor_watches_the_repo_itself() {
        let repo = repo_with_roster();
        // A real git repo, because the watcher watches the gitdir too.
        // Test code may spawn `git` — the boundary gate exempts it.
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .args(["init", "--quiet"])
            .status()
            .expect("git");
        assert!(status.success());
        let (tx, rx) = std::sync::mpsc::channel();
        let wakes = core_wakes(repo.path(), tx).expect("a watcher of our own");

        std::fs::write(repo.path().join(".clank/config.json"), ROSTER).unwrap();
        rx.recv_timeout(Duration::from_secs(5))
            .expect("a change in the repo still wakes it");
        drop(wakes);
    }

    /// The handoff reconciles the gap, and only when it covered it.
    #[test]
    fn the_handoff_wakes_the_consumer_once_it_is_watching() {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(reconcile_after(&tx, Ok(())).is_some());
        assert_eq!(
            rx.try_recv(),
            Ok(()),
            "a change in the gap is baselined by the new watcher, not \
             reported, so the handoff has to say so itself"
        );
        assert_eq!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "once, not a stream of them"
        );

        let failed: anyhow::Result<()> = Err(anyhow::anyhow!("no watcher"));
        assert!(reconcile_after(&tx, failed).is_none());
        assert_eq!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty),
            "and with nothing watching, a wake would promise a freshness \
             that is not coming"
        );
    }

    /// A client that cannot share must still be woken, by itself.
    ///
    /// The uncovered case is a suspended monitor holding the flock:
    /// nothing evicts it and spawning cannot take the lease, so the
    /// only safe end is to stop sharing. Silence is the one outcome
    /// this plan may not produce.
    #[test]
    fn a_client_that_cannot_share_falls_back_rather_than_waiting() {
        assert!(!give_up_sharing(0, 3));
        assert!(!give_up_sharing(2, 3));
        assert!(give_up_sharing(3, 3), "bounded, not endless");
        assert!(give_up_sharing(9, 3));
    }

    /// Drive the watchdog over a SEQUENCE of ticks and say when it
    /// stood down, which is the only thing worth asserting about it.
    fn stood_down_after(ticks: &[u64], limit: u32) -> Option<usize> {
        let mut seen = ticks[0];
        let mut missed = 0;
        for (i, now) in ticks.iter().enumerate().skip(1) {
            match watchdog_look(seen, *now, missed, limit) {
                Watch::Keep { tick, missed: m } => {
                    seen = tick;
                    missed = m;
                }
                Watch::StandDown => return Some(i),
            }
        }
        None
    }

    #[test]
    fn a_monitor_stands_down_only_when_its_fold_has_stopped() {
        assert_eq!(
            stood_down_after(&[7, 8, 9, 10, 11, 12], 4),
            None,
            "a fold that keeps advancing never stands down"
        );
        assert_eq!(
            stood_down_after(&[7, 7, 7], 4),
            None,
            "three still looks is not yet the limit"
        );
        assert_eq!(
            stood_down_after(&[7, 7, 7, 7, 7], 4),
            Some(4),
            "four in a row is the wedge"
        );
        // The part a predicate taking `missed` cannot state: the count
        // RESETS, so a slow fold is not a dying one.
        assert_eq!(
            stood_down_after(&[7, 7, 7, 8, 8, 8, 9], 4),
            None,
            "a fold that advances late clears what it had missed"
        );
        assert_eq!(
            stood_down_after(&[7, 7, 7, 8, 8, 8, 8, 8], 4),
            Some(7),
            "and stalling after that still ends it"
        );
    }

    /// The frames must survive the wire, since a client that cannot
    /// read a note has no way to tell that from having no work.
    #[test]
    fn every_frame_round_trips() {
        let hello = Hello {
            proto: PROTO,
            repo: "/w/proj".into(),
            who: Who::Agent {
                author: "codex".into(),
                holder: Some(Holder {
                    tool: clank_core::vocab::Tool::Codex,
                    session: "s-1".into(),
                }),
                has_cli_sources: true,
            },
        };
        let back: Hello = serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
        assert_eq!(back, hello);

        for note in [
            Note::Beat { fold: fold(1, 2) },
            Note::Items {
                items: vec![WaitItem::PromoteFromQueue {
                    name: "next".into(),
                    priority: 500,
                }],
                fold: fold(3, 4),
            },
        ] {
            let back: Note = serde_json::from_str(&serde_json::to_string(&note).unwrap()).unwrap();
            assert_eq!(back, note);
        }

        for welcome in [
            Welcome::Ok {
                proto: PROTO,
                fold: fold(5, 6),
            },
            Welcome::Decline {
                proto: PROTO,
                why: "proto 2".into(),
            },
        ] {
            let back: Welcome =
                serde_json::from_str(&serde_json::to_string(&welcome).unwrap()).unwrap();
            assert_eq!(back, welcome);
        }
    }
}
