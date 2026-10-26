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
// Version 7 also makes single-tag diagnostics scalar.
// Version 8 hosts native timers and presents their additive event variant.
// Old monitors must not keep serving the previous lifecycle after an upgrade.
// Version 10 owns eligible-event interruption even without an armed waiter.
// Version 9 can host the same inbox but cannot promise this control lifecycle;
// a fresh client must negotiate retirement rather than silently reuse it.
// 11 admits one automatic Grok wait per bound session and reports refusals.
// 12 publishes its sources' lifecycle and takes a control request to
// restart one.
// 15 no longer carries PR-review items, which an older monitor can still
// send and this build cannot decode.
// 16 owns turn-end policy: an agent's hello may carry its hook's report,
// answered with verdicts and then one delivery result on that connection,
// and control requests attend and clear.
// 17 carries the exact answer an `unblocked` item delivers and receives it
// on delivery; a 16 monitor sends neither and still teaches `block clean`.
// 18 parks an attending agent's turn-end instead of silencing it; a 17
// monitor would leave it deaf to new work until its attendance ends.
// 19 ends a parked Stop with the `user_command` silent reason when a signed
// command waits, which an 18 hook cannot decode; an 18 monitor never
// releases a park for one.
pub(crate) const PROTO: u32 = 19;

#[cfg(test)]
#[path = "monitor_grok_tests.rs"]
mod grok_tests;

#[cfg(test)]
#[path = "monitor_turn_tests.rs"]
mod turn_tests;

/// What a monitor can do, not just what its frames look like.
///
/// 4 is the HOSTING contract: this monitor runs an author's config
/// `wait_events` and routes their rows back, so a wait that has such a
/// source may stop running it. 3 is the monitor that does not — and
/// one of those can still be RUNNING after an ordinary binary update,
/// holding its repo's lease. It would accept the same hello, serve
/// only projected repo work, and the client would have stopped
/// tailing its own sources: configured events missed for as long as
/// that monitor lives (codex on a135276).
///
/// 5 adds the STAND-DOWN: a holder on 5 can be ASKED to retire by a
/// successor, over the socket, and answers. Anything older cannot be
/// asked at all — proto 3 and 4 both exit only on their own fold
/// watchdog — so "the live peer answered with a number below ours" is
/// the positive evidence that a cooperative retirement is unavailable,
/// and it is the ONLY thing that may be read that way. The bump is
/// forced rather than chosen: proto-4 monitors were already running
/// when this was written, so putting the capability on 4 would have
/// made `proto: 4` ambiguous between a peer that can be asked and one
/// that cannot, which is exactly the inference the bootstrap must not
/// make (codex on b1d5d27).
///
/// So a capability change is a version change. The client refuses a
/// monitor on any other number and watches for itself, which is the
/// behaviour it had before this plan rather than a failure.
///
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
        /// The hook's report this wait carries. The monitor answers it
        /// with verdicts instead of items, and the connection's next
        /// frame is the delivery result. `None` for a plain wait.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_end: Option<super::turn_wire::Report>,
    },
    /// A read-only fold subscription. Any number, no identity, and
    /// never any items.
    Observer,
    /// A harness's recovery helper: waits for the monitor's decision to
    /// retry a failed turn and never takes inbox work, so its presence is
    /// not evidence the agent is waiting for any (`recovery.rs`).
    RecoveryHelper { author: String, holder: Holder },
    /// One request, answered with one note, after which the connection
    /// closes. Never a reader.
    Control { request: Control },
}

/// What a control client asks of the monitor.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub(crate) enum Control {
    /// Restart one of `author`'s command sources, if what the monitor
    /// runs for it is still the configuration `accepted` fingerprints.
    Restart {
        author: String,
        key: String,
        accepted: String,
    },
    /// Attend background work from `author`'s next turn-end.
    Attend {
        author: String,
        session: clank_core::SessionId,
        selector: clank_core::turn_policy::Selector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        desc: Option<String>,
        expect_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<clank_core::turn_policy::ShellProof>,
    },
    /// Withdraw `author`'s attendance, and the check-in it owes.
    Clear {
        author: String,
        session: clank_core::SessionId,
    },
    /// A turn `author`'s harness reported for its own `session`, for
    /// recovery to read. Answered with how a retry would reach it.
    Observe {
        author: String,
        tool: clank_core::Tool,
        session: String,
        turn: serde_json::Value,
    },
    /// A recovery helper's answer to one of `author`'s attempts.
    RecoveryReply {
        author: String,
        tool: clank_core::Tool,
        reply: super::recovery::Reply,
    },
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
    Ok { proto: u32 },
    Decline { proto: u32, why: String },
}

/// What the monitor pushes to a client.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum Note {
    /// A lifecycle or admission refusal. It must never fall back to a local
    /// watcher, which would bypass the monitor's ownership decision.
    Refused { why: String },
    /// A newer wait of the same agent session was admitted, and this one
    /// is retired: it ends silently, without watching for itself.
    Superseded,
    /// What this client's author is owed. Empty is never sent; a client
    /// blocks until it has something, as a park does today.
    Items {
        items: Vec<WaitItem>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_cwd: Option<std::path::PathBuf>,
    },
    /// A recovery helper's one authorized attempt: submit `wake` and
    /// answer with a `Control::RecoveryReply`.
    Recover {
        /// With `n`, which attempt the helper's answer is for.
        opened_by: String,
        n: u32,
        tag: String,
        wake: String,
    },
    /// A control request was carried out.
    Done { what: String },
    /// A turn-end verdict, for a connection carrying a report.
    Verdict(super::turn_wire::Frame),
    /// Proof of life, and nothing else.
    ///
    /// It carried the monitor's fold counter so a client could tell a
    /// WEDGED monitor from a quiet one. Two separate signals already
    /// answer that, and neither needs the number:
    ///
    /// - A CLIENT watches arrival. It gives up on silence past
    ///   `BEAT_GRACE` and goes back to watching for itself, which is
    ///   the same answer for a wedged monitor and a dead one — and
    ///   both are the answer it can act on.
    /// - The WATCHDOG, inside the monitor, compares successive reads
    ///   of a private counter that only a COMPLETED fold advances.
    ///   That is the one place two of these were ever compared.
    ///
    /// A beat does not stand for a fresh fold, and the first version
    /// of this comment said it did (codex on 258d979): `admit_into`
    /// and the author-source branch both serve against the CACHED
    /// folded state, so beats and counter advances are not one for
    /// one.
    Beat,
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
/// It lives OUTSIDE the repo, in this uid's [`runtime_dir`] keyed by
/// [`repo_tag`], because it means nothing once its monitor is gone. As
/// a plain file it is bound by PATH_MAX rather than `sun_path`, so
/// neither the repo's depth nor the namespace's costs anything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Descriptor {
    pub(crate) proto: u32,
    pub(crate) pid: i32,
    /// The canonicalized repo, so a descriptor left behind by a repo
    /// since moved is recognised rather than trusted.
    pub(crate) repo: String,
    pub(crate) socket: String,
    /// The build serving, fixed for the monitor's life. What it has since
    /// seen of the installation is in its own observation record, never
    /// here: this file is routing, and a stale monitor's late write must
    /// not be able to change where clients connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) build: Option<DescribedBuild>,
}

/// A monitor's build, as its descriptor names it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct DescribedBuild {
    pub(crate) running: String,
    pub(crate) installed_at: String,
    pub(crate) source: crate::build_stamp::Source,
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
/// adb5453).
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

/// Whether a bounded retirement is under way, as the watchdog sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retiring {
    No,
    Within,
    /// Past its deadline and the margin after it.
    Overdue,
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
///
/// A monitor retiring has stopped folding on purpose, so while its
/// retirement is within its deadline the fold's age is not judged; past
/// it, the monitor is forced out like a stalled one.
pub(crate) fn watchdog_look(
    seen: u64,
    now: u64,
    missed: u32,
    limit: u32,
    retiring: Retiring,
) -> Watch {
    match retiring {
        Retiring::No => {}
        Retiring::Within => {
            return Watch::Keep {
                tick: now,
                missed: 0,
            };
        }
        Retiring::Overdue => return Watch::StandDown,
    }
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

/// What a READ-ONLY look at a runtime namespace established.
///
/// Separate from [`ensure_dir`] because discovery must not create. A
/// client asking where a monitor is has no business making a directory,
/// and requiring write access to find out would make a read-only repo
/// undiscoverable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Trust {
    /// Ours, and the same ownership/mode policy a mutation demands.
    Ours,
    /// Not there. An ordinary absence: nothing is published.
    Absent,
    /// There and NOT ours. The string says why.
    Theirs(String),
}

/// Whether a runtime namespace is ours to trust, creating nothing.
///
/// The read half of the boundary, and the half this plan nearly
/// shipped without. Moving the descriptor out of a repo we own and
/// into a PREDICTABLE path under world-writable `/tmp` means another
/// uid can get there first: create `/tmp/clank-run-<our-uid>`, leave a
/// descriptor naming the right repo and proto, and point `socket` at
/// theirs. Matching fields are not evidence of who wrote them. Our
/// client would then read it and connect before any mutation guard ran
/// — refusing to PUBLISH into a planted namespace does nothing about
/// consuming what is already in it (codex on 9dfc3e3).
///
/// Vetted with `vet_dir`, so it is the one policy and not a second
/// opinion: `symlink_metadata` refuses an indirection, and for a real
/// directory we own `/tmp`'s sticky bit stops anyone else replacing it
/// between this answer and the read that follows.
pub(crate) fn trusted_dir(dir: &std::path::Path, me: u32) -> Trust {
    match std::fs::symlink_metadata(dir) {
        Ok(md) => match vet_dir(dir, &md, me) {
            Ok(()) => Trust::Ours,
            Err(why) => Trust::Theirs(why),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Trust::Absent,
        // Not an absence: something is there that cannot be vetted,
        // and reading through it is the thing being prevented.
        Err(e) => Trust::Theirs(format!("cannot stat {}: {e}", dir.display())),
    }
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
pub(crate) fn restrict(path: &std::path::Path) -> Result<(), String> {
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
        sweep_dead_sockets(dir, pid, crate::signal::alive);
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

/// What an opening frame turns out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Greeting {
    Client(Hello),
    /// Declined, and the decline says this monitor is OBSOLETE: the
    /// client speaks a NEWER version than it does.
    ///
    /// Which means it should go, and it is the only thing entitled to
    /// decide that — it is acting on its own process, in its own repo,
    /// so there is no identity to establish, no signal to aim and no
    /// permission to negotiate. All of that belonged to an earlier
    /// design where a STRANGER ended the holder, and all of it is
    /// deleted.
    Obsolete(Welcome),
    Decline(Welcome),
}

/// The compatibility FLOOR: what a monitor may rely on reading from
/// ANY version's opening frame, past or future.
///
/// Separate from [`Hello`] because `Hello` is the version-gated WORK
/// body and may change shape; these three fields are the contract that
/// does not. A frame that cannot produce one of these is a frame this
/// monitor knows nothing about, and nothing is decided from it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct Opening {
    pub(crate) proto: u32,
    pub(crate) repo: String,
    pub(crate) who: OpeningWho,
}

/// The floor's view of who is asking.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub(crate) enum OpeningWho {
    Agent {
        author: String,
    },
    Observer,
    /// A kind this build does not know. Decided AGAINST, never for: a
    /// client we cannot identify is not one whose word ends a repo's
    /// owner.
    #[serde(other)]
    Unknown,
}

/// Read an opening frame, and notice when answering it means this
/// monitor is finished.
///
/// A client stays version-gated exactly as before — a body decoded
/// under the wrong version is the hazard that gate exists for. The ONE
/// addition is which side of the mismatch we are on, and it is reached
/// only after the frame has been VALIDATED as something addressed to
/// this repo by somebody this repo knows.
///
/// That validation is the correction, and it was a real hole: the
/// first version re-read `Envelope.proto` alone, so a bare
/// `{"proto":6}` — no repo, no identity, from anyone who could reach
/// the socket — retired the monitor, as did a higher-version frame
/// asking for a different repo entirely (codex on c4b5899). I had
/// claimed an unknown author could not reach this decision; that was
/// true only of the equal-version path, which is not the path that
/// matters.
/// What a monitor says when a newer client tells it it is obsolete. The
/// same words since proto 3, so a client can tell this answer from an
/// older monitor's other refusals byte for byte.
pub(crate) fn obsolete_why(ours: u32, theirs: u32) -> String {
    format!("this monitor speaks proto {ours}, the client speaks {theirs}")
}

pub(crate) fn greeting_of(line: &str, repo: &std::path::Path) -> Greeting {
    let decline = |why: String| Greeting::Decline(Welcome::Decline { proto: PROTO, why });
    let Ok(open) = serde_json::from_str::<Opening>(line) else {
        // Not even the floor. Whatever it is, it decides nothing.
        return match accept_hello(line, repo) {
            Ok(hello) => Greeting::Client(hello),
            Err(d) => Greeting::Decline(d),
        };
    };
    if !serves(repo, &open.repo) {
        return decline(format!(
            "this monitor serves {}, the client asked for {}",
            repo.display(),
            open.repo
        ));
    }
    if open.proto > PROTO {
        // Older than the client, and the client is addressed to this
        // repo and known to it. We are what an install replaced.
        return match &open.who {
            OpeningWho::Observer => Greeting::Obsolete(Welcome::Decline {
                proto: PROTO,
                why: obsolete_why(PROTO, open.proto),
            }),
            OpeningWho::Agent { author } => match AgentLabel::parse(author)
                .ok()
                .filter(|a| crate::agent_store::resolve_role(repo, a).is_ok())
            {
                Some(_) => Greeting::Obsolete(Welcome::Decline {
                    proto: PROTO,
                    why: obsolete_why(PROTO, open.proto),
                }),
                // On this repo's socket, for this repo, but not one of
                // its agents. Refused, and this monitor stays.
                None => decline(format!("`{author}` is not on this repo's roster")),
            },
            OpeningWho::Unknown => {
                decline("a client kind this monitor does not recognise".to_string())
            }
        };
    }
    match accept_hello(line, repo) {
        Ok(hello) => Greeting::Client(hello),
        Err(d) => Greeting::Decline(d),
    }
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
pub(crate) const SPAWN_SETTLE: Duration = Duration::from_millis(600);

/// How many times a client tries to attach before watching for itself.
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
pub(crate) fn connect_within(
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
pub(crate) fn write_frame(
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
            | Seen::OlderPeer { why, .. }
            | Seen::Unproven(why) => Attached::Alone(why),
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
        &runtime_dir(unsafe { libc::getuid() }),
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
    let socket = match published_in(namespace, repo, unsafe { libc::getuid() }) {
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
        repo: repo.to_string_lossy().to_string(),
        who,
    };
    let line = match serde_json::to_string(&hello) {
        Ok(line) => line,
        Err(e) => return Seen::Unproven(format!("cannot compose a hello: {e}")),
    };

    let mut conn = Conn::over_reader(std::io::BufReader::new(stream));
    if let Err(e) = write_frame(conn.io.get_ref(), format!("{line}\n").as_bytes(), deadline) {
        return Seen::Unproven(format!("cannot greet {}: {e}", socket));
    }

    let answer = match read_frame(&mut conn.io, &mut conn.partial, deadline) {
        Ok(answer) => answer,
        Err(e) => {
            return Seen::Unproven(format!("the handshake with {} failed: {e}", socket));
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

/// The right to try a REPAIR of this repo's monitor, held while one
/// client does it.
///
/// Not the canonical lease. That one stops two monitors OWNING a repo
/// and says nothing about how many clients may poll for it or fork a
/// child at it: each could take and drop it and each then start one,
/// which is a poller and a fork per waiting client (codex on
/// cf29219). This coalesces them to one useful repair per repo.
///
/// Flocked, so it coalesces ACROSS processes — every parked wait is
/// its own — and held until startup and classification are done,
/// including the failing and expiring paths, because a permit
/// released early is one the next client takes while this one is
/// still working.
///
/// It lives beside the SOCKET rather than under `.clank/`: it is
/// process state with no meaning once the process is gone, and the
/// repo is not where such things belong.
#[derive(Debug)]
pub(crate) struct RepairPermit {
    _file: std::fs::File,
}

impl Drop for RepairPermit {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // A concurrent fork can briefly inherit this open-file description
        // before CLOEXEC closes it. Release explicitly when repair ends,
        // rather than leave admission waiting for that unrelated child.
        unsafe {
            libc::flock(self._file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Where this uid's RUNTIME state lives: the descriptor, the monitor
/// lease, the repair gate and the status lease.
///
/// Outside the repo, because none of it means anything once its
/// process is gone — and because whether git could see it depended on
/// each repo's `.clank/.gitignore`, a file that propagates by being
/// TRACKED. A fork from a repo that never had one started dirty and
/// nothing in the new worktree fixed it.
///
/// FIXED, and deliberately independent of the environment. Socket
/// allocation may try candidates because the published descriptor then
/// tells everyone which was chosen; a rendezvous has no such
/// discovery, so every caller must derive the SAME path or it is not
/// one. Two clients differing only in `XDG_RUNTIME_DIR` each took a
/// repair permit in a different directory, locked different inodes,
/// and both forked (codex on 88b6234).
pub(crate) fn runtime_dir(me: u32) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/tmp/clank-run-{me}"))
}

/// Vet and create this uid's runtime directory.
///
/// For the MUTATION boundaries only. Discovery derives a path without
/// it, so a read-only lookup never has to make a directory to find out
/// where something would be.
pub(crate) fn ensure_runtime_dir() -> Result<(), String> {
    let me = unsafe { libc::getuid() };
    ensure_dir(&runtime_dir(me), me)
}

/// This repo's runtime path with the given extension.
///
/// Derivation ONLY: it creates nothing and needs no write access, so a
/// read-only lookup — which is what discovery is — can ask where a
/// descriptor would be without making a directory to find out. The
/// mutation boundaries vet and create (codex on 2a0c59a).
pub(crate) fn runtime_path(repo: &std::path::Path, ext: &str) -> std::path::PathBuf {
    runtime_path_in(&runtime_dir(unsafe { libc::getuid() }), repo, ext)
}

/// [`runtime_path`] with the namespace supplied.
///
/// The derivation itself, so a test can hand it a directory that does
/// NOT exist and watch it stay that way. Asking against the real
/// namespace proves nothing: other tests have already created it.
pub(crate) fn runtime_path_in(
    dir: &std::path::Path,
    repo: &std::path::Path,
    ext: &str,
) -> std::path::PathBuf {
    dir.join(format!("{}.{ext}", repo_tag(repo)))
}

/// A temp repo that takes its RUNTIME entries with it.
///
/// The namespace outlives the repo it is keyed by, which is the whole
/// point — and which means a tempdir that used to carry its lock away
/// now leaves one behind. Production's set is bounded by the repos on
/// the machine; a suite that makes a fresh repo per test is bounded by
/// nothing, and left about four hundred entries per run.
///
/// Lives here rather than in each test module because there is one
/// namespace, and a second copy of this list would be a second answer
/// to the same question.
///
/// Unlinking is safe HERE and not in production: the name is derived
/// from this tempdir's own path, so no other repo's lease can be at it,
/// and drop order releases a lease declared after the repo before this
/// runs. In production, unlinking a lock a holder still has open would
/// let the next caller lock a different inode at the same name — which
/// is why nothing sweeps this directory.
#[cfg(test)]
pub(crate) struct TempRepo(tempfile::TempDir);

#[cfg(test)]
impl TempRepo {
    pub(crate) fn new() -> Self {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".clank")).unwrap();
        Self(d)
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        self.0.path()
    }
}

#[cfg(test)]
impl Drop for TempRepo {
    fn drop(&mut self) {
        for ext in ["json", "lock", "status", "repair"] {
            let _ = std::fs::remove_file(runtime_path(self.0.path(), ext));
        }
        // Mutation callers canonicalize even when a test's temp root has a
        // /var or /tmp alias. Their inode is keyed by that canonical path.
        if let Ok(repo) = dunce::canonicalize(self.0.path()) {
            let _ = std::fs::remove_file(runtime_path(&repo, "plan-mutation"));
            let _ = std::fs::remove_file(runtime_path(&repo, "roster"));
        }
    }
}

/// What asking for the repair gate established.
///
/// `Busy` and `Unavailable` are different facts and were one: every
/// open, create and lock failure reported as "another client is
/// already starting one", which is not established when the namespace
/// itself could not be used.
#[derive(Debug)]
pub(crate) enum Repair {
    Acquired(RepairPermit),
    /// Somebody else is repairing this repo.
    Busy,
    /// The gate could not be used at all, and why.
    Unavailable(String),
}

/// Take this repo's repair gate.
pub(crate) fn take_repair_permit(repo: &std::path::Path) -> Repair {
    let me = unsafe { libc::getuid() };
    take_repair_permit_in(repo, &runtime_dir(me), me)
}

/// [`take_repair_permit`] with the namespace supplied, so a test can
/// drive an unusable one — and so the production choice of `repair_dir`
/// is observable rather than assumed.
pub(crate) fn take_repair_permit_in(
    repo: &std::path::Path,
    dir: &std::path::Path,
    me: u32,
) -> Repair {
    // The same vetting the socket boundary does, for a reason that
    // belongs to this lock rather than to security in general: a gate
    // file replaced while a holder keeps its old fd lets the next
    // caller lock a DIFFERENT inode at the same path, and then both
    // are repairing.
    if let Err(why) = ensure_dir(dir, me) {
        return Repair::Unavailable(why);
    }
    match repair_permit(repo, dir) {
        Ok(Some(permit)) => Repair::Acquired(permit),
        Ok(None) => Repair::Busy,
        Err(why) => Repair::Unavailable(why),
    }
}

/// Take the repair permit in `dir`.
pub(crate) fn repair_permit(
    repo: &std::path::Path,
    dir: &std::path::Path,
) -> Result<Option<RepairPermit>, String> {
    use std::os::fd::AsRawFd;
    let path = dir.join(format!("{}.repair", repo_tag(repo)));
    // Never unlinked: the held fd and its flock ARE the state, and
    // removing the file on release would let the next caller create a
    // new inode at the same name beside a holder still on the old one.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| format!("cannot open the repair gate at {}: {e}", path.display()))?;
    // SAFETY: valid owned fd; flock has no memory effects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    Ok((rc == 0).then_some(RepairPermit { _file: file }))
}

/// A stable digest of a canonical repo path.
///
/// blake3 over the path's RAW BYTES. `DefaultHasher` was wrong twice
/// over: its algorithm is explicitly unspecified across Rust releases,
/// which bites exactly when binaries from two installs contend during
/// an upgrade, and `to_string_lossy` is not the path. The scheme must
/// not vary with `PROTO`, the compiler, or the standard library.
pub(crate) fn repo_tag(repo: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    blake3::hash(repo.as_os_str().as_bytes()).to_hex()[..32].to_string()
}

/// Whether this repo's ownership became available./// Whether this repo's ownership became available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ownership {
    /// The lease was taken and released, so the next monitor started
    /// can hold it.
    Ready,
    /// Still held when the deadline passed. Not permission.
    Busy,
}

/// Wait, within one deadline, for this repo's ownership to free.
///
/// Probing by TAKING the lease is the canonical proof and needs no
/// timer: it is non-blocking, and dropping it immediately hands the
/// repo to whichever monitor is started next. Several clients racing
/// here is fine — whoever wins serves the rest.
#[cfg(test)]
pub(crate) async fn wait_for_ownership(
    repo: &std::path::Path,
    by: std::time::Instant,
    step: Duration,
) -> Ownership {
    wait_for_ownership_by(repo, by, step, |r| acquire_lease(r).is_ok()).await
}

/// Wait within one deadline using the caller's non-blocking ownership probe.
///
/// A seam, because the boundary that matters cannot be reached
/// otherwise: ownership becoming available and then the deadline
/// passing BEFORE anything is started. Taking the real lease is
/// fast, so a test cannot arrange that gap by scheduling — and a
/// guard nothing can drive is a guard nobody has checked.
pub(crate) async fn wait_for_ownership_by(
    repo: &std::path::Path,
    by: std::time::Instant,
    step: Duration,
    took_it: impl Fn(&std::path::Path) -> bool,
) -> Ownership {
    loop {
        // Expiry BEFORE readiness. Taking the lease first meant an
        // already-expired deadline still returned Ready when the lease
        // happened to be free, and so did a timer that resumed after
        // `by` — and the caller then started a child with no further
        // check (codex on cf29219).
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Ownership::Busy;
        }
        if took_it(repo) {
            return Ownership::Ready;
        }
        // Async, so a client waiting for a monitor to go does not park
        // the runtime thread it is waiting on.
        tokio::time::sleep(step.min(left)).await;
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
        |r| acquire_lease(r).is_ok(),
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
    took_it: impl Fn(&std::path::Path) -> bool,
) -> Attached {
    // ONE deadline over the whole handoff: notify, wait for ownership
    // to free, start, attach. The retries used to take a fresh budget
    // each, so the count was the bound rather than any deadline.
    let by = std::time::Instant::now() + settle + budget;
    attach_or_spawn_until(
        repo,
        who,
        by,
        budget,
        settle / limit,
        limit,
        spawn,
        connect,
        took_it,
    )
    .await
}

/// [`attach_or_spawn_by`] under a deadline the caller already holds: the
/// first handshake gets at most `first` of what is left of it, and every
/// later step only what is left when it starts.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn attach_or_spawn_until(
    repo: &std::path::Path,
    who: Who,
    by: std::time::Instant,
    first: Duration,
    step: Duration,
    limit: u32,
    spawn: impl Fn(&std::path::Path) -> std::io::Result<Box<dyn Started>>,
    connect: impl Fn(
        &std::path::Path,
        std::time::Instant,
    ) -> std::io::Result<std::os::unix::net::UnixStream>
    + Copy,
    took_it: impl Fn(&std::path::Path) -> bool,
) -> Attached {
    let left = by.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Attached::Alone("the handoff ran out of time before it began".into());
    }
    let mut why = match look_at(repo, who.clone(), first.min(left), connect) {
        Seen::Shared(conn) => return Attached::Shared(conn),
        // A monitor that should keep the repo. Starting one would add
        // a monitor that cannot own anything, and doing it on every
        // park is the cost this classification exists to remove.
        Seen::Theirs(why) | Seen::Newer(why) | Seen::Unproven(why) => {
            return Attached::Alone(why);
        }
        Seen::Vacant(why) | Seen::OlderPeer { why, .. } => why,
    };

    // The repo is ours when its LEASE frees, and nothing sooner says
    // so — so whether it freed has to be an ANSWER, not a fall-through.
    // All three ways out of the old loop reached the start below:
    // acquired, retries exhausted, deadline gone. Only the first is
    // permission (codex on 1c0ce9d).
    //
    // Bounded by the DEADLINE and not by a retry count, because the
    // count ended the wait after about 450ms while a holder standing
    // down legitimately has its retirement budget plus settlement. A
    // child started at 450ms loses the lease and exits, and nothing
    // creates the replacement once the lease finally frees.
    // ONE repair per repo, not one per waiting client. The permit is
    // taken before any lease polling and held through startup — if
    // another client holds it, this one is not a second repairer and
    // does its own work (codex on cf29219).
    let permit = match take_repair_permit(repo) {
        Repair::Acquired(permit) => permit,
        Repair::Busy => {
            return Attached::Alone(format!("{why}, and another client is already starting one"));
        }
        // Not established that anybody is repairing — only that we
        // cannot ask. Saying otherwise would be a guess reported as a
        // fact.
        Repair::Unavailable(gate) => {
            return Attached::Alone(format!("{why}, and the repair gate is unusable: {gate}"));
        }
    };

    // A winner may have appeared while we queued for the permit, so
    // the answer from before it is stale. Attaching to a compatible
    // one, or leaving a newer one serving, is cheaper and more correct
    // than starting anything.
    let left = by.saturating_duration_since(std::time::Instant::now());
    if !left.is_zero() {
        match look_at(repo, who.clone(), left, connect) {
            Seen::Shared(conn) => return Attached::Shared(conn),
            Seen::Theirs(next) | Seen::Newer(next) | Seen::Unproven(next) => {
                return Attached::Alone(next);
            }
            Seen::Vacant(next) | Seen::OlderPeer { why: next, .. } => why = next,
        }
    }

    match wait_for_ownership_by(repo, by, step, took_it).await {
        Ownership::Ready => {}
        // Still held when time ran out. This client does its own work;
        // a monitor that cannot own the repo helps nobody.
        Ownership::Busy => {
            return Attached::Alone(format!("{why}, and its lease is still held"));
        }
    }

    // Expiry again, immediately before starting. Checking it before
    // the lease probe is necessary and does not cover a probe that
    // finished after `by`, or this task resuming past it (codex on
    // 06021ee). The permit goes with the early return.
    if by
        .saturating_duration_since(std::time::Instant::now())
        .is_zero()
    {
        return Attached::Alone(format!("{why}, and the handoff ran out of time"));
    }

    match spawn(repo) {
        // Handed over BEFORE any attach can return, so no exit path
        // below can be the one that drops it.
        Ok(started) => supervise(started),
        Err(e) => return Attached::Alone(format!("{why}, and starting one failed: {e}")),
    }

    let mut attempts = 1;
    while !give_up_sharing(attempts, limit) {
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        tokio::time::sleep(step.min(left)).await;
        attempts += 1;
        // What is LEFT of the one deadline, never a fresh budget: an
        // attempt entering its handshake just before `by` could
        // otherwise block for another whole `ATTACH_BUDGET`, and an
        // outer check cannot make that bounded.
        let left = by.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match look_at(repo, who.clone(), left, connect) {
            Seen::Shared(conn) => return Attached::Shared(conn),
            Seen::Theirs(next) | Seen::Newer(next) | Seen::Unproven(next) => {
                return Attached::Alone(next);
            }
            Seen::Vacant(next) | Seen::OlderPeer { why: next, .. } => why = next,
        }
    }
    // Held until here: startup and classification are both done, and a
    // permit released sooner is one the next client takes while this
    // one is still working.
    drop(permit);
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
/// Ask the running monitor to restart one source, and say what it
/// answered. Never starts a monitor: with none running there is nothing
/// to restart.
pub(crate) fn request(repo: &std::path::Path, control: Control) -> Result<String, String> {
    let mut conn = match attach_by(
        repo,
        Who::Control { request: control },
        ATTACH_BUDGET,
        connect_within,
    ) {
        Attached::Shared(conn) => conn,
        Attached::Alone(why) => return Err(format!("no monitor is running for this repo ({why})")),
    };
    let deadline = std::time::Instant::now() + ATTACH_BUDGET + SETTLE_BUDGET;
    match conn.note(deadline) {
        Ok(Note::Done { what }) => Ok(what),
        Ok(Note::Refused { why }) => Err(why),
        Ok(other) => Err(format!("the monitor answered {other:?}")),
        Err(e) => Err(format!("the monitor did not answer: {e}")),
    }
}

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

    let installed = match crate::build_stamp::rendezvous() {
        Some(rendezvous) => rendezvous,
        None => crate::build_stamp::Rendezvous {
            path: std::env::current_exe()?,
            source: crate::build_stamp::Source::Fallback,
        },
    };
    monitor_command(&installed, repo)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map(|child| Box::new(Spawned(child)) as Box<dyn Started>)
}

/// The command that starts `repo`'s monitor from the installation at
/// `installed`, telling it where that is so it compares against the same
/// file it was started from.
fn monitor_command(
    installed: &crate::build_stamp::Rendezvous,
    repo: &std::path::Path,
) -> std::process::Command {
    let mut command = std::process::Command::new(&installed.path);
    command
        .args(["monitor", "start", "--repo"])
        .arg(repo)
        .arg("--installed-at")
        .arg(&installed.path)
        .args(["--installed-from", installed.source.as_str()]);
    command
}

/// Held for the life of the monitor. Dropping it — including by dying —
/// frees it, so there is no stale-holder protocol to get wrong.
///
/// A lock on a file no longer at the lease's path proves nothing: no one
/// looking at the path sees it, so the next client finds the path free
/// and starts a second monitor. So the lease knows which file it locked
/// and can be asked whether that is still the file at its path.
#[derive(Debug)]
pub(crate) struct MonitorLease {
    _file: std::fs::File,
    dir: std::path::PathBuf,
    path: std::path::PathBuf,
    me: u32,
    held: FileId,
}

/// Which file: its device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(md: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        FileId {
            dev: md.dev(),
            ino: md.ino(),
        }
    }
}

impl std::fmt::Display for FileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dev {} inode {}", self.dev, self.ino)
    }
}

/// Why a lease was not taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaseError {
    /// Another holder has the file at the path.
    Busy(String),
    /// What was locked could not be shown to be the file at the path.
    /// Never ownership, and never a reason to lease somewhere else.
    Unverifiable(String),
}

impl std::error::Error for LeaseError {}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LeaseError::Busy(why) | LeaseError::Unverifiable(why) => f.write_str(why),
        }
    }
}

/// What is at a lease's path, against what the lease locked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    Held,
    Absent,
    Other(FileId),
    Unreadable(String),
}

impl std::fmt::Display for Found {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Found::Held => f.write_str("the file held"),
            Found::Absent => f.write_str("absent"),
            Found::Other(id) => write!(f, "{id}"),
            Found::Unreadable(why) => write!(f, "unreadable ({why})"),
        }
    }
}

/// What is at `path`, against `held`.
fn found_at(path: &std::path::Path, held: FileId) -> Found {
    match std::fs::metadata(path) {
        Ok(md) if FileId::of(&md) == held => Found::Held,
        Ok(md) => Found::Other(FileId::of(&md)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Found::Absent,
        Err(e) => Found::Unreadable(e.to_string()),
    }
}

impl MonitorLease {
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub(crate) fn held(&self) -> FileId {
        self.held
    }

    /// Whether the file this lease locked is still the one at its path.
    pub(crate) fn check(&self) -> Found {
        found_at(&self.path, self.held)
    }

    /// Take the lease at this lease's path afresh, verified as at first.
    fn retake(&self) -> Result<MonitorLease, LeaseError> {
        acquire_lease_with(&self.dir, self.path.clone(), self.me, |_| {})
    }
}

/// Take a repo's monitor lease, or report who has it.
///
/// The lease IS the liveness, as it is for `StatusLease`: if it can be
/// taken, the previous owner is gone, whatever a descriptor still says.
/// The pid in the descriptor is for diagnostics and is never the
/// authority — a pid can be reused, a lease cannot.
pub(crate) fn acquire_lease(repo: &std::path::Path) -> Result<MonitorLease, LeaseError> {
    let me = unsafe { libc::getuid() };
    acquire_lease_in(repo, &runtime_dir(me), me)
}

/// [`acquire_lease`] with the namespace supplied, so a test can hand it
/// one it must refuse.
///
/// A namespace we cannot vet is an ERROR here, not a fallback to
/// somewhere else. The socket may try candidates because the published
/// descriptor then says which was chosen; a lease has no such
/// discovery, so a second path is not a degraded lease but a second
/// lease. The monitor therefore refuses to start and says why — and the
/// CLIENT is what degrades, to `Attached::Alone`, watching the repo
/// itself exactly as it does when no socket directory is usable.
pub(crate) fn acquire_lease_in(
    repo: &std::path::Path,
    dir: &std::path::Path,
    me: u32,
) -> Result<MonitorLease, LeaseError> {
    acquire_lease_with(dir, runtime_path_in(dir, repo, "lock"), me, |_| {})
}

/// The lease at `path`, accepted only when the file locked is the file
/// at the path. `between` runs after each open and before its lock, so
/// a test can replace the path there. A path replaced in that window is
/// tried once more, then unverifiable.
fn acquire_lease_with(
    dir: &std::path::Path,
    path: std::path::PathBuf,
    me: u32,
    between: impl Fn(&std::path::Path),
) -> Result<MonitorLease, LeaseError> {
    use std::os::fd::AsRawFd;

    if let Err(why) = ensure_dir(dir, me) {
        return Err(LeaseError::Unverifiable(format!(
            "cannot use this uid's runtime directory: {why}"
        )));
    }
    let mut replaced = None;
    for _ in 0..2 {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| {
                LeaseError::Unverifiable(format!(
                    "cannot open the monitor lease at {}: {e}",
                    path.display()
                ))
            })?;
        between(&path);
        // SAFETY: valid owned fd; flock has no memory effects.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.kind() == std::io::ErrorKind::WouldBlock {
                LeaseError::Busy(format!(
                    "another monitor already owns this repo: its lease at {} is held",
                    path.display()
                ))
            } else {
                LeaseError::Unverifiable(format!(
                    "cannot lock the monitor lease at {}: {e}",
                    path.display()
                ))
            });
        }
        let held = file.metadata().map(|md| FileId::of(&md)).map_err(|e| {
            LeaseError::Unverifiable(format!(
                "cannot read the locked lease at {}: {e}",
                path.display()
            ))
        })?;
        match found_at(&path, held) {
            Found::Held => {
                return Ok(MonitorLease {
                    _file: file,
                    dir: dir.to_path_buf(),
                    path,
                    me,
                    held,
                });
            }
            other => replaced = Some((held, other)),
        }
    }
    let (held, found) = replaced.expect("the loop ran");
    Err(LeaseError::Unverifiable(format!(
        "the monitor lease at {} kept changing while it was taken: locked {held}, found {found}",
        path.display()
    )))
}

/// Publish a descriptor by RENAME, never by truncating in place.
///
/// A client reads this without the lease, so a partial write would be
/// read as a corrupt descriptor — and truncating in place can leave the
/// tail of a longer predecessor behind when the new one is shorter.
/// Rename is atomic within a directory, so a reader sees one whole
/// descriptor or the previous one (codex on 06ccd66).
pub(crate) fn publish(repo: &std::path::Path, d: &Descriptor) -> std::io::Result<()> {
    let me = unsafe { libc::getuid() };
    publish_in(repo, &runtime_dir(me), me, d)
}

/// [`publish`] with the namespace supplied, so a test can hand it one
/// it must refuse.
///
/// Vetted, like every other mutation boundary in this namespace, and
/// for a sharper reason than the others: the descriptor is how a
/// client FINDS the socket. A directory another uid planted in
/// world-writable `/tmp` would take a descriptor naming THEIR socket,
/// and every client would connect to it. An unvetted `create_dir_all`
/// was also a way to create the namespace at the umask's mode — 0o775
/// under 002 — which `dir_is_acceptable` then refuses forever, so the
/// first publish on such a machine locked every later lease out of its
/// own directory.
pub(crate) fn publish_in(
    repo: &std::path::Path,
    dir: &std::path::Path,
    me: u32,
    d: &Descriptor,
) -> std::io::Result<()> {
    ensure_dir(dir, me).map_err(std::io::Error::other)?;
    let final_path = runtime_path_in(dir, repo, "json");
    // Named for the writer so a stray temp from a dead monitor cannot
    // be mistaken for this one's.
    let tmp = final_path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(d)?)?;
    std::fs::rename(&tmp, &final_path)
}

/// What a repo's monitor published, if anything readable.
#[cfg(test)]
pub(crate) fn read_published(repo: &std::path::Path) -> Option<Descriptor> {
    match published(repo) {
        Published::Ours(d) => Some(d),
        _ => None,
    }
}

/// What a repo's published descriptor turns out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Published {
    /// Ours, and its body parsed.
    Ours(Descriptor),
    /// A monitor of ANOTHER version published this. The number is
    /// trusted because it was read first; the socket only when the
    /// rest of the body happened to parse, which is why it is
    /// optional. Without one there is no way to reach the holder, and
    /// a successor must say that rather than guess a path.
    ///
    /// The pid likewise, read on its own so that a body whose other
    /// fields changed can still be asked whether its process lives.
    Other {
        proto: u32,
        pid: Option<i32>,
        socket: Option<String>,
    },
    /// Nothing there, or nothing readable even as a version.
    Nothing,
    /// The NAMESPACE is not ours, so nothing read from it is evidence
    /// of anything. Distinct from `Nothing` because an absence invites
    /// starting a monitor and this does not: the lease lives in the
    /// same namespace and cannot be taken either.
    Untrusted(String),
}

/// What the runtime namespace says about this repo's monitor.
pub(crate) fn published(repo: &std::path::Path) -> Published {
    let me = unsafe { libc::getuid() };
    published_in(&runtime_dir(me), repo, me)
}

/// Read the descriptor VERSION FIRST, as every frame on the socket is.
///
/// Serde parses a struct whole, so a descriptor whose SHAPE changed
/// fails before its `proto` can be read, and the reader cannot tell "a
/// monitor of another version is here" from "no monitor is here". The
/// first is a candidate to replace; the second is a reason to start
/// one. Collapsing them is how a successor would spawn against a live
/// holder for ever (codex on 258d979).
pub(crate) fn published_in(dir: &std::path::Path, repo: &std::path::Path, me: u32) -> Published {
    match trusted_dir(dir, me) {
        Trust::Ours => {}
        Trust::Absent => return Published::Nothing,
        Trust::Theirs(why) => return Published::Untrusted(why),
    }
    let Ok(raw) = std::fs::read(runtime_path_in(dir, repo, "json")) else {
        return Published::Nothing;
    };
    let Ok(envelope) = serde_json::from_slice::<Envelope>(&raw) else {
        return Published::Nothing;
    };
    let body = serde_json::from_slice::<Descriptor>(&raw).ok();
    let pid = serde_json::from_slice::<PidEnvelope>(&raw)
        .ok()
        .map(|p| p.pid);
    if !admits(PROTO, envelope.proto) {
        return Published::Other {
            proto: envelope.proto,
            pid,
            // An older descriptor still parses here when it only ADDED
            // fields we now ignore, which is how a successor reaches
            // the holders this plan exists for.
            socket: body.map(|d| d.socket),
        };
    }
    match body {
        Some(d) => Published::Ours(d),
        // Our own number on a body we cannot read. Not a candidate and
        // not an absence: something is there and it is not what it
        // says it is.
        None => Published::Other {
            proto: envelope.proto,
            pid,
            socket: None,
        },
    }
}

#[derive(serde::Deserialize)]
struct PidEnvelope {
    pid: i32,
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
    Agent { author: AgentLabel },
    Observer,
    Helper { author: AgentLabel },
}

#[derive(Debug)]
struct Client {
    who: Served,
    holder: Option<Holder>,
    out: tokio::sync::mpsc::Sender<String>,
    /// The task that owns this client's socket.
    ///
    /// Retained so a retirement can tell TRANSMISSION from queue
    /// admission. `send_note` only `try_send`s into `out`; the frame
    /// is written by this task, and a detached handle is cancelled
    /// when the runtime shuts down — so returning from `start` could
    /// lose the final frame, and a transient in it has no WAL row to
    /// replay from (codex on 294c18d).
    ///
    /// `None` only in the tests that read a queue directly and have no
    /// socket for anyone to write to.
    writer: Option<tokio::task::JoinHandle<Wrote>>,
    /// The writer's last frame, written after everything queued once
    /// the queue closes, so a full queue cannot drop it.
    farewell: Option<tokio::sync::oneshot::Sender<String>>,
    /// What THIS client has been handed from its author's source logs.
    ///
    /// Per client, not per monitor: the monitor outlives every client,
    /// so a set it kept for itself would mean a client admitted later
    /// is told nothing about rows logged before it arrived. See
    /// `command_source::owed_from_log`.
    shown: std::collections::HashSet<(String, u64, String)>,
    /// This connection, so that a delivery result is credited only to the
    /// connection that carried the presentation.
    conn: u64,
    /// The hook's report this connection carries, until its verdict.
    report: Option<Reporting>,
}

/// A report waiting on its verdict.
#[derive(Debug)]
struct Reporting {
    report: super::turn_wire::Report,
    /// Whether it has been stepped once: after that it is a parked report,
    /// not a new Stop.
    stepped: bool,
    /// The connection's read side, with anything read past the hello, until
    /// the owner starts hearing it.
    reader: Option<(Vec<u8>, tokio::net::unix::OwnedReadHalf)>,
}

/// A connection's identity, never reused within a monitor's life.
fn next_conn() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// What a reporting connection said after its hello, or that it closed.
#[derive(Debug)]
enum Heard {
    Line(String),
    Closed,
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
        super::MonitorCmd::Upgrade => super::monitor_upgrade::run().await,
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

    // The build before the lease: a monitor already stale when it first
    // looks never owns the repo, so a replacement between its exec and
    // here is not advertised.
    let running = crate::build_stamp::running();
    let installed = match args.installed_at.clone() {
        Some(path) => Some(crate::build_stamp::Rendezvous {
            path,
            source: args
                .installed_from
                .as_deref()
                .and_then(crate::build_stamp::Source::parse)
                .unwrap_or(crate::build_stamp::Source::Invocation),
        }),
        None => crate::build_stamp::rendezvous(),
    };
    let mut installation = installed
        .as_ref()
        .map(|r| crate::build_stamp::Installation::at(r.path.clone()));
    if let Some(installation) = &mut installation
        && let crate::build_stamp::Install::Replaced(other) = installation.look(running)
    {
        eprintln!(
            "monitor: {} holds build {other}, not this one ({running}); not starting",
            installation.path().display()
        );
        return Ok(());
    }

    // The lease first: everything after it assumes no other owner,
    // including clearing a socket a dead one left behind.
    let lease = acquire_lease(&repo).map_err(|e| anyhow::anyhow!("{e}"))?;
    let me = unsafe { libc::getuid() };
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let pid = std::process::id() as i32;
    let (listener, socket) = bind_listener(&socket_dirs(me, xdg.as_deref()), pid, me, &lease)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    forget_dead_observations(&repo);
    let described = installed.as_ref().map(|r| DescribedBuild {
        running: running.to_string(),
        installed_at: r.path.to_string_lossy().into_owned(),
        source: r.source,
    });
    let mut tenure = Tenure::new(lease);
    if let (Some(installation), Some(installed)) = (installation, installed.as_ref()) {
        let (repo, birth) = (repo.clone(), crate::proc_identity::token_for(pid));
        let (installed_at, source) = (
            installed.path.to_string_lossy().into_owned(),
            installed.source,
        );
        tenure = tenure.with_build(Built::new(
            installation,
            running,
            Box::new(move |install| {
                let observation = Observation {
                    pid,
                    birth: birth.clone(),
                    running: running.to_string(),
                    installed_at: installed_at.clone(),
                    source,
                    state: install.into(),
                };
                if let Err(e) = note_observation(&repo, &observation) {
                    eprintln!("monitor: cannot record what it sees of its installation: {e}");
                }
            }),
        ));
    }
    let describe_this = {
        let (repo, socket, described) = (repo.clone(), socket.clone(), described.clone());
        move || Descriptor {
            build: described.clone(),
            ..describe(&repo, pid, &socket)
        }
    };
    let republish = {
        let (repo, describe_this) = (repo.clone(), describe_this.clone());
        move || publish(&repo, &describe_this()).map_err(|e| e.to_string())
    };
    let lease_lost = tenure.watch.clone();

    // The monitor's own fold counter. Private on purpose: it was
    // once published and put on every frame, and no reader ever
    // compared two of them — `beat_overdue`'s own doc says comparing
    // ticks "would add nothing a client can act on". What needs it is
    // the watchdog, in this process, and a stalled fold reaches a
    // client as silence rather than as a number that failed to move.
    let mut fold: u64 = 0;
    publish(&repo, &describe_this())?;
    println!(
        "monitor for {} listening on {}",
        repo.display(),
        socket.display()
    );

    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let (joined, mut arrivals) = tokio::sync::mpsc::unbounded_channel::<Greeted>();
    let mut clients: Vec<Client> = Vec::new();
    // Writers of clients already retired — on a wake, or for falling
    // behind — whose last frame may still be unwritten. Pruned as they
    // finish, so this never grows with the number of wakes a long-lived
    // monitor serves, and awaited by `retire` so a stand-down does not
    // cut one short.
    let mut leaving: Vec<tokio::task::JoinHandle<Wrote>> = Vec::new();

    // THE watcher for this repo. One, here, instead of one per parked
    // wait and two per `clank tui` — the whole point of the plan: ~170
    // of them were attached on the machine this was measured on, each
    // refolding the repo on every change.
    use futures_util::StreamExt;
    let (fs_tx, fs_rx) = std::sync::mpsc::channel::<()>();
    let timer_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let event_dirty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let _watcher = crate::repo_watch::RepoStateWatcher::attach_with_invalidations(
        &repo,
        false,
        fs_tx,
        Some(timer_dirty.clone()),
        Some(event_dirty.clone()),
    )?;
    let mut timer_backlogs: std::collections::BTreeMap<
        AgentLabel,
        crate::cli::timer_source::Backlog,
    > = Default::default();
    let mut beats = beats(fs_rx, FOLD_TICK);

    // Stamped by the fold loop below and by nothing else. A stamp from
    // the accept loop would prove only that the process breathes —
    // which is precisely the wedged monitor it has to detect.
    let heartbeat = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(fold));
    watch_the_fold(
        heartbeat.clone(),
        tenure.watch.clone(),
        WATCHDOG_TICK,
        WATCHDOG_MISSES,
    );

    // THE producers for this repo. One set per author, owned by this
    // process rather than by whichever parked wait happened to take
    // the ingest lease — which is what stops the user's command being
    // respawned on every delivery.
    // A SIGNAL, never a payload. What a client is owed is decided in
    // one place, from the durable log and that client's own `shown`
    // set; an item carried here and sent straight on would bypass both
    // — delivering a row a later admission replays again, or one some
    // other client has since acked (codex on ca2ea69).
    let (source_items, mut from_sources) =
        tokio::sync::mpsc::unbounded_channel::<(AgentLabel, crate::cli::event_source::Emission)>();
    // The last completed fold, so a source signal can present against
    // the same state a tick would.
    let mut folded_state: Option<crate::repo_state::RepoState> = None;
    // Transients with nowhere to go yet. A genuine non-durable
    // emission arriving before the first fold has no state to present
    // against AND no row for a later fold to replay, so dropping it
    // loses it outright (codex on aa3d9a5).
    let mut pending: Vec<(AgentLabel, WaitItem)> = Vec::new();
    let mut hosted: std::collections::BTreeMap<AgentLabel, Hosted> = Default::default();
    // Collected by the owner loop below as they end, and handed whole
    // to the retirement: `abort` only asks, and a producer or a
    // handshake live at the cutoff still owns its resources.
    let mut owned = OwnedTasks::default();
    // What each hosted command source's producer last reported, published
    // with the sources for the panels that show them.
    let (lifecycle_tx, mut lifecycles_in) = tokio::sync::mpsc::unbounded_channel();
    let mut lifecycles: Lifecycles = Default::default();
    let mut account = None;
    let birth = crate::proc_identity::token_for(std::process::id() as i32);
    owned.departing.extend(reconcile_hosting_reporting(
        &repo,
        &mut hosted,
        &source_items,
        false,
        Some(&lifecycle_tx),
    ));
    if let Some(birth) = &birth {
        publish_sources(
            &repo,
            sources_account(&hosted, &lifecycles, birth.clone()),
            &mut account,
            &clients,
        );
    }

    for author in roster_authors(&repo) {
        owned.interrupting.notify(&repo, author);
    }
    // Agents whose turn failed with nothing left to wake them.
    let (recovery_answers, mut answered) = tokio::sync::mpsc::unbounded_channel::<RecoveryAnswer>();
    let (asks, mut authorizations) = tokio::sync::mpsc::unbounded_channel::<Authorize>();
    let (identities, mut identified) = tokio::sync::mpsc::unbounded_channel::<Identified>();
    let mut recovery = crate::cli::recovery::Driver::new(
        repo.as_ref().clone(),
        std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_default(),
    );
    recovery.discover_pending(&crate::agent_store::agents_root(&repo));
    let (record_changes, mut changed_records) = tokio::sync::mpsc::unbounded_channel();
    let records = crate::cli::recovery::RecordWatch::spawn(record_changes);
    // Every agent's turn-ends, decided here and nowhere else.
    let mut turns = crate::cli::turn_custody::Turns::new(repo.as_ref().clone());
    let (heard_tx, mut heard) = tokio::sync::mpsc::unbounded_channel::<(u64, Heard)>();
    let (check_in_asks, mut check_in_authorizations) =
        tokio::sync::mpsc::unbounded_channel::<CheckInAsk>();
    let (check_in_done, mut checked_in) = tokio::sync::mpsc::unbounded_channel::<CheckedIn>();
    let user_home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let (mut user_commands, uc_wakes) = crate::cli::user_command::live::UserCommands::new(
        &repo,
        user_home.clone(),
        crate::cli::user_command::store::Store::in_home(&user_home),
    );
    let crate::cli::user_command::live::Wakes {
        changed: mut uc_changed,
        asks: mut uc_asks,
        done: mut uc_done,
    } = uc_wakes;
    let ended = loop {
        if tenure.retiring() {
            tenure
                .watch
                .begin_retiring(LEASE_RETIREMENT + SETTLE_BUDGET);
            let decision = stand_down(
                &repo,
                std::mem::take(&mut clients),
                std::mem::take(&mut leaving),
                std::mem::take(&mut hosted),
                std::mem::take(&mut owned),
                &mut arrivals,
                &mut from_sources,
                folded_state.as_ref(),
                &mut pending,
                Some(Drain {
                    owners: &crate::cli::source_group::OWNERS,
                    grace: DRAIN_GRACE,
                }),
                LEASE_RETIREMENT,
                SETTLE_BUDGET,
            )
            .await;
            match decision {
                Shutdown::ReadyToRelease(_) => break Ok(()),
                Shutdown::RequiresProcessExit(_) => exit_holding_the_lease(),
            }
        }
        let timer_deadline = hosted
            .values()
            .filter_map(|host| {
                host.timers
                    .as_ref()
                    .and_then(|timers| timers.next_deadline())
            })
            .min();
        let recovery_due = recovery.next_due().map(|at| {
            let wait = at.saturating_sub(crate::cli::recovery::unix_now());
            tokio::time::Instant::now() + Duration::from_secs(wait)
        });
        let turns_due = [turns.next_deadline(), report_deadline(&clients)]
            .into_iter()
            .flatten()
            .min()
            .map(|at| {
                let wait = at.saturating_sub(crate::cli::turn_custody::now_ms());
                tokio::time::Instant::now() + Duration::from_millis(wait)
            });
        let uc_due = user_commands.next_due().map(|at| {
            let wait = at.saturating_sub(crate::cli::recovery::unix_now());
            tokio::time::Instant::now() + Duration::from_secs(wait)
        });
        // Receive, then look at the lease, then act: whatever woke the
        // loop, nothing it leads to runs as owner on a lease not
        // verified since.
        let wake = tokio::select! {
            // The watchdog saw the lease's path change.
            _ = lease_lost.wake.notified() => Wake::LeaseChanged,
            _ = crate::cli::timer_source::deadline(recovery_due) => Wake::RecoveryDue,
            _ = crate::cli::timer_source::deadline(turns_due) => Wake::TurnsDue,
            Some((conn, said)) = heard.recv() => Wake::Heard(conn, said),
            Some(ask) = check_in_authorizations.recv() => Wake::AuthorizeCheckIn(ask),
            Some(done) = checked_in.recv() => Wake::CheckedIn(done),
            Some(a) = authorizations.recv() => Wake::Authorize(a),
            Some(identity) = identified.recv() => Wake::Identified(identity),
            Some(answer) = answered.recv() => Wake::Answered(answer),
            Some(event) = changed_records.recv() => {
                let mut events = std::collections::BTreeSet::from([event]);
                while let Ok(more) = changed_records.try_recv() {
                    events.insert(more);
                }
                Wake::Records(events)
            }
            _ = crate::cli::timer_source::deadline(timer_deadline) => Wake::TimersDue,
            Some(()) = uc_changed.recv() => {
                while uc_changed.try_recv().is_ok() {}
                Wake::UserCommands
            }
            Some(ask) = uc_asks.recv() => Wake::UserCommandAsk(ask),
            Some(done) = uc_done.recv() => Wake::UserCommandDone(done),
            _ = crate::cli::timer_source::deadline(uc_due) => Wake::UserCommandsDue,
            accepted = listener.accept() => Wake::Accepted(accepted?.0),
            Some(greeted) = arrivals.recv() => Wake::Arrived(greeted),
            Some((author, emission)) = from_sources.recv() => Wake::Emitted(author, emission),
            Some(report) = lifecycles_in.recv() => Wake::Lifecycle(report),
            // A cancelled producer finished winding down. Nothing to
            // do but stop owning it — which is the point: this set is
            // what a retirement would otherwise have to wait for, and
            // collecting here keeps it from growing with the number of
            // config changes a long-lived monitor sees.
            Some(author) = owned.interrupting.next(), if !owned.interrupting.is_empty() => Wake::Interrupted(author),
            Some(_) = owned.departing.next(), if !owned.departing.is_empty() => Wake::Collected,
            // A handshake finished. Whatever it decided has already
            // gone to `arrivals`; this only stops us owning the task.
            Some(_) = owned.greeting.next(), if !owned.greeting.is_empty() => Wake::Collected,
            Some(_) = owned.hearing.next(), if !owned.hearing.is_empty() => Wake::Collected,
            // A beat: the FS event or the heartbeat, whichever landed.
            // Receiving from a channel is cancel-safe and carries no
            // state, which is the point — see `beats`.
            beat = beats.recv() => Wake::Beat(beat.is_some()),
        };
        let owner = tenure.may_act(&republish);
        // Which wakes are state changes a waiting user command is looked at
        // again on; its own store's changes are not, since a monitor writing
        // a record wakes itself.
        let uc_round = match &wake {
            Wake::Heard(..) | Wake::Arrived(..) | Wake::Records(..) | Wake::UserCommandsDue => {
                Some(true)
            }
            Wake::UserCommands => Some(false),
            _ => None,
        };
        match wake {
            Wake::LeaseChanged | Wake::Collected => {}
            Wake::UserCommands | Wake::UserCommandsDue => {}
            Wake::UserCommandDone(done) => user_commands.finished(done),
            Wake::UserCommandAsk(ask) => {
                let answer = if owner {
                    let settling = user_commands.settling(crate::cli::recovery::unix_now());
                    match user_command_look(
                        &repo,
                        &clients,
                        &turns,
                        &mut recovery,
                        &settling,
                        &ask.label,
                        &ask.session,
                    ) {
                        crate::cli::user_command::dispatch::Look::Go => Ok(()),
                        crate::cli::user_command::dispatch::Look::Parked => {
                            Err("its turn is parked".to_string())
                        }
                        crate::cli::user_command::dispatch::Look::Wait(why) => Err(why),
                    }
                } else {
                    Err("this monitor no longer holds the repo's lease".to_string())
                };
                let _ = ask.reply.send(answer);
            }
            Wake::RecoveryDue => {
                if owner {
                    let routes = recovery.check_in_routes();
                    let acts = recovery.due(crate::cli::recovery::unix_now(), |author, session| {
                        due_gate(
                            &repo,
                            &clients,
                            &mut turns,
                            &routes,
                            folded_state.as_ref(),
                            &hosted,
                            &mut timer_backlogs,
                            &pending,
                            author,
                            session,
                        )
                    });
                    act_on_recovery(
                        &repo,
                        &clients,
                        acts,
                        &recovery_answers,
                        &asks,
                        &records,
                        &mut owned,
                    );
                }
            }
            // Dropped unanswered when retiring, which the asker reads as
            // a veto.
            Wake::Authorize(a) => {
                if owner {
                    authorize(
                        &repo,
                        &clients,
                        &hosted,
                        &pending,
                        &mut recovery,
                        &mut turns,
                        a,
                    );
                }
            }
            Wake::TurnsDue => {
                let now = crate::cli::turn_custody::now_ms();
                leaving.extend(expire_reports(&mut clients, now));
                if owner {
                    let routes = recovery.check_in_routes();
                    let stepped = due_check_ins(&mut turns, now, |author| {
                        gather(
                            &repo,
                            folded_state.as_ref(),
                            author,
                            &hosted,
                            &mut Default::default(),
                            &pending,
                            &routes,
                        )
                    });
                    for (author, g, due) in stepped {
                        if let Some(due) = due {
                            owned.departing.push(tokio::spawn(deliver_check_in(
                                repo.as_ref().clone(),
                                due,
                                std::sync::Arc::new(Panes),
                                check_in_asks.clone(),
                                check_in_done.clone(),
                            )));
                        }
                        project(&mut recovery, &mut turns, &author, &g);
                    }
                }
            }
            Wake::Heard(conn, said) => {
                let routes = recovery.check_in_routes();
                let author = turns.carrier(conn).cloned();
                match said {
                    Heard::Line(line) => match (
                        author,
                        serde_json::from_str::<crate::cli::turn_wire::TurnResult>(&line),
                    ) {
                        (Some(author), Ok(result)) => {
                            let settled = settle_result(
                                &mut turns,
                                &mut pending,
                                &author,
                                conn,
                                result,
                                |pending| {
                                    gather(
                                        &repo,
                                        folded_state.as_ref(),
                                        &author,
                                        &hosted,
                                        &mut Default::default(),
                                        pending,
                                        &routes,
                                    )
                                },
                            );
                            let g = gather(
                                &repo,
                                folded_state.as_ref(),
                                &author,
                                &hosted,
                                &mut Default::default(),
                                &pending,
                                &routes,
                            );
                            match settled {
                                Ok(Some(commit)) => {
                                    let repo = repo.as_ref().clone();
                                    owned
                                        .departing
                                        .push(tokio::spawn(async move { commit.run(&repo).await }));
                                }
                                Ok(None) => {}
                                Err(why) => eprintln!("monitor: refused a delivery result: {why}"),
                            }
                            project(&mut recovery, &mut turns, &author, &g);
                            leaving.extend(serve_reports(
                                &repo,
                                &mut clients,
                                folded_state.as_ref(),
                                Some(&author),
                                &pending,
                                &mut Default::default(),
                                &hosted,
                                &mut turns,
                                &routes,
                            ));
                        }
                        (_, Ok(result)) => eprintln!(
                            "monitor: a result for {:?} came from a connection that carried \
                             nothing; nothing was committed",
                            result.presentation()
                        ),
                        (_, Err(e)) => {
                            eprintln!("monitor: an unreadable frame after a report: {e}")
                        }
                    },
                    Heard::Closed => {
                        let (gone, kept): (Vec<Client>, Vec<Client>) = std::mem::take(&mut clients)
                            .into_iter()
                            .partition(|c| c.conn == conn);
                        clients = kept;
                        leaving.extend(gone.into_iter().filter_map(|c| c.writer));
                        if let Some(author) = author {
                            let g = gather(
                                &repo,
                                folded_state.as_ref(),
                                &author,
                                &hosted,
                                &mut Default::default(),
                                &pending,
                                &routes,
                            );
                            turns.closed(&author, conn, &g);
                            project(&mut recovery, &mut turns, &author, &g);
                            leaving.extend(serve_reports(
                                &repo,
                                &mut clients,
                                folded_state.as_ref(),
                                Some(&author),
                                &pending,
                                &mut Default::default(),
                                &hosted,
                                &mut turns,
                                &routes,
                            ));
                        }
                    }
                }
            }
            Wake::AuthorizeCheckIn(ask) => {
                let allowed = if owner {
                    let routes = recovery.check_in_routes();
                    let g = gather(
                        &repo,
                        folded_state.as_ref(),
                        &ask.author,
                        &hosted,
                        &mut Default::default(),
                        &pending,
                        &routes,
                    );
                    may_check_in_now(&repo, &recovery, &mut turns, &g, &ask)
                } else {
                    Err("this monitor no longer holds the repo's lease".into())
                };
                let _ = ask.reply.send(allowed);
            }
            Wake::CheckedIn(done) => {
                if let Err(why) = done.outcome {
                    eprintln!(
                        "monitor: `{}`'s check-in was not delivered: {why}",
                        done.author
                    );
                    let routes = recovery.check_in_routes();
                    let g = gather(
                        &repo,
                        folded_state.as_ref(),
                        &done.author,
                        &hosted,
                        &mut Default::default(),
                        &pending,
                        &routes,
                    );
                    turns.check_in_failed(&done.author, done.check_in, why, &g);
                    project(&mut recovery, &mut turns, &done.author, &g);
                }
            }
            Wake::Identified((author, session, running)) => {
                let acts = recovery.identified(
                    &author,
                    &session,
                    running,
                    crate::cli::recovery::unix_now(),
                );
                if owner {
                    act_on_recovery(
                        &repo,
                        &clients,
                        acts,
                        &recovery_answers,
                        &asks,
                        &records,
                        &mut owned,
                    );
                }
            }
            Wake::Answered((author, attempt, answer)) => {
                let acts =
                    recovery.answer(&author, &attempt, answer, crate::cli::recovery::unix_now());
                if owner {
                    act_on_recovery(
                        &repo,
                        &clients,
                        acts,
                        &recovery_answers,
                        &asks,
                        &records,
                        &mut owned,
                    );
                }
            }
            Wake::Records(events) => {
                let now = crate::cli::recovery::unix_now();
                let mut acts = Vec::new();
                for event in events {
                    acts.extend(recovery.changed(event, now));
                }
                if owner {
                    act_on_recovery(
                        &repo,
                        &clients,
                        acts,
                        &recovery_answers,
                        &asks,
                        &records,
                        &mut owned,
                    );
                }
            }
            Wake::TimersDue => {
                if owner {
                    for host in hosted.values_mut() {
                        if let Some(timers) = &mut host.timers {
                            timers.due();
                        }
                    }
                }
            }
            // Dropped when retiring: the peer reads EOF and finds the
            // successor.
            Wake::Accepted(stream) => {
                if owner {
                    let repo = repo.clone();
                    let joined = joined.clone();
                    // Handshaking OFF this loop is the point: a peer that
                    // connects and says nothing must not stall folding or
                    // anyone else's registration.
                    owned.greeting.push(tokio::spawn(async move {
                        match greet(stream, &repo).await {
                            // Ownership transfers on a SUCCESSFUL send. A
                            // failed one means the receiver is gone, and
                            // dropping the `Greeted` there would detach
                            // the writer `admit` just spawned — a socket
                            // task nothing accounts for, after the
                            // admission cutoff (codex on b37b6b6). So the
                            // greeting tears down what it could not hand
                            // over, and the cutoff observes THIS task
                            // ending.
                            Ok(greeted) => {
                                if let Err(returned) = joined.send(greeted) {
                                    if let Greeted::Client(c) = returned.0
                                        && let Some(writer) = c.writer
                                    {
                                        writer.abort();
                                        let _ = writer.await;
                                    }
                                }
                            }
                            Err(e) => eprintln!("monitor: a client was not admitted: {e}"),
                        }
                    }));
                }
            }
            Wake::Arrived(greeted) => {
                let client = match greeted {
                    // Kept, not served: the retirement this leads to
                    // presents to it with everyone else.
                    Greeted::Client(mut client) => {
                        if let Some(reader) = client.report.as_mut().and_then(|r| r.reader.take()) {
                            owned.hearing.push(tokio::spawn(hear(
                                client.conn,
                                reader,
                                heard_tx.clone(),
                            )));
                        }
                        if !owner {
                            clients.push(client);
                            continue;
                        }
                        client
                    }
                    Greeted::Control {
                        request,
                        out,
                        writer,
                    } => {
                        let answered = match &request {
                            _ if !owner => {
                                Err("this monitor no longer holds the repo's lease".to_string())
                            }
                            Control::Restart { author, key, .. } => {
                                let restarted = restart_source(
                                    &repo,
                                    &mut hosted,
                                    &source_items,
                                    &lifecycle_tx,
                                    &mut owned.departing,
                                    &request,
                                )
                                .await;
                                if restarted.is_ok() {
                                    if let Ok(author) = AgentLabel::parse(author) {
                                        lifecycles.remove(&(author, key.clone()));
                                    }
                                    if let Some(birth) = &birth {
                                        publish_sources(
                                            &repo,
                                            sources_account(&hosted, &lifecycles, birth.clone()),
                                            &mut account,
                                            &clients,
                                        );
                                    }
                                }
                                restarted
                            }
                            Control::Observe { .. } | Control::RecoveryReply { .. } => {
                                let (answered, acts) = match recovery_control(
                                    &repo,
                                    &mut recovery,
                                    &request,
                                    crate::cli::recovery::unix_now(),
                                ) {
                                    Ok((said, acts)) => (Ok(said), acts),
                                    Err(why) => (Err(why), Vec::new()),
                                };
                                act_on_recovery(
                                    &repo,
                                    &clients,
                                    acts,
                                    &recovery_answers,
                                    &asks,
                                    &records,
                                    &mut owned,
                                );
                                answered
                            }
                            Control::Attend { .. } | Control::Clear { .. } => {
                                let routes = recovery.check_in_routes();
                                let answered =
                                    turn_control(&repo, &mut turns, &request, |author| {
                                        gather(
                                            &repo,
                                            folded_state.as_ref(),
                                            author,
                                            &hosted,
                                            &mut Default::default(),
                                            &pending,
                                            &routes,
                                        )
                                    });
                                answered.map(|(author, said)| {
                                    let g = gather(
                                        &repo,
                                        folded_state.as_ref(),
                                        &author,
                                        &hosted,
                                        &mut Default::default(),
                                        &pending,
                                        &routes,
                                    );
                                    project(&mut recovery, &mut turns, &author, &g);
                                    if turns.attending(&author) {
                                        recovery.wants_identity(&author);
                                    }
                                    identify_agents(&repo, &mut recovery, &identities, &mut owned);
                                    said
                                })
                            }
                        };
                        let note = match answered {
                            Ok(what) => Note::Done { what },
                            Err(why) => Note::Refused { why },
                        };
                        if let Ok(line) = serde_json::to_string(&note) {
                            let _ = out.try_send(format!("{line}\n"));
                        }
                        drop(out);
                        leaving.push(writer);
                        continue;
                    }
                    // Already acknowledged on its own connection, so
                    // nothing is owed to the successor here.
                    //
                    // The drain is one presentation opportunity for
                    // the clients attached right now, each bounded by
                    // `WRITE_BUDGET`, and then this process goes
                    // whether or not anyone read it. Waiting for
                    // quiescence instead would never finish: an
                    // observer holds its connection for as long as its
                    // panel is open. The ones that get nothing read
                    // EOF, which is the recovery door they already
                    // have.
                    // A monitor already retiring (a stale build, a lost
                    // lease) leaves by its own retirement, with the
                    // longer producer drain, not this short one.
                    Greeted::StandDown if !owner => continue,
                    Greeted::StandDown => {
                        let decision = stand_down(
                            &repo,
                            std::mem::take(&mut clients),
                            std::mem::take(&mut leaving),
                            std::mem::take(&mut hosted),
                            std::mem::take(&mut owned),
                            &mut arrivals,
                            &mut from_sources,
                            folded_state.as_ref(),
                            &mut pending,
                            Some(Drain {
                                owners: &crate::cli::source_group::OWNERS,
                                grace: HANDOFF_DRAIN,
                            }),
                            RETIRE_BUDGET,
                            SETTLE_BUDGET,
                        )
                        .await;
                        match decision {
                            // The lease is held for the life of this
                            // process, so returning IS the handoff —
                            // and it is only safe once nothing this
                            // monitor owned is still running.
                            Shutdown::ReadyToRelease(_) => break Ok(()),
                            Shutdown::RequiresProcessExit(_) => exit_holding_the_lease(),
                        }
                    }
                };
                // Served at once rather than at the next tick — the
                // producers have been running since the monitor
                // started, so a newcomer may already be owed what they
                // logged — but to the NEWCOMER ALONE. A queue is
                // `CLIENT_QUEUE` deep, so presenting to everybody on
                // each arrival would let a burst of joins retire
                // established readers that were keeping up perfectly
                // well (codex on 0555c8f).
                leaving.retain(|w| !w.is_finished());
                let reporting = match (&client.who, &client.report) {
                    (Served::Agent { author }, Some(_)) => Some(author.clone()),
                    _ => None,
                };
                leaving.extend(admit_into(
                    &repo,
                    &mut clients,
                    client,
                    folded_state.as_ref(),
                    &mut pending,
                    &hosted,
                ));
                if let Some(author) = reporting {
                    // Read afresh, as a newcomer's items are: the cached
                    // backlog may still hold a row acknowledged since.
                    let routes = recovery.check_in_routes();
                    leaving.extend(serve_reports(
                        &repo,
                        &mut clients,
                        folded_state.as_ref(),
                        Some(&author),
                        &pending,
                        &mut Default::default(),
                        &hosted,
                        &mut turns,
                        &routes,
                    ));
                }
            }
            // A source produced something. WHAT that author is owed is
            // decided from their logs, not from this item — except
            // when no log holds it, which is the one case the channel
            // is the only record of. Kept and served whether or not
            // this monitor still owns the repo: it is in its custody.
            Wake::Emitted(author, emission) => {
                if owner && roster_authors(&repo).contains(&author) {
                    owned.interrupting.notify(&repo, author.clone());
                }
                if matches!(emission.0, WaitItem::TimerEvent { .. })
                    && let Some(backlog) = timer_backlogs.get_mut(&author)
                {
                    backlog.invalidate();
                }
                pending.extend(
                    transient_of(emission)
                        .into_iter()
                        .map(|item| (author.clone(), item)),
                );
                if let Some(state) = &folded_state {
                    leaving.extend(serve_to_cached(
                        &repo,
                        &mut clients,
                        state,
                        Some(&author),
                        &mut pending,
                        &mut timer_backlogs,
                        &hosted,
                    ));
                }
                if owner && let Some(state) = &folded_state {
                    let routes = recovery.check_in_routes();
                    leaving.extend(serve_reports(
                        &repo,
                        &mut clients,
                        Some(state),
                        Some(&author),
                        &pending,
                        &mut timer_backlogs,
                        &hosted,
                        &mut turns,
                        &routes,
                    ));
                }
            }
            // A hosted producer moved: started, ended, backing off.
            Wake::Lifecycle(report) => {
                if record_lifecycle(&hosted, &mut lifecycles, report)
                    && owner
                    && let Some(birth) = &birth
                {
                    publish_sources(
                        &repo,
                        sources_account(&hosted, &lifecycles, birth.clone()),
                        &mut account,
                        &clients,
                    );
                }
            }
            Wake::Interrupted(author) => owned.interrupting.completed(&repo, author),
            Wake::Beat(open) => {
                if !open {
                    anyhow::bail!("the repo watcher stopped");
                }
                let events_changed = event_dirty.swap(false, std::sync::atomic::Ordering::AcqRel);
                let timers_changed = timer_dirty.swap(false, std::sync::atomic::Ordering::AcqRel);
                if timers_changed {
                    for backlog in timer_backlogs.values_mut() {
                        backlog.invalidate();
                    }
                }
                // Fold BEFORE claiming one. The counter means a fold
                // completed, so moving it first and then failing the
                // rebuild would show the watchdog progress that never
                // happened — the invariant this plan set for itself
                // (codex on 13e9fa7).
                match advance(&repo, fold, &heartbeat).await {
                    Some((next, state)) => {
                        fold = next;
                        // Looked at again: the fold was a long await.
                        let owner = owner && tenure.may_act(&republish);
                        if events_changed && owner {
                            for author in roster_authors(&repo) {
                                owned.interrupting.notify(&repo, author);
                            }
                        }
                        // `.clank/` is a wake dir, so an edited
                        // `wait_events` takes effect here rather than
                        // on a restart.
                        if owner {
                            owned.departing.extend(reconcile_hosting_reporting(
                                &repo,
                                &mut hosted,
                                &source_items,
                                timers_changed,
                                Some(&lifecycle_tx),
                            ));
                            forget_replaced(&hosted, &mut lifecycles);
                            if let Some(birth) = &birth {
                                publish_sources(
                                    &repo,
                                    sources_account(&hosted, &lifecycles, birth.clone()),
                                    &mut account,
                                    &clients,
                                );
                            }
                        }
                        // ONE presentation, carrying the projection,
                        // the durable rows and anything held from
                        // before there was a state to present it
                        // against.
                        leaving.extend(serve_to_cached(
                            &repo,
                            &mut clients,
                            &state,
                            None,
                            &mut pending,
                            &mut timer_backlogs,
                            &hosted,
                        ));
                        let routes = recovery.check_in_routes();
                        if owner {
                            leaving.extend(serve_reports(
                                &repo,
                                &mut clients,
                                Some(&state),
                                None,
                                &pending,
                                &mut timer_backlogs,
                                &hosted,
                                &mut turns,
                                &routes,
                            ));
                            let now = crate::cli::recovery::unix_now();
                            let mut acts = recovery.reconcile(
                                recovery_facts(
                                    &repo,
                                    &state,
                                    &clients,
                                    &hosted,
                                    &mut timer_backlogs,
                                    &pending,
                                    &mut turns,
                                    &routes,
                                ),
                                now,
                            );
                            act_on_recovery(
                                &repo,
                                &clients,
                                acts,
                                &recovery_answers,
                                &asks,
                                &records,
                                &mut owned,
                            );
                            identify_agents(&repo, &mut recovery, &identities, &mut owned);
                        }
                        folded_state = Some(state);
                    }
                    // The counter stays where it is, so the
                    // watchdog keeps counting this as a still look —
                    // which is how a repeatedly failing fold ends up
                    // standing the monitor down.
                    None => {
                        // Keep an observed ack/config invalidation until a fold
                        // can apply it; clearing dirty must not strand Pending.
                        if events_changed {
                            event_dirty.store(true, std::sync::atomic::Ordering::Release);
                        }
                        if timers_changed {
                            timer_dirty.store(true, std::sync::atomic::Ordering::Release);
                        }
                        eprintln!("monitor: fold failed, held at {fold}");
                    }
                }
            }
        }
        if let Some(changed) = uc_round
            && owner
        {
            if changed {
                user_commands.state_changed();
            }
            let routes = recovery.check_in_routes();
            leaving.extend(user_command_round(
                &repo,
                &mut user_commands,
                &mut clients,
                folded_state.as_ref(),
                &pending,
                &mut timer_backlogs,
                &hosted,
                &mut turns,
                &mut recovery,
                &routes,
                &recovery_answers,
                &asks,
                &records,
                &mut owned,
            ));
        }
    };
    let _ = std::fs::remove_file(observation_path(&repo, pid));
    ended
}

/// What woke the monitor's loop, received before anything is done
/// about it.
enum Wake {
    LeaseChanged,
    RecoveryDue,
    Authorize(Authorize),
    Identified(Identified),
    Answered(RecoveryAnswer),
    Records(std::collections::BTreeSet<crate::cli::recovery::RecordEvent>),
    TimersDue,
    Accepted(tokio::net::UnixStream),
    Arrived(Greeted),
    Emitted(AgentLabel, crate::cli::event_source::Emission),
    Lifecycle(crate::cli::event_source::LifecycleReport),
    Interrupted(AgentLabel),
    /// A held report's hook reached its deadline, or a check-in is due.
    TurnsDue,
    /// A reporting connection said something, or closed.
    Heard(u64, Heard),
    AuthorizeCheckIn(CheckInAsk),
    CheckedIn(CheckedIn),
    /// A task this loop owned has ended.
    Collected,
    /// Whether the watcher is still there.
    Beat(bool),
    /// A user-command record changed on disk.
    UserCommands,
    /// A waiting user command expires, or a released park has settled.
    UserCommandsDue,
    /// A user-command attempt asks for the loop's last look.
    UserCommandAsk(crate::cli::user_command::live::Ask),
    UserCommandDone(crate::cli::user_command::live::Done),
}

/// One evaluation of the repo's user commands against the loop's view of
/// its agents: release the parks that alone stand in a verified request's
/// way, have recovery read the records of agents a request waits on, and
/// spawn the attempts the pass hands out.
#[allow(clippy::too_many_arguments)]
fn user_command_round(
    repo: &std::path::Path,
    user_commands: &mut crate::cli::user_command::live::UserCommands,
    clients: &mut Vec<Client>,
    state: Option<&crate::repo_state::RepoState>,
    pending: &[(AgentLabel, WaitItem)],
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    turns: &mut crate::cli::turn_custody::Turns,
    recovery: &mut crate::cli::recovery::Driver,
    routes: &Routes,
    answers: &tokio::sync::mpsc::UnboundedSender<RecoveryAnswer>,
    asks: &tokio::sync::mpsc::UnboundedSender<Authorize>,
    records: &crate::cli::recovery::RecordWatch,
    owned: &mut OwnedTasks,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let settling = user_commands.settling(crate::cli::recovery::unix_now());
    let mut spawned = Vec::new();
    let out = {
        let mut agents = LoopAgents {
            repo,
            clients,
            turns,
            recovery,
            settling: &settling,
        };
        user_commands.pass(&mut agents, &mut spawned)
    };
    owned.departing.extend(spawned);
    let acts = recovery.want_records(&out.watch);
    act_on_recovery(repo, clients, acts, answers, asks, records, owned);
    turns.set_user_commands(out.release.clone());
    let mut leaving = Vec::new();
    for author in &out.release {
        leaving.extend(serve_reports(
            repo,
            clients,
            state,
            Some(author),
            pending,
            timer_backlogs,
            hosted,
            turns,
            routes,
        ));
    }
    leaving
}

/// The loop's agents, as a user-command pass sees them.
struct LoopAgents<'a> {
    repo: &'a std::path::Path,
    clients: &'a [Client],
    turns: &'a crate::cli::turn_custody::Turns,
    recovery: &'a mut crate::cli::recovery::Driver,
    settling: &'a std::collections::BTreeSet<AgentLabel>,
}

impl crate::cli::user_command::dispatch::Agents for LoopAgents<'_> {
    fn binding(&self, label: &AgentLabel) -> Option<(clank_core::Tool, clank_core::SessionId)> {
        crate::agent_store::load_agent_config(self.repo, label)
            .ok()
            .flatten()
            .and_then(|c| c.session)
            .map(|s| (s.tool, s.id))
    }

    fn look(
        &mut self,
        label: &AgentLabel,
        session: &clank_core::SessionId,
    ) -> crate::cli::user_command::dispatch::Look {
        user_command_look(
            self.repo,
            self.clients,
            self.turns,
            self.recovery,
            self.settling,
            label,
            session,
        )
    }
}

/// Whether `author` sits idle for a user command now: no question, block or
/// presentation pending, no hook of `session` held here, no released park
/// still settling, and its record's latest turn completed.
fn user_command_look(
    repo: &std::path::Path,
    clients: &[Client],
    turns: &crate::cli::turn_custody::Turns,
    recovery: &mut crate::cli::recovery::Driver,
    settling: &std::collections::BTreeSet<AgentLabel>,
    author: &AgentLabel,
    session: &clank_core::SessionId,
) -> crate::cli::user_command::dispatch::Look {
    use crate::cli::user_command::dispatch::Look;
    let dir = crate::agent_store::agents_root(repo).join(author.as_str());
    if crate::cli::question_state::asking(&dir, session.as_str()).is_some() {
        return Look::Wait("a question to the user is pending".into());
    }
    if crate::cli::wait::blocked(repo, author) {
        return Look::Wait("a block is pending".into());
    }
    if turns.presenting(author) {
        return Look::Wait("a wake is being presented to it".into());
    }
    let held = clients.iter().any(|c| {
        matches!(&c.who, Served::Agent { author: a } if a == author)
            && c.report
                .as_ref()
                .is_some_and(|r| r.report.session() == session)
    });
    if held {
        return Look::Parked;
    }
    if settling.contains(author) {
        return Look::Wait("its released turn is ending".into());
    }
    match recovery.turn_completed(author, session.as_str()) {
        Ok(()) => Look::Go,
        Err(why) => Look::Wait(why),
    }
}

/// What the monitor knows about each bound roster agent, for recovery.
///
/// Obligation is what a newly arriving waiter would be handed, derived as
/// a delivery would derive it but into a set no client owns. It is only
/// derived for an agent with no deliverer, which is the only kind that
/// can be stuck, or one whose repeat count an owed-work change could end.
fn recovery_facts(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    clients: &[Client],
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(AgentLabel, WaitItem)],
    turns: &mut crate::cli::turn_custody::Turns,
    routes: &Routes,
) -> Vec<crate::cli::recovery::Facts> {
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let blind_nudge = crate::cli::config::load_with_home(repo, home.as_deref())
        .recovery
        .blind_nudge;
    let mut facts = Vec::new();
    let mut bound = std::collections::BTreeSet::new();
    for author in roster_authors(repo) {
        let Ok(cfg) = crate::agent_store::load_agent_config(repo, &author) else {
            continue;
        };
        let dir = crate::agent_store::agents_root(repo).join(author.as_str());
        if crate::cli::question_state::path(&dir).exists() {
            // A question of a session no longer the agent's is over.
            crate::cli::question_state::observe(&dir, []);
        }
        let Some(session) = cfg.as_ref().and_then(|c| c.session.clone()) else {
            continue;
        };
        bound.insert(author.clone());
        let delivering = clients
            .iter()
            .any(|c| matches!(&c.who, Served::Agent { author: a } if a == &author));
        let auto = crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref())
            == clank_core::AutoMode::On;
        let counting = turns.counting(&author);
        let owed = if delivering && !counting {
            Vec::new()
        } else {
            owed_items(
                repo,
                state,
                &author,
                hosted,
                timer_backlogs,
                pending,
                &reviews,
                head.as_ref(),
            )
        };
        let obligated = !delivering && !owed.is_empty();
        let cwd = std::env::current_dir().ok();
        let g = crate::cli::turn_custody::Gathered::of(
            repo,
            &author,
            Some((&owed, cwd.as_deref())),
            route_of(routes, &author),
        );
        if counting {
            turns.observed(&author, &g);
        }
        let quiet = turns.recovery_quiet(&author, &g);
        let (attended, repeated) = turns.projection(&author, &g);
        let asking = g.asking().map(str::to_string);
        let role = crate::agent_store::resolve_role(repo, &author)
            .map(|r| r.to_string())
            .unwrap_or_default();
        facts.push(crate::cli::recovery::Facts {
            agent_dir: dir,
            work: identities(&owed),
            identify: turns.attending(&author),
            author,
            session,
            auto,
            delivering,
            obligated,
            role,
            blind_nudge,
            attending: obligated && quiet.is_some(),
            asking: asking.is_some(),
            projection: crate::cli::recovery::Projection {
                attended,
                asking,
                repeated,
            },
        });
    }
    turns.retire_unbound(&bound);
    facts
}

/// Each item's stable identity, as a delivery names it: never its
/// rendering, age or repeat count.
fn identities(owed: &[WaitItem]) -> Vec<String> {
    crate::cli::wake_render::mirror(owed, None)
        .iter()
        .map(crate::cli::wake_render::WaitItem::identity)
        .collect()
}

/// Everything `author` is owed: the repo's work, its sources' unshown rows
/// and what `pending` holds for it from a source no log keeps, all through
/// the one block-aware selection a waiting client is served by.
#[allow(clippy::too_many_arguments)]
fn owed_items(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    author: &AgentLabel,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(AgentLabel, WaitItem)],
    reviews: &crate::fs_plan_state_lookup::FsPlanStateLookup,
    head: Option<&clank_core::wait::HeadCommit>,
) -> Vec<WaitItem> {
    let Ok(inputs) = crate::cli::wait::derive_wait_inputs(repo, author) else {
        return Vec::new();
    };
    let status = state.fold.derive_status(reviews, &inputs.work_policy, head);
    let current;
    let sources = match hosted.get(author) {
        Some(host) => &host.spec,
        None => {
            current = hostable(repo, author).unwrap_or_default();
            &current
        }
    };
    let mut shown = Default::default();
    let mut external =
        crate::cli::event_source::owed_from_sources(repo, author, sources, &mut shown);
    external.extend(
        timer_backlogs
            .entry(author.clone())
            .or_insert_with(|| {
                crate::cli::timer_source::Backlog::new(
                    crate::agent_store::agents_root(repo)
                        .join(author.as_str())
                        .join("events"),
                    Default::default(),
                )
            })
            .owed(&mut shown),
    );
    external.extend(
        pending
            .iter()
            .filter(|(whose, _)| whose == author)
            .map(|(_, item)| item.clone()),
    );
    crate::cli::wait::owed_for(repo, author, &inputs, &status, &state.fold, &mut external)
}

/// Publish what the monitor holds of `author`'s turns: its attendance,
/// question and repeat count.
fn project(
    recovery: &mut crate::cli::recovery::Driver,
    turns: &mut crate::cli::turn_custody::Turns,
    author: &AgentLabel,
    g: &crate::cli::turn_custody::Gathered,
) {
    let (attended, repeated) = turns.projection(author, g);
    recovery.project(
        author,
        crate::cli::recovery::Projection {
            attended,
            asking: g.asking().map(str::to_string),
            repeated,
        },
        crate::cli::recovery::unix_now(),
    );
}

/// A transport's answer, and the attempt it answers.
type RecoveryAnswer = (
    AgentLabel,
    crate::cli::recovery::AttemptRef,
    crate::cli::recovery::Answer,
);

/// [`recovery_gate`] for an attempt falling due, against the work the
/// latest fold says is owed. Recovery reconciles only after a fold, so with
/// none yet there is nothing it may act on.
#[allow(clippy::too_many_arguments)]
fn due_gate(
    repo: &std::path::Path,
    clients: &[Client],
    turns: &mut crate::cli::turn_custody::Turns,
    routes: &Routes,
    state: Option<&crate::repo_state::RepoState>,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(AgentLabel, WaitItem)],
    author: &AgentLabel,
    session: &str,
) -> crate::cli::recovery::Gate {
    let Some(state) = state else {
        return crate::cli::recovery::Gate::Hold;
    };
    let owed = owed_now(repo, state, author, hosted, timer_backlogs, pending);
    recovery_gate(repo, clients, turns, routes, author, session, &owed)
}

/// Whether a due recovery attempt may go ahead now, from the agent as it
/// is at this moment rather than at the last fold: never over a pending
/// question or an attendance that quiets all of `owed`, and never once a
/// deliverer, a new binding or auto mode turned off has made it someone
/// else's call. `owed` is what the agent is owed now, never a stand-in.
fn recovery_gate(
    repo: &std::path::Path,
    clients: &[Client],
    turns: &mut crate::cli::turn_custody::Turns,
    routes: &Routes,
    author: &AgentLabel,
    session: &str,
    owed: &[WaitItem],
) -> crate::cli::recovery::Gate {
    use crate::cli::recovery::Gate;
    let Ok(Some(cfg)) = crate::agent_store::load_agent_config(repo, author) else {
        return Gate::SessionChanged;
    };
    if cfg
        .session
        .as_ref()
        .is_none_or(|s| s.id.as_str() != session)
    {
        return Gate::SessionChanged;
    }
    if clients
        .iter()
        .any(|c| matches!(&c.who, Served::Agent { author: a } if a == author))
    {
        return Gate::Deliverer;
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if crate::cli::team::resolve_effective_auto_mode(Some(&cfg), home.as_deref())
        != clank_core::AutoMode::On
    {
        return Gate::AutoOff;
    }
    let cwd = std::env::current_dir().ok();
    let g = crate::cli::turn_custody::Gathered::of(
        repo,
        author,
        Some((owed, cwd.as_deref())),
        route_of(routes, author),
    );
    match turns.recovery_quiet(author, &g) {
        Some(_) => Gate::Hold,
        None => Gate::Go,
    }
}

/// A transport, its preparation done off the loop, asking the loop that
/// owns the live state whether it may make its one mutation now.
#[derive(Debug)]
struct Authorize {
    author: AgentLabel,
    attempt: crate::cli::recovery::AttemptRef,
    /// The harness and its pane's screen as just read, for a send typed
    /// into a pane.
    screen: Option<(clank_core::Tool, Option<String>)>,
    /// The provider process found in that pane.
    process: Option<crate::cli::pane_target::Process>,
    /// The transport that will make the mutation.
    transport: crate::cli::recovery::Transport,
    /// The repo folded afresh for this send, so the last look can derive
    /// the agent's work from disk as it is then.
    state: Option<crate::repo_state::RepoState>,
    reply: tokio::sync::oneshot::Sender<bool>,
}

/// Fold the repo afresh, then ask the loop whether `attempt` may be sent
/// now. Refused when the loop is gone or does not answer.
async fn authorized(
    asks: &tokio::sync::mpsc::UnboundedSender<Authorize>,
    repo: &std::path::Path,
    author: AgentLabel,
    attempt: crate::cli::recovery::AttemptRef,
    screen: Option<(clank_core::Tool, Option<String>)>,
    process: Option<crate::cli::pane_target::Process>,
    transport: crate::cli::recovery::Transport,
) -> bool {
    let state = folded(repo).await.ok();
    let (reply, answer) = tokio::sync::oneshot::channel();
    let asked = asks.send(Authorize {
        author,
        attempt,
        screen,
        process,
        transport,
        state,
        reply,
    });
    asked.is_ok() && answer.await == Ok(true)
}

/// Answer a transport's request with the loop's last look.
fn authorize(
    repo: &std::path::Path,
    clients: &[Client],
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    pending: &[(AgentLabel, WaitItem)],
    recovery: &mut crate::cli::recovery::Driver,
    turns: &mut crate::cli::turn_custody::Turns,
    a: Authorize,
) {
    let allowed = may_send_now(repo, clients, hosted, pending, recovery, turns, &a);
    let _ = a.reply.send(allowed);
}

/// The last look before any transport's mutation, made on the loop that
/// owns the live state, so a change already on disk vetoes the send
/// whether or not a fold has seen it: the same gate a deadline asks
/// (binding, deliverer, auto mode, a question, block or attendance); for a
/// typed send, the input read as empty; and then the driver's own view,
/// given the role on the roster and the work derived from the repo as
/// freshly folded for this send, with the record read afresh nearest the
/// mutation. Not atomic: what changes after it is the race the capability
/// evidence must accept.
fn may_send_now(
    repo: &std::path::Path,
    clients: &[Client],
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    pending: &[(AgentLabel, WaitItem)],
    recovery: &mut crate::cli::recovery::Driver,
    turns: &mut crate::cli::turn_custody::Turns,
    a: &Authorize,
) -> bool {
    let routes = recovery.check_in_routes();
    let Some((crate::cli::recovery::Gate::Go, owed)) =
        last_look(repo, clients, hosted, pending, turns, &routes, a)
    else {
        return false;
    };
    if let Some((tool, screen)) = &a.screen
        && screen
            .as_deref()
            .map(|s| crate::cli::composer::composer(*tool, s))
            != Some(crate::cli::composer::Composer::Empty)
    {
        return false;
    }
    let Ok(role) = crate::agent_store::resolve_role(repo, &a.author) else {
        return false;
    };
    let live = crate::cli::recovery::Live {
        role: role.to_string(),
        work: identities(&owed),
        bound: crate::agent_store::load_agent_config(repo, &a.author)
            .ok()
            .flatten()
            .and_then(|c| c.session)
            .as_ref()
            .map(crate::cli::recovery::Bound::of),
        transport: a.transport,
        process: a.process.clone(),
    };
    recovery.may_send(&a.author, &a.attempt, &live)
}

/// The deadline's gate asked again at the last look, against the work
/// derived from the repo as folded for this send, and that work. `None`
/// when the send carries no fold.
fn last_look(
    repo: &std::path::Path,
    clients: &[Client],
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    pending: &[(AgentLabel, WaitItem)],
    turns: &mut crate::cli::turn_custody::Turns,
    routes: &Routes,
    a: &Authorize,
) -> Option<(crate::cli::recovery::Gate, Vec<WaitItem>)> {
    let state = a.state.as_ref()?;
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);
    // Read afresh, never the loop's cached backlog: a recurring occurrence
    // acknowledged since its last invalidation is still in that cache.
    let backlog = crate::cli::timer_source::Backlog::new(
        crate::agent_store::agents_root(repo)
            .join(a.author.as_str())
            .join("events"),
        Default::default(),
    )
    .inspection();
    let owed = owed_items(
        repo,
        state,
        &a.author,
        hosted,
        &mut [(a.author.clone(), backlog)].into_iter().collect(),
        pending,
        &reviews,
        head.as_ref(),
    );
    let gate = recovery_gate(
        repo,
        clients,
        turns,
        routes,
        &a.author,
        &a.attempt.session,
        &owed,
    );
    Some((gate, owed))
}

/// A check-in's typing, prepared off the loop, asking the loop whether it
/// may type now.
struct CheckInAsk {
    author: AgentLabel,
    session: clank_core::SessionId,
    /// The attendance the check-in was fired for.
    check_in: clank_core::turn_policy::CheckInRef,
    /// The harness and its pane's screen as just read.
    screen: (clank_core::Tool, Option<String>),
    /// The provider process found in that pane.
    process: crate::cli::pane_target::Process,
    reply: tokio::sync::oneshot::Sender<Result<(), String>>,
}

/// How a check-in's one typing went.
struct CheckedIn {
    author: AgentLabel,
    check_in: clank_core::turn_policy::CheckInRef,
    outcome: Result<(), String>,
}

/// An agent's pane, the harness and process running in it, and the pane's
/// screen as read when it was found.
pub(crate) type Located = (
    crate::cli::pane_target::Found,
    clank_core::Tool,
    crate::cli::pane_target::Process,
    Option<String>,
);

/// Where a check-in is typed: the agent's pane.
pub(crate) trait Typist: Send + Sync + 'static {
    /// The agent's pane, the harness and process running in it, and the
    /// pane's screen as read now.
    fn locate(
        &self,
        repo: std::path::PathBuf,
        author: AgentLabel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Located, String>> + Send>>;

    /// Type `text` into the pane and submit it.
    fn submit(
        &self,
        found: crate::cli::pane_target::Found,
        tool: clank_core::Tool,
        text: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;
}

/// The agent's zellij pane, as recovery's typed sends reach it.
struct Panes;

impl Typist for Panes {
    fn locate(
        &self,
        repo: std::path::PathBuf,
        author: AgentLabel,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Located, String>> + Send>> {
        Box::pin(async move {
            let (found, tool, process) = crate::cli::pane_target::discover_process(repo, author)
                .await
                .map_err(|e| format!("its pane was not found: {e:#}"))?;
            let screen =
                crate::cli::open_zellij::dump_pane_screen(&found.session, &found.pane).await;
            Ok((found, tool, process, screen))
        })
    }

    fn submit(
        &self,
        found: crate::cli::pane_target::Found,
        tool: clank_core::Tool,
        text: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
        Box::pin(async move {
            let target = crate::cli::pane_input::Target {
                session: &found.session,
                pane: &found.pane,
                tool,
                version: None,
            };
            let text = crate::cli::pane_input::Admitted::new(tool, None, &text);
            crate::cli::pane_input::submit_async(target, &text)
                .await
                .map_err(|e| match e {
                    crate::cli::open_zellij::PaneWrite::NotStarted(why) => {
                        format!("typing it did not start: {why}")
                    }
                    crate::cli::open_zellij::PaneWrite::Unknown(why) => {
                        format!("typing it may not have finished: {why}")
                    }
                })
        })
    }
}

/// Type one check-in through the agent's resume route: find its pane,
/// ask the loop for its last look, then submit. Any failure is reported,
/// never retried: the check-in is spent either way.
async fn deliver_check_in(
    repo: std::path::PathBuf,
    due: crate::cli::turn_custody::CheckInDue,
    typist: std::sync::Arc<dyn Typist>,
    asks: tokio::sync::mpsc::UnboundedSender<CheckInAsk>,
    done: tokio::sync::mpsc::UnboundedSender<CheckedIn>,
) {
    let outcome = async {
        let (found, tool, process, screen) = typist.locate(repo, due.author.clone()).await?;
        let (reply, answer) = tokio::sync::oneshot::channel();
        asks.send(CheckInAsk {
            author: due.author.clone(),
            session: due.session.clone(),
            check_in: due.check_in.clone(),
            screen: (tool, screen),
            process,
            reply,
        })
        .map_err(|_| "the monitor is gone".to_string())?;
        match answer.await {
            Ok(Ok(())) => typist.submit(found, tool, due.text.clone()).await,
            Ok(Err(why)) => Err(format!("its last look refused it: {why}")),
            Err(_) => Err("the monitor is gone".to_string()),
        }
    }
    .await;
    let _ = done.send(CheckedIn {
        author: due.author,
        check_in: due.check_in,
        outcome,
    });
}

/// The last look before a check-in is typed: the session still bound, the
/// route still granted for the very process recovery identified, its
/// input line empty, and, by `g` as gathered now, the attendance still the
/// one the check-in was fired for with every wake gate open.
fn may_check_in_now(
    repo: &std::path::Path,
    recovery: &crate::cli::recovery::Driver,
    turns: &mut crate::cli::turn_custody::Turns,
    g: &crate::cli::turn_custody::Gathered,
    ask: &CheckInAsk,
) -> Result<(), String> {
    let (tool, screen) = &ask.screen;
    if !crate::agent_store::binding_matches(repo, &ask.author, *tool, ask.session.as_str()) {
        return Err("the session no longer holds the label".into());
    }
    if !recovery.may_type_check_in(&ask.author, &ask.process) {
        return Err("its pane runs no harness identified as granted a typed resume".into());
    }
    if screen
        .as_deref()
        .map(|s| crate::cli::composer::composer(*tool, s))
        != Some(crate::cli::composer::Composer::Empty)
    {
        return Err("its input line is not empty".into());
    }
    turns.may_type_check_in(&ask.author, &ask.check_in, g)
}

/// Step every check-in due by `now`, gathering each agent as it is
/// evaluated: what was gathered, and the check-in to type, if one is owed.
/// Each due attendance resolves, or is retired with its binding, so none
/// is due again.
fn due_check_ins(
    turns: &mut crate::cli::turn_custody::Turns,
    now: u64,
    mut gathered: impl FnMut(&AgentLabel) -> crate::cli::turn_custody::Gathered,
) -> Vec<(
    AgentLabel,
    crate::cli::turn_custody::Gathered,
    Option<crate::cli::turn_custody::CheckInDue>,
)> {
    turns
        .due_authors(now)
        .into_iter()
        .map(|author| {
            let g = gathered(&author);
            let due = turns.check_in(&author, &g);
            (author, g, due)
        })
        .collect()
}

/// Settle a result from connection `conn` against `author`'s custody. A
/// delivery discharges the transient items it presented; anything else
/// leaves them held, owed to the next presentation.
fn settle_result(
    turns: &mut crate::cli::turn_custody::Turns,
    pending: &mut Vec<(AgentLabel, WaitItem)>,
    author: &AgentLabel,
    conn: u64,
    result: crate::cli::turn_wire::TurnResult,
    gathered: impl FnOnce(&[(AgentLabel, WaitItem)]) -> crate::cli::turn_custody::Gathered,
) -> Result<Option<crate::cli::turn_custody::Commit>, String> {
    let g = gathered(pending);
    let commit = turns.result(author, conn, result, &g)?;
    if let Some(commit) = &commit {
        pending.retain(|(whose, item)| {
            let ids = identities(std::slice::from_ref(item));
            let discharged =
                !ids.is_empty() && ids.iter().all(|id| commit.presented().contains(id));
            whose != author || !discharged
        });
    }
    Ok(commit)
}

/// What identifying an agent's running harness found, for its binding.
type Identified = (
    AgentLabel,
    clank_core::agent_config::Session,
    Result<crate::cli::harness_identity::Running, String>,
);

/// Identify, off the loop, each agent whose running harness recovery has not
/// asked about yet.
fn identify_agents(
    repo: &std::path::Path,
    recovery: &mut crate::cli::recovery::Driver,
    found: &tokio::sync::mpsc::UnboundedSender<Identified>,
    owned: &mut OwnedTasks,
) {
    for (author, session) in recovery.unidentified() {
        let repo = repo.to_path_buf();
        let found = found.clone();
        owned.departing.push(tokio::spawn(async move {
            let running = crate::cli::harness_identity::identify(repo, author.clone()).await;
            let _ = found.send((author, session, running));
        }));
    }
}

/// Carry out what recovery decided.
fn act_on_recovery(
    repo: &std::path::Path,
    clients: &[Client],
    acts: Vec<crate::cli::recovery::Act>,
    answers: &tokio::sync::mpsc::UnboundedSender<RecoveryAnswer>,
    asks: &tokio::sync::mpsc::UnboundedSender<Authorize>,
    records: &crate::cli::recovery::RecordWatch,
    owned: &mut OwnedTasks,
) {
    use crate::cli::recovery::{Act, Answer, Transport};
    for act in acts {
        match act {
            Act::Watch(path) => records.watch(path),
            Act::Unwatch(path) => records.unwatch(path),
            Act::Dispatch {
                author,
                attempt,
                transport: Transport::Helper,
                tag,
                text,
                ..
            } => {
                let helper = clients.iter().find(|c| {
                    matches!(&c.who, Served::Helper { author: a } if a == &author)
                        && c.holder
                            .as_ref()
                            .is_some_and(|h| h.session == attempt.session)
                });
                let sent = helper.is_some_and(|c| {
                    send_note(
                        c,
                        &Note::Recover {
                            opened_by: attempt.opened_by.clone(),
                            n: attempt.n,
                            wake: crate::cli::pane_input::Admitted::new(
                                clank_core::Tool::OpenCode,
                                None,
                                &text,
                            )
                            .as_str()
                            .to_string(),
                            tag,
                        },
                    )
                });
                if !sent {
                    let _ = answers.send((author, attempt, Answer::Unavailable));
                }
            }
            Act::Dispatch {
                author,
                attempt,
                transport: Transport::Typed,
                text,
                ..
            } => {
                let repo = repo.to_path_buf();
                let answers = answers.clone();
                let asks = asks.clone();
                owned.departing.push(tokio::spawn(async move {
                    let answer = match crate::cli::pane_target::discover_process(
                        repo.clone(),
                        author.clone(),
                    )
                    .await
                    {
                        Ok((found, tool, process)) => {
                            let version = process.harness.clone().ok();
                            let screen = crate::cli::open_zellij::dump_pane_screen(
                                &found.session,
                                &found.pane,
                            )
                            .await;
                            let go = authorized(
                                &asks,
                                &repo,
                                author.clone(),
                                attempt.clone(),
                                Some((tool, screen)),
                                Some(process),
                                Transport::Typed,
                            )
                            .await;
                            match go {
                                false => Answer::Unavailable,
                                true => match crate::cli::pane_input::submit_async(
                                    crate::cli::pane_input::Target {
                                        session: &found.session,
                                        pane: &found.pane,
                                        tool,
                                        version: version.as_deref(),
                                    },
                                    &crate::cli::pane_input::Admitted::new(
                                        tool,
                                        version.as_deref(),
                                        &text,
                                    ),
                                )
                                .await
                                {
                                    Ok(()) => Answer::Submitted { started: None },
                                    Err(crate::cli::open_zellij::PaneWrite::NotStarted(_)) => {
                                        Answer::Unavailable
                                    }
                                    Err(crate::cli::open_zellij::PaneWrite::Unknown(_)) => {
                                        Answer::Indeterminate
                                    }
                                },
                            }
                        }
                        Err(_) => Answer::Unavailable,
                    };
                    let _ = answers.send((author, attempt, answer));
                }));
            }
            Act::Dispatch {
                author,
                attempt,
                transport: Transport::CodexStart,
                target,
                text,
                ..
            } => {
                let repo = repo.to_path_buf();
                let answers = answers.clone();
                let asks = asks.clone();
                owned.departing.push(tokio::spawn(async move {
                    let go = authorized(
                        &asks,
                        &repo,
                        author.clone(),
                        attempt.clone(),
                        None,
                        None,
                        Transport::CodexStart,
                    );
                    let text =
                        crate::cli::pane_input::Admitted::new(clank_core::Tool::Codex, None, &text);
                    let answer = crate::cli::codex_interrupt::start_if_failed(
                        &repo, &author, &target, &text, go,
                    )
                    .await;
                    let _ = answers.send((author, attempt, answer));
                }));
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
///
/// It also looks at the lease's path, and only looks: a change it sees
/// is flagged for the loop, which alone re-takes or retires.
fn watch_the_fold(
    tick: std::sync::Arc<std::sync::atomic::AtomicU64>,
    lease: std::sync::Arc<LeaseWatch>,
    every: Duration,
    limit: u32,
) {
    std::thread::spawn(move || {
        let mut seen = tick.load(std::sync::atomic::Ordering::Acquire);
        let mut missed = 0;
        loop {
            std::thread::sleep(every);
            let now = tick.load(std::sync::atomic::Ordering::Acquire);
            let retiring = lease.retiring(unix_ms(), every * limit);
            match watchdog_look(seen, now, missed, limit, retiring) {
                Watch::Keep { tick, missed: m } => {
                    seen = tick;
                    missed = m;
                }
                Watch::StandDown => {
                    let why = match retiring {
                        Retiring::Overdue => "its retirement overran its deadline".to_string(),
                        _ => format!(
                            "the fold has not advanced past {seen} in {:?}",
                            every * limit
                        ),
                    };
                    eprintln!(
                        "monitor: lease {} ({}) at {}: forced: {why}; producers are cut off, \
                         not drained",
                        lease.path.display(),
                        lease.held(),
                        timestamp(),
                    );
                    crate::cli::source_group::exit(STOOD_DOWN_EXIT);
                }
            }
            if retiring == Retiring::No {
                lease.observe();
            }
        }
    });
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "an unknown time".into())
}

/// What the loop and the watchdog share about the lease. The watchdog
/// only looks, and wakes the loop when what it saw changed; the loop
/// alone acts, after its own look.
pub(crate) struct LeaseWatch {
    path: std::path::PathBuf,
    held: std::sync::Mutex<FileId>,
    wake: tokio::sync::Notify,
    /// Unix millis a retirement under way must be done by; 0 when none is.
    retiring_until: std::sync::atomic::AtomicU64,
}

impl LeaseWatch {
    fn new(lease: &MonitorLease) -> std::sync::Arc<Self> {
        std::sync::Arc::new(LeaseWatch {
            path: lease.path().to_path_buf(),
            held: std::sync::Mutex::new(lease.held()),
            wake: tokio::sync::Notify::new(),
            retiring_until: Default::default(),
        })
    }

    fn held(&self) -> FileId {
        *self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The watchdog's look: read-only. Wakes the loop when the path no
    /// longer holds the file locked; it is the loop's look that decides.
    fn observe(&self) {
        if found_at(&self.path, self.held()) != Found::Held {
            self.wake.notify_one();
        }
    }

    fn begin_retiring(&self, within: Duration) {
        self.retiring_until.store(
            unix_ms() + within.as_millis() as u64,
            std::sync::atomic::Ordering::Release,
        );
    }

    fn retiring(&self, now_ms: u64, margin: Duration) -> Retiring {
        match self
            .retiring_until
            .load(std::sync::atomic::Ordering::Acquire)
        {
            0 => Retiring::No,
            until if now_ms <= until + margin.as_millis() as u64 => Retiring::Within,
            _ => Retiring::Overdue,
        }
    }
}

/// One look at the lease that found it lost, and what was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Detection {
    path: std::path::PathBuf,
    held: FileId,
    found: Found,
    at: String,
    done: String,
}

impl std::fmt::Display for Detection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "lease {} held {}, path {}, at {}: {}",
            self.path.display(),
            self.held,
            self.found,
            self.at,
            self.done
        )
    }
}

/// What settling the lease decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Settled {
    Owner,
    Retaken(Detection),
    Retire(Detection),
    /// A different build is installed: retire, and never repair or
    /// advertise.
    Stale {
        running: crate::build_stamp::Stamp,
        installed: crate::build_stamp::Stamp,
    },
}

/// A monitor's view of its own build against the installation.
pub(crate) struct Built {
    installation: crate::build_stamp::Installation,
    running: crate::build_stamp::Stamp,
    shown: Option<crate::build_stamp::Install>,
    /// Records a changed state where status reads it.
    note: Box<dyn Fn(&crate::build_stamp::Install) + Send>,
}

impl Built {
    pub(crate) fn new(
        installation: crate::build_stamp::Installation,
        running: crate::build_stamp::Stamp,
        note: Box<dyn Fn(&crate::build_stamp::Install) + Send>,
    ) -> Self {
        Built {
            installation,
            running,
            shown: None,
            note,
        }
    }

    /// Look at the installation, noting and logging only a change.
    fn look(&mut self) -> crate::build_stamp::Install {
        let install = self.installation.look(self.running);
        if self.shown.as_ref() != Some(&install) {
            eprintln!(
                "monitor: installation {} at {}: {install:?} (running {})",
                self.installation.path().display(),
                timestamp(),
                self.running
            );
            (self.note)(&install);
            self.shown = Some(install.clone());
        }
        install
    }
}

/// The loop's hold on the repo, and the only code that re-takes the
/// lease, publishes for it or gives it up.
pub(crate) struct Tenure {
    lease: MonitorLease,
    watch: std::sync::Arc<LeaseWatch>,
    retiring: bool,
    build: Option<Built>,
}

impl Tenure {
    pub(crate) fn new(lease: MonitorLease) -> Self {
        let watch = LeaseWatch::new(&lease);
        Tenure {
            lease,
            watch,
            retiring: false,
            build: None,
        }
    }

    pub(crate) fn with_build(self, build: Built) -> Self {
        Tenure {
            build: Some(build),
            ..self
        }
    }

    /// Look at the lease and settle a loss: re-taken, verified and only
    /// then republished by `publish`, or given up for retirement. Once
    /// given up, it stays given up.
    pub(crate) fn settle(&mut self, publish: &dyn Fn() -> Result<(), String>) -> Settled {
        if self.retiring {
            return Settled::Retire(self.given_up());
        }
        // The build first: a stale build retires before the lease could
        // be repaired or the descriptor republished.
        if let Some(build) = &mut self.build
            && let crate::build_stamp::Install::Replaced(installed) = build.look()
        {
            self.retiring = true;
            return Settled::Stale {
                running: build.running,
                installed,
            };
        }
        let found = self.lease.check();
        if found == Found::Held {
            return Settled::Owner;
        }
        let (path, held) = (self.lease.path().to_path_buf(), self.lease.held());
        let detected = |done: String| Detection {
            path,
            held,
            found,
            at: timestamp(),
            done,
        };
        let retaken = self.lease.retake().map_err(|e| e.to_string());
        let republished = retaken.and_then(|lease| {
            *self
                .watch
                .held
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = lease.held();
            let now = lease.held();
            self.lease = lease;
            publish().map(|()| now)
        });
        let settled = match republished {
            Ok(now) => Settled::Retaken(detected(format!("re-taken as {now} and republished"))),
            Err(why) => {
                self.retiring = true;
                Settled::Retire(detected(format!("retiring: {why}")))
            }
        };
        if let Settled::Retaken(d) | Settled::Retire(d) = &settled {
            eprintln!("monitor: {d}");
        }
        settled
    }

    /// Whether what a wake leads to may run as owner: the lease looked
    /// at now, and a loss settled first.
    pub(crate) fn may_act(&mut self, publish: &dyn Fn() -> Result<(), String>) -> bool {
        matches!(self.settle(publish), Settled::Owner | Settled::Retaken(_))
    }

    pub(crate) fn retiring(&self) -> bool {
        self.retiring
    }

    fn given_up(&self) -> Detection {
        Detection {
            path: self.lease.path().to_path_buf(),
            held: self.lease.held(),
            found: self.lease.check(),
            at: timestamp(),
            done: "already retiring".into(),
        }
    }
}

/// The sink `author`'s hosted sources emit into, reporting their
/// lifecycle when the monitor publishes it.
fn host_sink(
    into: &tokio::sync::mpsc::UnboundedSender<(AgentLabel, crate::cli::event_source::Emission)>,
    lifecycle: Option<&LifecycleTx>,
    author: &AgentLabel,
    instance: u64,
) -> crate::cli::event_source::EmissionSink {
    let sink = crate::cli::event_source::EmissionSink::for_author(into.clone(), author.clone());
    match lifecycle {
        Some(tx) => sink.reporting(tx.clone(), author.clone(), instance),
        None => sink,
    }
}

/// The account of what the monitor hosts, from `hosted` and the last
/// lifecycle each running source reported.
fn sources_account(
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    lifecycles: &Lifecycles,
    birth: crate::proc_identity::ProcToken,
) -> crate::cli::source_status::Account {
    use crate::cli::source_status::{ConfiguredSource, Hosted as Entry, fingerprint};
    let sources = hosted
        .iter()
        .flat_map(|(author, host)| {
            host.spec.iter().map(move |(config, _)| {
                let key = ConfiguredSource::of(author, config).key;
                Entry {
                    agent: author.to_string(),
                    lifecycle: lifecycles
                        .get(&(author.clone(), key.clone()))
                        .filter(|(instance, _)| current(hosted, author, &key, *instance))
                        .map(|(_, state)| state.clone()),
                    key,
                    accepted: fingerprint(config),
                }
            })
        })
        .collect();
    crate::cli::source_status::Account {
        monitor: std::process::id() as i32,
        birth,
        sources,
    }
}

/// Publish the account when it says something new, and wake the
/// observers that show it. A rewrite of the same account would refresh
/// every panel for nothing.
fn publish_sources(
    repo: &std::path::Path,
    account: crate::cli::source_status::Account,
    last: &mut Option<crate::cli::source_status::Account>,
    clients: &[Client],
) {
    if last.as_ref() == Some(&account) {
        return;
    }
    match crate::cli::source_status::publish(repo, &account) {
        Ok(()) => {
            *last = Some(account);
            for client in clients.iter().filter(|c| c.who == Served::Observer) {
                send_note(client, &Note::Beat);
            }
        }
        Err(e) => eprintln!("monitor: could not publish its sources: {e}"),
    }
}

/// Restart one of an author's command sources for a control client,
/// if what runs for it is still the configuration the asker was shown.
/// Only that source's task is stopped, as a config change stops it: its
/// producer is given its grace, and the replacement cannot start a run
/// until that group is gone and its lease free.
async fn restart_source(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<AgentLabel, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(AgentLabel, crate::cli::event_source::Emission)>,
    lifecycle: &LifecycleTx,
    departing: &mut futures_util::stream::FuturesUnordered<tokio::task::JoinHandle<()>>,
    request: &Control,
) -> Result<String, String> {
    use clank_core::agent_config::WaitEventSource;
    let Control::Restart {
        author,
        key,
        accepted,
    } = request
    else {
        return Err("not a restart".into());
    };
    let author =
        AgentLabel::parse(author).map_err(|e| format!("invalid author `{author}`: {e}"))?;
    let host = hosted
        .get_mut(&author)
        .ok_or_else(|| format!("the monitor runs no sources for `{author}`"))?;
    let config = host
        .spec
        .iter()
        .map(|(config, _)| config)
        .find(|config| matches!(config, WaitEventSource::Command(c) if c.source_key() == *key))
        .cloned()
        .ok_or_else(|| format!("`{author}` has no command source `{key}` running"))?;
    if crate::cli::source_status::fingerprint(&config) != *accepted {
        return Err(
            "what runs for this source changed since it was shown; reopen its page".to_string(),
        );
    }
    let old = host
        .tasks
        .remove(key)
        .ok_or_else(|| format!("`{author}`'s `{key}` has no running task"))?;
    departing.push(old.stop());
    let events_dir = crate::agent_store::agents_root(repo)
        .join(author.as_str())
        .join("events");
    host.start(
        &author,
        &config,
        crate::cli::event_source::SourceOrigin::Config,
        &events_dir,
        &Starts::hosting(into, Some(lifecycle)),
    );
    Ok(format!("restarted `{author}`'s `{key}`"))
}

/// One author's config sources, running in the monitor.
#[derive(Default)]
struct Hosted {
    /// The definitions what runs was started from: each task's and the
    /// timers'. A source is compared against its own entry here, so an
    /// edit restarts only the source it changed.
    spec: Vec<(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )>,
    /// Each source's task, by the storage key of the log it writes —
    /// the identity its durable state is filed under, so it outlives an
    /// edit to anything but the source's name.
    tasks: std::collections::BTreeMap<String, HostedTask>,
    timers: Option<crate::cli::timer_source::TimerSet>,
}

/// Starts a source's task under the storage key of the log it writes.
type Spawn = dyn Fn(
    &clank_core::agent_config::WaitEventSource,
    crate::cli::event_source::SourceOrigin,
    &std::path::Path,
    crate::cli::event_source::EmissionSink,
) -> Option<crate::cli::wait::SourceTask>;

/// What a host starts its sources' tasks with.
struct Starts<'a> {
    into: &'a tokio::sync::mpsc::UnboundedSender<(AgentLabel, crate::cli::event_source::Emission)>,
    lifecycle: Option<&'a LifecycleTx>,
    spawn: &'a Spawn,
}

impl<'a> Starts<'a> {
    fn hosting(
        into: &'a tokio::sync::mpsc::UnboundedSender<(
            AgentLabel,
            crate::cli::event_source::Emission,
        )>,
        lifecycle: Option<&'a LifecycleTx>,
    ) -> Self {
        Self {
            into,
            lifecycle,
            spawn: &spawn_hosted,
        }
    }
}

/// No invocation scope: `hostable` resolves no CLI items, so every
/// source a monitor hosts is an author's config source.
fn spawn_hosted(
    source: &clank_core::agent_config::WaitEventSource,
    origin: crate::cli::event_source::SourceOrigin,
    events_dir: &std::path::Path,
    sink: crate::cli::event_source::EmissionSink,
) -> Option<crate::cli::wait::SourceTask> {
    crate::cli::wait::spawn_source(source, origin, Some(events_dir), "", sink, true)
}

/// One hosted source's task, and which instance of it this is.
struct HostedTask {
    instance: u64,
    handle: tokio::task::JoinHandle<()>,
    /// Asks a command source to end within its grace. A task without
    /// one is cancelled.
    stop: Option<tokio::sync::watch::Sender<bool>>,
}

impl HostedTask {
    /// Begin ending this task, and hand back its handle to observe the
    /// end: a command source drains its producer first, so a stop is
    /// never awaited in place.
    #[must_use = "a stopped task has not necessarily ended; observe it"]
    fn stop(self) -> tokio::task::JoinHandle<()> {
        let HostedTask { handle, stop, .. } = self;
        match stop {
            Some(stop) => {
                let _ = stop.send(true);
            }
            None => handle.abort(),
        }
        handle
    }
}

impl Hosted {
    /// Stop this author's producers and hand back their handles.
    ///
    /// Stopping only BEGINS an end; it does not establish that the
    /// task's future and the resources it owns have been dropped. A
    /// retirement that discarded these could release the lease while
    /// that was still pending (codex on d2b012d), so the handles come
    /// back and the caller observes them end.
    #[must_use = "a stopped task has not necessarily ended; observe these"]
    fn stop(mut self) -> Vec<tokio::task::JoinHandle<()>> {
        self.timers.take();
        std::mem::take(&mut self.tasks)
            .into_values()
            .map(HostedTask::stop)
            .collect()
    }

    /// Start `source`'s task as a new instance, if it runs one.
    fn start(
        &mut self,
        author: &AgentLabel,
        source: &clank_core::agent_config::WaitEventSource,
        origin: crate::cli::event_source::SourceOrigin,
        events_dir: &std::path::Path,
        starts: &Starts<'_>,
    ) {
        let instance = next_instance();
        let sink = host_sink(starts.into, starts.lifecycle, author, instance);
        if let Some(task) = (starts.spawn)(source, origin, events_dir, sink) {
            self.tasks.insert(
                task.key,
                HostedTask {
                    instance,
                    handle: task.handle,
                    stop: task.stop,
                },
            );
        }
    }

    /// Cancel all production at a stand-down, once its drain has had its
    /// chance to retire every group, preserving the accepted spec for the
    /// owner's final presentation; passive future deadlines included.
    fn stop_producing(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        self.timers.take();
        for task in self.tasks.values() {
            task.handle.abort();
        }
        std::mem::take(&mut self.tasks)
            .into_values()
            .map(|task| task.handle)
            .collect()
    }
}

/// An author's CONFIG sources — the only kind the monitor may host.
///
/// A `--event` source belongs to ONE INVOCATION and is never pooled:
/// a monitor that ran one would deliver an invocation's events to
/// whoever else happened to be that author, so it keeps its wait
/// watching for itself.
fn hostable(
    repo: &std::path::Path,
    author: &AgentLabel,
) -> anyhow::Result<
    Vec<(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )>,
> {
    crate::cli::wait::resolve_wait_event_sources(repo, author, &[])
}

/// Every agent this repo's roster names.
///
/// The ROSTER and not the connected clients, which is the whole point:
/// tying a producer to whoever happens to be attached is tying it to a
/// delivery client again, and the churn that causes is what this plan
/// exists to end. A monitor's producers live as long as the monitor.
fn roster_authors(repo: &std::path::Path) -> Vec<AgentLabel> {
    crate::agent_store::roster_labels(repo)
}

#[cfg(test)]
#[must_use = "a stopped producer may still be winding down; keep its handle"]
fn reconcile_hosting(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<AgentLabel, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(AgentLabel, crate::cli::event_source::Emission)>,
) -> Vec<tokio::task::JoinHandle<()>> {
    reconcile_hosting_changed(repo, hosted, into, false)
}

#[cfg(test)]
#[must_use = "a stopped producer may still be winding down; keep its handle"]
fn reconcile_hosting_changed(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<AgentLabel, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(AgentLabel, crate::cli::event_source::Emission)>,
    timers_changed: bool,
) -> Vec<tokio::task::JoinHandle<()>> {
    reconcile_hosting_reporting(repo, hosted, into, timers_changed, None)
}

/// The channel a hosted source's lifecycle is reported on.
type LifecycleTx = tokio::sync::mpsc::UnboundedSender<crate::cli::event_source::LifecycleReport>;

/// What each hosted source's producer last reported, with the instance of
/// the task that reported it.
type Lifecycles =
    std::collections::BTreeMap<(AgentLabel, String), (u64, crate::cli::event_source::Lifecycle)>;

/// A fresh instance number for tasks this monitor spawns, so a replaced
/// task's late reports are known for what they are.
fn next_instance() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Keep a producer's report if it is from the task hosted for its source
/// now; a replaced task's late word is dropped. Whether it was kept.
fn record_lifecycle(
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    lifecycles: &mut Lifecycles,
    (author, key, instance, state): crate::cli::event_source::LifecycleReport,
) -> bool {
    if !current(hosted, &author, &key, instance) {
        return false;
    }
    lifecycles.insert((author, key), (instance, state));
    true
}

/// Forget what replaced or removed tasks last said: a source's new task
/// has said nothing yet, and its old one's word is not about it.
fn forget_replaced(
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    lifecycles: &mut Lifecycles,
) {
    lifecycles.retain(|(author, key), (instance, _)| current(hosted, author, key, *instance));
}

/// Whether `instance` is the task `author` hosts for `key` now.
fn current(
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    author: &AgentLabel,
    key: &str,
    instance: u64,
) -> bool {
    hosted
        .get(author)
        .and_then(|h| h.tasks.get(key))
        .is_some_and(|task| task.instance == instance)
}

/// Bring hosting into line with the roster and its configs, reporting
/// each hosted source's lifecycle on `lifecycle`.
///
/// Called on every fold, because `.clank/` is a wake dir and an edited
/// `wait_events` must take effect without restarting the monitor.
///
/// Source by source: a task whose definition is unchanged is left
/// running as the same instance, a removed or changed one is stopped,
/// and only what is new or changed is started. Timers are reconciled
/// apart from the tasks, so neither ever restarts the other.
fn reconcile_hosting_reporting(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<AgentLabel, Hosted>,
    into: &tokio::sync::mpsc::UnboundedSender<(AgentLabel, crate::cli::event_source::Emission)>,
    timers_changed: bool,
    lifecycle: Option<&LifecycleTx>,
) -> Vec<tokio::task::JoinHandle<()>> {
    reconcile_hosting_with(
        repo,
        hosted,
        timers_changed,
        &Starts::hosting(into, lifecycle),
    )
}

fn reconcile_hosting_with(
    repo: &std::path::Path,
    hosted: &mut std::collections::BTreeMap<AgentLabel, Hosted>,
    timers_changed: bool,
    starts: &Starts<'_>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let retry_timers = |timers: &mut crate::cli::timer_source::TimerSet| {
        if timers_changed {
            timers.refresh();
            timers.retry_one_shots();
        } else {
            timers.retry();
        }
    };
    let mut departing = Vec::new();
    let authors = roster_authors(repo);
    for author in &authors {
        // An invalid edit is a rejected replacement, not an empty config.
        // Keep the last accepted drivers and definitions until a valid edit.
        let spec = match hostable(repo, author) {
            Ok(spec) => spec,
            Err(error) => {
                eprintln!("monitor: rejected source config for `{author}`: {error}");
                if let Some(running) = hosted.get_mut(author)
                    && let Some(timers) = &mut running.timers
                {
                    retry_timers(timers);
                }
                continue;
            }
        };
        let host = hosted.entry(author.clone()).or_default();
        if host.spec != spec {
            // Once per change of this author's sources, not on every fold.
            for (source, _) in &spec {
                if let Some(problem) = crate::cli::event_interrupt::hint_problem(source) {
                    eprintln!("monitor: `{author}`: {problem}");
                }
            }
        }
        let events_dir = crate::agent_store::agents_root(repo)
            .join(author.as_str())
            .join("events");
        departing.extend(reconcile_tasks(host, author, &spec, &events_dir, starts));
        let timers = reconcile_timers(host, author, &spec, &events_dir, starts, retry_timers);
        host.spec = spec
            .into_iter()
            .filter(|(s, _)| !is_timer(s))
            .chain(timers)
            .collect();
        if host.tasks.is_empty() && host.timers.is_none() {
            hosted.remove(author);
        }
    }
    // An author off the roster keeps nothing running: a revoked agent's
    // producer is a producer nobody can ever be served from.
    let gone: Vec<AgentLabel> = hosted
        .keys()
        .filter(|a| !authors.contains(a))
        .cloned()
        .collect();
    for author in gone {
        if let Some(running) = hosted.remove(&author) {
            departing.extend(running.stop());
        }
    }
    departing
}

fn is_timer(source: &clank_core::agent_config::WaitEventSource) -> bool {
    matches!(source, clank_core::agent_config::WaitEventSource::Timer(_))
}

/// Bring `host`'s tasks into line with `spec`, matching each by the
/// storage key it owns and replacing it only when its definition
/// changed. The stopped tasks, to observe as they end.
fn reconcile_tasks(
    host: &mut Hosted,
    author: &AgentLabel,
    spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )],
    events_dir: &std::path::Path,
    starts: &Starts<'_>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let by_key = |spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )]| {
        spec.iter()
            .filter_map(|entry| {
                Some((
                    crate::cli::wait::task_key(&entry.0, entry.1, "")?,
                    entry.clone(),
                ))
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let wanted = by_key(spec);
    let accepted = by_key(&host.spec);
    let replaced: Vec<String> = host
        .tasks
        .keys()
        .filter(|key| match wanted.get(*key) {
            Some(definition) => accepted.get(*key) != Some(definition),
            None => true,
        })
        .cloned()
        .collect();
    let stopped = replaced
        .iter()
        .filter_map(|key| host.tasks.remove(key))
        .map(HostedTask::stop)
        .collect();
    for (key, (source, origin)) in &wanted {
        if !host.tasks.contains_key(key) {
            host.start(author, source, *origin, events_dir, starts);
        }
    }
    stopped
}

/// Bring `host`'s timers into line with `spec`, leaving an unchanged set
/// as it is and carrying each unchanged timer's deadline into a changed
/// one. The timer definitions now accepted: the old ones when the new
/// set cannot be built.
fn reconcile_timers(
    host: &mut Hosted,
    author: &AgentLabel,
    spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )],
    events_dir: &std::path::Path,
    starts: &Starts<'_>,
    retry_timers: impl Fn(&mut crate::cli::timer_source::TimerSet),
) -> Vec<(
    clank_core::agent_config::WaitEventSource,
    crate::cli::event_source::SourceOrigin,
)> {
    let timers = |spec: &[(
        clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
    )]| {
        spec.iter()
            .filter(|(s, _)| is_timer(s))
            .cloned()
            .collect::<Vec<_>>()
    };
    let wanted = timers(spec);
    let accepted = timers(&host.spec);
    if wanted.is_empty() {
        host.timers = None;
        return wanted;
    }
    if wanted == accepted
        && let Some(running) = &mut host.timers
    {
        // One retry per existing reconciliation wake. New/replaced
        // sets already attempted catchup; do not try them twice.
        retry_timers(running);
        return wanted;
    }
    let sink = host_sink(starts.into, starts.lifecycle, author, next_instance());
    // Constructors neither ingest nor spawn: a set that cannot be built
    // leaves the running one untouched.
    match crate::cli::timer_source::TimerSet::new(&wanted, events_dir, None, sink) {
        Ok(mut timers) => {
            if let Some(previous) = host.timers.take() {
                timers.keep_unchanged(previous);
            }
            timers.arm();
            host.timers = Some(timers);
            wanted
        }
        Err(error) => {
            eprintln!("monitor: timer config for `{author}`: {error}");
            if let Some(running) = &mut host.timers {
                retry_timers(running);
            }
            accepted
        }
    }
}

/// What of an emission the monitor must carry itself.
///
/// Empty for anything the log holds, because the log is what decides
/// who is owed it and when. The item itself only for a `Transient`:
/// github keeps delivering after a failed append so the agent still
/// learns about the event, and that copy is the only record there will
/// ever be.
///
/// Reads the provenance the producer SENT rather than inspecting the
/// logs. Two rounds were spent inferring it and both were wrong: a
/// durable emission is not equal to its stored row, because
/// presentation is stamped after the append, and absence proves
/// nothing anyway once a row can be compacted away (codex on 0555c8f,
/// aa3d9a5).
fn transient_of(emission: crate::cli::event_source::Emission) -> Vec<WaitItem> {
    use crate::cli::event_source::Durability;
    match emission {
        (_, Durability::Logged) => Vec::new(),
        (item, Durability::Transient) => vec![item],
    }
}

/// Add a newcomer, having presented to IT and to nobody else.
///
/// A named step rather than loop plumbing because the thing worth
/// getting right is which clients a join touches: a queue is
/// `CLIENT_QUEUE` deep, so presenting to everybody on each arrival
/// lets a burst of joins retire established readers that were keeping
/// up. Returns nothing because a newcomer retired on its first frame
/// simply never joins, which is the same answer as never arriving.
fn admit_into(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    client: Client,
    state: Option<&crate::repo_state::RepoState>,
    pending: &mut Vec<(AgentLabel, WaitItem)>,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let mut leaving = Vec::new();
    // EOF closes each writer's receiver. Remove those connections before
    // admitting a replacement; no task token or cleanup by session-wide key.
    let mut live = Vec::new();
    for old in clients.drain(..) {
        if old.out.is_closed() {
            if let Some(writer) = old.writer {
                leaving.push(writer);
            }
        } else {
            live.push(old);
        }
    }
    *clients = live;
    let duplicate = automatic_grok_client(repo, &client)
        && clients
            .iter()
            .any(|old| old.holder == client.holder && old.who == client.who);
    // A report's binding is the policy's to judge: it answers a stale one
    // Silent, which a refusal would turn into noise.
    let refusal = client.report.is_none().then(|| client_refusal(repo, &client)).flatten().or_else(|| duplicate.then(||
        "this Grok session already has a connected automatic wait; keep that task or cancel it before starting another".to_owned()));
    if let Some(why) = refusal {
        send_note(&client, &Note::Refused { why });
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
        return leaving;
    }
    // One parked wait per agent session: the one admitted last. The
    // harness never cancels a Claude or Codex hook's background wait, so
    // the older ones are left parked, and each would wake the agent with
    // the same work. Grok's automatic wait is a task Grok manages, so its
    // duplicate is refused above instead, keeping the connected one.
    let (older, kept): (Vec<Client>, Vec<Client>) = std::mem::take(clients)
        .into_iter()
        .partition(|old| same_wait(old, &client));
    *clients = kept;
    for old in older {
        leaving.extend(supersede(old));
    }
    let mut arriving = vec![client];
    if let Some(state) = state {
        // The REAL buffer: a transient retained while its author had
        // no client is owed to the first client that author gets, and
        // a newcomer that returns on admission work without it would
        // leave it waiting for a tick that may never find an eligible
        // client (codex on 1552f66).
        leaving.extend(serve_to_cached(
            repo,
            &mut arriving,
            state,
            None,
            pending,
            &mut Default::default(),
            hosted,
        ));
        clients.append(&mut arriving);
        return leaving;
    }
    clients.append(&mut arriving);
    leaving
}

fn automatic_grok_client(repo: &std::path::Path, client: &Client) -> bool {
    let Served::Agent { author } = &client.who else {
        return false;
    };
    if !client
        .holder
        .as_ref()
        .is_some_and(|h| h.tool == clank_core::Tool::Grok)
    {
        return false;
    }
    let Ok(cfg) = crate::agent_store::load_agent_config(repo, author) else {
        return false;
    };
    let home = super::grok_setup::launch_home(repo, author);
    crate::cli::team::resolve_effective_auto_mode(cfg.as_ref(), home.as_deref())
        == clank_core::AutoMode::On
}

fn client_refusal(repo: &std::path::Path, client: &Client) -> Option<String> {
    let (Served::Agent { author } | Served::Helper { author }) = &client.who else {
        return None;
    };
    let holder = client.holder.as_ref()?;
    if !crate::agent_store::binding_matches(repo, author, holder.tool, &holder.session) {
        return Some(format!(
            "this {} session no longer owns `{author}`",
            holder.tool.as_str()
        ));
    }
    None
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

pub(crate) fn describe(repo: &std::path::Path, pid: i32, socket: &std::path::Path) -> Descriptor {
    Descriptor {
        proto: PROTO,
        pid,
        repo: repo.to_string_lossy().to_string(),
        socket: socket.to_string_lossy().to_string(),
        build: None,
    }
}

/// What one monitor process has seen of its installation: kept apart
/// from the routing descriptor, under that process's own name, so it can
/// be written at any time without touching where clients connect.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Observation {
    pub(crate) pid: i32,
    pub(crate) birth: Option<crate::proc_identity::ProcToken>,
    pub(crate) running: String,
    pub(crate) installed_at: String,
    pub(crate) source: crate::build_stamp::Source,
    pub(crate) state: Observed,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Observed {
    Current,
    Replaced { installed: String },
    Unavailable { why: String },
}

impl From<&crate::build_stamp::Install> for Observed {
    fn from(install: &crate::build_stamp::Install) -> Self {
        use crate::build_stamp::Install;
        match install {
            Install::Current => Observed::Current,
            Install::Replaced(stamp) => Observed::Replaced {
                installed: stamp.to_string(),
            },
            Install::Unavailable(why) => Observed::Unavailable { why: why.clone() },
        }
    }
}

const OBSERVATION_PREFIX: &str = "build-";

fn observation_path(repo: &std::path::Path, pid: i32) -> std::path::PathBuf {
    runtime_path(repo, &format!("{OBSERVATION_PREFIX}{pid}.json"))
}

/// Write `observation` as its process's record for `repo`, by rename.
pub(crate) fn note_observation(
    repo: &std::path::Path,
    observation: &Observation,
) -> std::io::Result<()> {
    let path = observation_path(repo, observation.pid);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(observation)?)?;
    std::fs::rename(tmp, path)
}

/// Remove this repo's observation records whose process is gone, as
/// left by monitors that died.
fn forget_dead_observations(repo: &std::path::Path) {
    let marker = runtime_path(repo, OBSERVATION_PREFIX);
    let (Some(dir), Some(prefix)) = (marker.parent(), marker.file_name()) else {
        return;
    };
    let prefix = prefix.to_string_lossy().into_owned();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(&prefix) {
            continue;
        }
        let alive = std::fs::read(entry.path())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Observation>(&bytes).ok())
            .is_some_and(|o| o.birth.is_some_and(|b| b.still_holds(o.pid)));
        if !alive {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether a repo's monitor process exists, and the build it serves.
///
/// Existence only: a `Running` monitor is not thereby responsive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// The published pid is alive and, where its observation record
    /// carries a birth, still the process that wrote it.
    Running {
        pid: i32,
        /// `None` when the descriptor carries none, as a legacy or
        /// other-version one does not.
        build: Option<DescribedBuild>,
        /// What it last recorded of its installation, when a record
        /// holds for this process.
        observed: Option<Observed>,
    },
    /// The published pid is dead, or is a stranger by its recorded birth.
    Gone {
        pid: i32,
    },
    /// Something is published but no pid can be believed from it.
    Unknown(String),
    None,
}

impl Liveness {
    /// The `monitor:` line of `clank status`.
    pub(crate) fn line(&self) -> String {
        match self {
            Liveness::Running {
                pid,
                build,
                observed,
            } => describe_build(*pid, build.as_ref(), observed.as_ref()),
            Liveness::Gone { pid } => format!("gone (pid {pid} exited)"),
            Liveness::Unknown(why) => format!("unknown: {why}"),
            Liveness::None => "none".to_string(),
        }
    }
}

/// Whether `repo`'s monitor is running: the routing descriptor joined to
/// the observation record of the process it names.
pub(crate) fn liveness(repo: &std::path::Path) -> Liveness {
    match published(repo) {
        Published::Ours(d) => serving(repo, d.pid, d.build),
        Published::Other { pid: Some(pid), .. } => serving(repo, pid, None),
        Published::Other {
            proto, pid: None, ..
        } => Liveness::Unknown(format!("a proto {proto} descriptor with no readable pid")),
        // Nothing read from a namespace we do not trust is evidence, of
        // a monitor or of its absence.
        Published::Untrusted(why) => Liveness::Unknown(why),
        Published::Nothing => Liveness::None,
    }
}

fn serving(repo: &std::path::Path, pid: i32, build: Option<DescribedBuild>) -> Liveness {
    if !crate::signal::alive(pid) {
        return Liveness::Gone { pid };
    }
    let recorded = std::fs::read(observation_path(repo, pid))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Observation>(&bytes).ok())
        .filter(|o| o.pid == pid)
        .and_then(|o| o.birth.map(|birth| (birth, o.state)));
    let observed = match recorded {
        Some((birth, state)) if birth.still_holds(pid) => Some(state),
        // A record without a birth this build can compare proves
        // nothing either way, so the live pid stands unobserved.
        None | Some((crate::proc_identity::ProcToken::Unrecognised, _)) => None,
        Some(_) => return Liveness::Gone { pid },
    };
    Liveness::Running {
        pid,
        build,
        observed,
    }
}

fn describe_build(pid: i32, build: Option<&DescribedBuild>, observed: Option<&Observed>) -> String {
    let Some(build) = build else {
        return format!("pid {pid}, build unknown (legacy)");
    };
    let state = match observed {
        Some(Observed::Current) => "current".to_string(),
        Some(Observed::Replaced { installed }) => {
            format!("replaced by {installed}, retiring")
        }
        Some(Observed::Unavailable { why }) => format!("install path unavailable: {why}"),
        None => "not yet observed".to_string(),
    };
    let fallback = if build.source == crate::build_stamp::Source::Fallback {
        " (fallback path)"
    } else {
        ""
    };
    format!(
        "pid {pid}, build {} at {}{fallback}: {state}",
        build.running, build.installed_at
    )
}

/// Fold, and move the heartbeat ONLY if that succeeded.
///
/// The heartbeat means a fold completed. Advancing it first and then
/// failing the rebuild would advertise progress that never happened,
/// and a loop that kept failing would keep looking fresh to the
/// watchdog — the invariant this plan set for itself, inverted
/// (codex on 13e9fa7).
async fn advance(
    repo: &std::path::Path,
    fold: u64,
    heartbeat: &std::sync::atomic::AtomicU64,
) -> Option<(u64, crate::repo_state::RepoState)> {
    let outcome = folded(repo).await;
    advance_with(fold, heartbeat, outcome)
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
    fold: u64,
    heartbeat: &std::sync::atomic::AtomicU64,
    outcome: anyhow::Result<crate::repo_state::RepoState>,
) -> Option<(u64, crate::repo_state::RepoState)> {
    let state = match outcome {
        Ok(state) => state,
        Err(e) => {
            eprintln!("monitor: {e}");
            return None;
        }
    };
    let next = fold + 1;
    heartbeat.store(next, std::sync::atomic::Ordering::Release);
    Some((next, state))
}

/// One completed fold of the repo.
/// One fold's publication, as a monitor in this process would make it:
/// what status reads of each agent's turns.
#[cfg(test)]
pub(crate) async fn publish_one_fold(repo: &std::path::Path) {
    publish(
        repo,
        &describe(
            repo,
            std::process::id() as i32,
            std::path::Path::new("socket"),
        ),
    )
    .unwrap();
    let state = folded(repo).await.unwrap();
    let mut recovery = crate::cli::recovery::Driver::new(repo.to_path_buf(), "/nonexistent".into());
    let mut turns = crate::cli::turn_custody::Turns::new(repo.to_path_buf());
    let facts = recovery_facts(
        repo,
        &state,
        &[],
        &Default::default(),
        &mut Default::default(),
        &[],
        &mut turns,
        &Routes::new(),
    );
    recovery.reconcile(facts, crate::cli::recovery::unix_now());
}

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

/// Which [`Served`] a client's declared identity earns, or why not.
///
/// A named function rather than the closure this was, so the roster
/// decision is reachable from a test without a socket.
fn served_for(repo: &std::path::Path, who: Who) -> Result<Served, Welcome> {
    match who {
        Who::Observer => Ok(Served::Observer),
        Who::Control { .. } => Err(Welcome::Decline {
            proto: PROTO,
            why: "a control request is answered, not served".to_string(),
        }),
        Who::RecoveryHelper { author, .. } => match served_for(
            repo,
            Who::Agent {
                author,
                holder: None,
                has_cli_sources: false,
                turn_end: None,
            },
        )? {
            Served::Agent { author } => Ok(Served::Helper { author }),
            other => Ok(other),
        },
        Who::Agent { author, .. } => {
            let author = AgentLabel::parse(&author).map_err(|e| Welcome::Decline {
                proto: PROTO,
                why: format!("invalid author `{author}`: {e}"),
            })?;
            match crate::agent_store::resolve_role(repo, &author) {
                Ok(_) => Ok(Served::Agent { author }),
                Err(e) => Err(Welcome::Decline {
                    proto: PROTO,
                    why: format!("`{author}` is not on this repo's roster: {e}"),
                }),
            }
        }
    }
}

/// What an accepted connection turned out to want.
#[derive(Debug)]
enum Greeted {
    Client(Client),
    /// A control request, for the owner loop to carry out and answer on
    /// `out` before the connection closes.
    Control {
        request: Control,
        out: tokio::sync::mpsc::Sender<String>,
        writer: tokio::task::JoinHandle<Wrote>,
    },
    /// A successor asked, and was told this monitor is retiring. The
    /// answer is already sent; what remains is to do it.
    StandDown,
}

#[cfg(test)]
impl Greeted {
    /// The client, for the tests that are about admission rather than
    /// about retirement.
    fn client(self) -> Client {
        match self {
            Greeted::Client(c) => c,
            Greeted::StandDown => panic!("a hello was answered as a successor's ask"),
            Greeted::Control { .. } => panic!("a hello was answered as a control request"),
        }
    }
}

/// Read an opening frame and answer it, bounded and asynchronous.
async fn greet(stream: tokio::net::UnixStream, repo: &std::path::Path) -> anyhow::Result<Greeted> {
    greet_within(stream, repo, ATTACH_BUDGET).await
}

/// [`greet`] with the budget supplied, so the timeout is testable
/// without waiting the production five seconds.
async fn greet_within(
    stream: tokio::net::UnixStream,
    repo: &std::path::Path,
    budget: Duration,
) -> anyhow::Result<Greeted> {
    tokio::time::timeout(budget, admit(stream, repo))
        .await
        .map_err(|_| anyhow::anyhow!("a client did not finish its hello within {budget:?}"))?
}

async fn admit(stream: tokio::net::UnixStream, repo: &std::path::Path) -> anyhow::Result<Greeted> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mut io = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    if io.read_line(&mut line).await? == 0 {
        anyhow::bail!("a client closed before saying hello");
    }

    let greeting = greeting_of(line.trim_end(), repo);
    if let Greeting::Obsolete(decline) = &greeting {
        // Told WHY before this process goes, so the client is not left
        // reading an abrupt EOF and guessing at it. Then the monitor
        // stands down, and the client's ordinary connect-or-spawn
        // starts the current one.
        let line = serde_json::to_string(decline)?;
        io.get_mut()
            .write_all(format!("{line}\n").as_bytes())
            .await?;
        return Ok(Greeted::StandDown);
    }

    if let Greeting::Client(Hello {
        who: Who::Control { request },
        ..
    }) = &greeting
    {
        let request = request.clone();
        io.get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::to_string(&Welcome::Ok { proto: PROTO })?
                )
                .as_bytes(),
            )
            .await?;
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let writer = tokio::spawn(write_to(
            io.into_inner(),
            queued,
            "a control client".to_string(),
        ));
        return Ok(Greeted::Control {
            request,
            out,
            writer,
        });
    }

    // An agent's author is resolved BEFORE admission: welcoming one the
    // roster does not know would register a connection nothing can ever
    // serve. An observer asks for no projection, so there is nothing to
    // resolve and nothing a roster could authorise — refusing it for
    // lacking an identity would only mean a panel could not subscribe.
    let admitted = match greeting {
        Greeting::Decline(d) => Err(d),
        Greeting::Obsolete(_) => unreachable!("returned above"),
        Greeting::Client(hello) => served_for(repo, hello.who.clone()).map(|who| match hello.who {
            Who::Agent {
                holder, turn_end, ..
            } => (who, holder, turn_end),
            Who::RecoveryHelper { holder, .. } => (who, Some(holder), None),
            Who::Observer | Who::Control { .. } => (who, None, None),
        }),
    };

    let welcome = match &admitted {
        Ok(_) => Welcome::Ok { proto: PROTO },
        Err(decline) => decline.clone(),
    };
    io.get_mut()
        .write_all(format!("{}\n", serde_json::to_string(&welcome)?).as_bytes())
        .await?;

    let (who, holder, report) = admitted.map_err(|d| anyhow::anyhow!("{d:?}"))?;
    if let Served::Agent { author, .. } = &who {
        tracing::debug!(%author, "monitor agent wait admitted");
    }
    let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
    let (farewell, last) = tokio::sync::oneshot::channel();
    let (writer, report) = match report {
        // A report is answered on this connection and its result read
        // back from it, so the read side outlives the writer: the result
        // arrives after the last verdict was written.
        Some(report) => {
            let early = io.buffer().to_vec();
            let (read, write) = io.into_inner().into_split();
            let writer = tokio::spawn(write_queued(
                write,
                queued,
                Some(last),
                who.name(),
                WRITE_BUDGET,
                std::future::pending(),
            ));
            (
                writer,
                Some(Reporting {
                    report,
                    stepped: false,
                    reader: Some((early, read)),
                }),
            )
        }
        None => (
            tokio::spawn(write_to_within(
                io.into_inner(),
                queued,
                Some(last),
                who.name(),
                WRITE_BUDGET,
            )),
            None,
        ),
    };
    Ok(Greeted::Client(Client {
        who,
        holder,
        out,
        writer: Some(writer),
        shown: Default::default(),
        conn: next_conn(),
        report,
        farewell: Some(farewell),
    }))
}

/// Read what a reporting connection says after its hello, one line at a
/// time, until it closes: the delivery result, and then EOF.
async fn hear(
    conn: u64,
    (early, read): (Vec<u8>, tokio::net::unix::OwnedReadHalf),
    heard: tokio::sync::mpsc::UnboundedSender<(u64, Heard)>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};
    const MAX_LINE: usize = 64 * 1024;
    let mut lines = tokio::io::BufReader::new(std::io::Cursor::new(early).chain(read));
    loop {
        let mut line = String::new();
        match (&mut lines)
            .take(MAX_LINE as u64)
            .read_line(&mut line)
            .await
        {
            Ok(0) | Err(_) => break,
            Ok(_) if !line.ends_with('\n') => break,
            Ok(_) => {
                if heard
                    .send((conn, Heard::Line(line.trim_end().to_string())))
                    .is_err()
                {
                    return;
                }
            }
        }
    }
    let _ = heard.send((conn, Heard::Closed));
}

impl Served {
    /// What to call this reader in a log line.
    fn name(&self) -> String {
        match self {
            Served::Agent { author, .. } => author.to_string(),
            Served::Observer => "an observer".to_string(),
            Served::Helper { author } => format!("{author}'s recovery helper"),
        }
    }
}

/// Drive one client's socket, independently of the fold.
///
/// Owning the socket here is what makes the owner's enqueue
/// non-blocking. A write that times out or fails ends the task, which
/// closes the queue — and a closed queue is how the owner learns the
/// client is gone, without ever touching the socket itself.
/// How a client's writer ended.
///
/// Returned rather than inferred. A task that TERMINATED is not a
/// frame that was WRITTEN, and the ways it can terminate are different
/// answers a retirement has to report separately — counting every
/// awaited handle as a success called a closed peer's lost frame a
/// delivery (codex on d2b012d).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wrote {
    /// The queue emptied and the sender was gone: everything queued
    /// reached the socket.
    Drained,
    /// The peer errored. What was still queued did not land.
    Failed,
    /// The peer stopped reading and a write did not finish inside
    /// `WRITE_BUDGET`.
    TimedOut,
}

async fn write_to(
    socket: tokio::net::UnixStream,
    queued: tokio::sync::mpsc::Receiver<String>,
    author: String,
) -> Wrote {
    write_to_within(socket, queued, None, author, WRITE_BUDGET).await
}

/// [`write_to`] with the budget supplied, so giving up is testable
/// without waiting the production two seconds.
async fn write_to_within(
    socket: tokio::net::UnixStream,
    queued: tokio::sync::mpsc::Receiver<String>,
    farewell: Option<tokio::sync::oneshot::Receiver<String>>,
    author: String,
    budget: Duration,
) -> Wrote {
    use tokio::io::AsyncReadExt;
    let (mut read, write) = socket.into_split();
    let unexpected = async move {
        let mut byte = [0u8; 1];
        let _ = read.read(&mut byte).await;
    };
    write_queued(write, queued, farewell, author, budget, unexpected).await
}

/// Write what is queued to `socket` until the queue closes, or until
/// `peer` says the other end spoke out of turn or went away.
async fn write_queued(
    mut socket: impl tokio::io::AsyncWrite + Unpin,
    mut queued: tokio::sync::mpsc::Receiver<String>,
    mut farewell: Option<tokio::sync::oneshot::Receiver<String>>,
    author: String,
    budget: Duration,
    peer: impl std::future::Future<Output = ()>,
) -> Wrote {
    use tokio::io::AsyncWriteExt;
    tokio::pin!(peer);
    loop {
        let line = tokio::select! {
            line = queued.recv() => match line {
                Some(line) => line,
                // Closed: the last frame, if one was left, after all
                // that was queued before it.
                None => match farewell.take().and_then(|mut last| last.try_recv().ok()) {
                    Some(line) => line,
                    None => return Wrote::Drained,
                },
            },
            () = &mut peer => return Wrote::Failed,
        };
        match tokio::time::timeout(budget, socket.write_all(line.as_bytes())).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("monitor: `{author}` closed: {e}");
                return Wrote::Failed;
            }
            Err(_) => {
                eprintln!("monitor: `{author}` stopped reading");
                return Wrote::TimedOut;
            }
        }
    }
}

/// What a retirement established before it released the repo.
///
/// Four outcomes for the writers rather than a success count, because
/// "the task ended" is not "the bytes left" and the ways it can end
/// are different answers: a closed peer, a peer that stopped reading,
/// a task that panicked or was cancelled, and one still going when
/// the budget ran out. Counting them together reported a lost frame
/// as a delivery (codex on d2b012d).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Retired {
    /// Queue emptied, socket closed: everything queued landed.
    pub(crate) drained: usize,
    /// Reached a terminal state without draining — the peer errored or
    /// stopped reading.
    pub(crate) failed: usize,
    /// Ended without reporting at all: panicked, or cancelled by
    /// something other than this retirement.
    pub(crate) lost: usize,
    /// Not collected during the grace, so cancelled here.
    ///
    /// Deliberately not "still writing": the deadline can win while a
    /// terminal outcome is already sitting ready, so this counts what
    /// was NOT ACCOUNTED FOR rather than what was definitely still
    /// going (codex on dd67677).
    pub(crate) abandoned: usize,
    /// Transients still held when the cutoff came.
    ///
    /// Reported because they are the one thing a retirement cannot
    /// make good: a durable row replays from the WAL for whoever
    /// connects next, and a transient has no row. Silence here would
    /// hide a lost wake.
    pub(crate) undelivered: usize,
    /// Every task this monitor owned was observed TERMINATED. Decides
    /// whether the lease may be released by unwinding; see
    /// [`Shutdown`].
    pub(crate) settled: bool,
}

/// Whether a retirement earned the right to give the repo up quietly.
///
/// Exhaustive, and returned rather than reported. The first version
/// made this a `bool` on [`Retired`]: the unsettled case printed
/// "releasing anyway" and then released anyway, which is a log line
/// where a contract was wanted (codex on dd67677).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shutdown {
    /// Everything this monitor owned has ended. Returning from `start`
    /// drops the lease, and nothing is left to race the successor.
    ReadyToRelease(Retired),
    /// Something cancelled has not stopped. The lease must NOT unwind:
    /// a successor taking the repo while a task still owns its
    /// resources is the thing ownership exists to prevent.
    RequiresProcessExit(Retired),
}

impl Shutdown {
    pub(crate) fn report(self) -> Retired {
        match self {
            Shutdown::ReadyToRelease(r) | Shutdown::RequiresProcessExit(r) => r,
        }
    }
}

/// How long a retirement waits for its clients' last frames.
///
/// A whole-step budget, not per client: retiring must be FINITE, and a
/// peer that has stopped reading cannot extend it by the number of its
/// peers. Each writer also bounds its own `write_all` with
/// `WRITE_BUDGET`, so this is the outer limit on all of them together.
const RETIRE_BUDGET: Duration = Duration::from_secs(3);

/// What of a successor's stand-down its producers may spend draining,
/// each source's stop grace clipped to it: the clients' last frames keep
/// a writer's whole budget of the rest.
const HANDOFF_DRAIN: Duration = RETIRE_BUDGET.saturating_sub(WRITE_BUDGET);

/// How long cancellation is given to actually take effect.
///
/// Separate from `RETIRE_BUDGET` and much shorter: by this point every
/// task has been asked to stop, so this covers only the time between
/// `abort` and the task's future being dropped. Exceeding it means the
/// process exits with the lease still held, because an orderly return
/// would unwind that lease first.
const SETTLE_BUDGET: Duration = Duration::from_millis(500);

/// The tasks this monitor owns besides its clients' writers.
///
/// One place rather than two locals, because a retirement has to take
/// ALL of them and "all of them" was spelled at a call site inside
/// `start`'s loop — which no test reaches, so a mutation that left one
/// set behind survived. Here [`OwnedTasks::take_all`] is the whole
/// rule and a test can hold it to it.
#[derive(Default)]
struct OwnedTasks {
    interrupting: super::event_interrupt::Scheduler,
    /// Producers a reconcile cancelled, still winding down.
    departing: futures_util::stream::FuturesUnordered<tokio::task::JoinHandle<()>>,
    /// Handshakes still running. One mid-greeting at the cutoff has a
    /// socket, and if it completes it puts a CLIENT with a live writer
    /// into `arrivals`.
    greeting: futures_util::stream::FuturesUnordered<tokio::task::JoinHandle<()>>,
    /// Readers of reporting connections. Each ends only when its peer
    /// closes, so a retirement cancels them rather than wait.
    hearing: futures_util::stream::FuturesUnordered<tokio::task::JoinHandle<()>>,
}

impl OwnedTasks {
    /// Cancel every live handshake and wait for it to END.
    ///
    /// Before the admission queue closes, and before anything else:
    /// these are what PRODUCE admissions, and one still running when
    /// the queue shuts can welcome a peer and spawn a writer that
    /// nothing owns. `false` means one would not stop, which leaves a
    /// live admission path and forbids an orderly release.
    ///
    /// One bound for the whole phase rather than one per greeting, so
    /// a crowd of slow peers cannot multiply the time this takes.
    async fn end_greetings(&mut self, within: Duration) -> bool {
        use futures_util::StreamExt;
        if self.greeting.is_empty() {
            return true;
        }
        for g in self.greeting.iter_mut() {
            g.abort();
        }
        let ended = async { while self.greeting.next().await.is_some() {} };
        tokio::time::timeout(within, ended).await.is_ok()
    }

    fn take_all(&mut self) -> Vec<tokio::task::JoinHandle<()>> {
        for h in self.hearing.iter() {
            h.abort();
        }
        std::mem::take(&mut self.departing)
            .into_iter()
            .chain(std::mem::take(&mut self.greeting))
            .chain(std::mem::take(&mut self.hearing))
            .chain(std::mem::take(&mut self.interrupting).take_handles())
            .collect()
    }
}

/// A stand-down's producer drain: every group asked to end, all given
/// one `grace` together, before any is cut off.
pub(crate) struct Drain {
    pub(crate) owners: &'static crate::cli::source_group::Owners,
    pub(crate) grace: Duration,
}

/// How long a lease-loss retirement gives its producers to drain.
const DRAIN_GRACE: Duration = crate::cli::source_group::DRAIN_GRACE;

const _: () = assert!(
    crate::cli::command_source::DEFAULT_STOP_GRACE.as_millis() >= DRAIN_GRACE.as_millis(),
    "a stopped source's default grace is no shorter than a drain's"
);

/// A lease-loss retirement's whole budget, so its drain is never cut
/// short by the ordinary stand-down's.
const LEASE_RETIREMENT: Duration = DRAIN_GRACE.saturating_add(RETIRE_BUDGET);

/// Exit WITH the lease held rather than unwind it. An ordinary return
/// would drop it first and let a successor take the repo on top of a
/// task that still owns its resources; the kernel releases the flock
/// when this process dies, which is the one release that cannot race.
fn exit_holding_the_lease() -> ! {
    eprintln!(
        "monitor: cancellation had not taken effect after {SETTLE_BUDGET:?}; exiting with \
         the lease held"
    );
    crate::cli::source_group::exit(STOOD_DOWN_EXIT)
}

/// Everything a stand-down does, from the cutoff to the decision.
///
/// Extracted from `start`'s arm so the whole transition is reachable
/// from a test. It was inline, and a mutation that skipped claiming
/// late arrivals SURVIVED: the helper was proven and its only caller
/// was not, which is the shape of gap that keeps producing these.
/// What stays in the loop is the match on the returned decision.
#[allow(clippy::too_many_arguments)]
async fn stand_down(
    repo: &std::path::Path,
    mut clients: Vec<Client>,
    mut leaving: Vec<tokio::task::JoinHandle<Wrote>>,
    mut hosted: std::collections::BTreeMap<AgentLabel, Hosted>,
    mut owned: OwnedTasks,
    arrivals: &mut tokio::sync::mpsc::UnboundedReceiver<Greeted>,
    from_sources: &mut tokio::sync::mpsc::UnboundedReceiver<(
        AgentLabel,
        crate::cli::event_source::Emission,
    )>,
    folded: Option<&crate::repo_state::RepoState>,
    pending: &mut Vec<(AgentLabel, WaitItem)>,
    drain: Option<Drain>,
    budget: Duration,
    settle: Duration,
) -> Shutdown {
    // ONE deadline across the stages, not one budget each. Giving
    // every phase a fresh `budget` made the cooperative worst case
    // `2 * budget + settle`, so a caller timing out on `budget` would
    // be wrong about its own contract (codex on 5f7bc36).
    let by = tokio::time::Instant::now() + budget;
    let left = |by: tokio::time::Instant| by.saturating_duration_since(tokio::time::Instant::now());
    // ZERO: the drain begins at the cutoff, not after the greetings
    // settle, so they share one timeline: producer admission closes and
    // every group has its TERM and its one cutoff now.
    let draining = drain.map(|drain| drain.owners.drain(drain.grace));
    // ONE: stop the things that PRODUCE admissions, with the receiver
    // still open. A greeting that already handed its client over must
    // still land, and one cancelled mid-handshake must not be able to
    // welcome a peer and spawn a writer after admission closed
    // (codex on b37b6b6). One bound for the phase, not one per
    // greeting.
    let greetings_ended = owned.end_greetings(left(by)).await;

    // TWO: with the producers of admissions stopped, draining the
    // queue is finite. It is the COMPLETE set only when the phase
    // above succeeded; when it did not, the decision at the end
    // forbids an orderly release for exactly that reason.
    let late = claim_late_arrivals(arrivals, &mut clients);
    if late > 0 {
        eprintln!("monitor: {late} admission(s) arrived during the cutoff");
    }

    // Before any producer task is cancelled: a group draining still
    // sends what it finishes into `from_sources`, read below, and its
    // host reads until the group is gone.
    let drained = match draining {
        Some(draining) => {
            let drainage = draining.finish(by).await;
            for (source, how) in &drainage.outcomes {
                eprintln!("monitor: source {}: {how}", source.display());
            }
            if drainage.end != crate::cli::source_group::DrainEnd::Complete {
                eprintln!(
                    "monitor: the producer drain did not finish: {:?}",
                    drainage.end
                );
            }
            drainage.end == crate::cli::source_group::DrainEnd::Complete
        }
        None => true,
    };

    // Successful sends already hold custody in this queue; cancelling
    // the producers cannot discard a private forwarding buffer. Durable
    // rows replay from the WAL, but transients need this final drain.
    let mut producers: Vec<tokio::task::JoinHandle<()>> = owned.take_all();
    for running in hosted.values_mut() {
        producers.extend(running.stop_producing());
    }
    from_sources.close();
    while let Ok((author, emission)) = from_sources.try_recv() {
        pending.extend(
            transient_of(emission)
                .into_iter()
                .map(|item| (author.clone(), item)),
        );
    }

    // FOUR: present ONCE, to the whole set. This used to run in the
    // caller, BEFORE the late clients were claimed — so a transient
    // whose only eligible recipient was still in the queue was
    // retained, then dropped unpresented when that client's sender
    // went. Both the item and its recipient were in hand and the item
    // was reported lost anyway. Nothing to present before the first
    // fold: what a client is owed is read against folded state.
    if let Some(state) = folded {
        leaving.extend(serve_to_cached(
            repo,
            &mut clients,
            state,
            None,
            pending,
            &mut Default::default(),
            &hosted,
        ));
    }

    let decision = retire(clients, leaving, producers, left(by), settle, pending.len()).await;
    let got = decision.report();
    eprintln!(
        "monitor: standing down for {} ({} drained, {} failed, {} lost, {} abandoned)",
        repo.display(),
        got.drained,
        got.failed,
        got.lost,
        got.abandoned
    );
    if got.undelivered > 0 {
        eprintln!(
            "monitor: {} transient(s) reached no client; they have no log row to replay from",
            got.undelivered
        );
    }
    // A greeting that could not be stopped is a live admission path,
    // and a drain not accounted for may leave a producer running: an
    // orderly release must leave neither behind.
    match (greetings_ended && drained, decision) {
        (true, d) => d,
        (false, d) => Shutdown::RequiresProcessExit(d.report()),
    }
}

/// Take what was admitted while the stand-down was being decided.
///
/// A client whose handshake completed moments ago is in the CHANNEL,
/// not in `clients`. Left there it is dropped with its writer still
/// owning a socket — the detached task the whole inventory exists to
/// prevent (codex on 056c991). Closing first is what makes this
/// finite: no further sender can add to it while we drain.
fn claim_late_arrivals(
    arrivals: &mut tokio::sync::mpsc::UnboundedReceiver<Greeted>,
    clients: &mut Vec<Client>,
) -> usize {
    arrivals.close();
    let mut took = 0;
    while let Ok(late) = arrivals.try_recv() {
        match late {
            Greeted::Client(c) => {
                clients.push(c);
                took += 1;
            }
            // A second successor, asking for what is already
            // happening. It was acknowledged on its own connection.
            Greeted::StandDown => {}
            // Told why, and drained with the readers so its writer is
            // owned to the end rather than detached.
            Greeted::Control { out, writer, .. } => {
                let client = Client {
                    who: Served::Observer,
                    holder: None,
                    out,
                    writer: Some(writer),
                    shown: Default::default(),
                    conn: 0,
                    report: None,
                    farewell: None,
                };
                send_note(
                    &client,
                    &Note::Refused {
                        why: "the monitor is standing down".to_string(),
                    },
                );
                clients.push(client);
                took += 1;
            }
        }
    }
    took
}

/// Give up ownership: no more admissions, the queued frames out, the
/// producers stopped, and every owned task observed to have ended.
///
/// The ORDER is the contract. Hosting is cancelled first so no source
/// can submit another emission; then each client's sender is dropped,
/// which is what ends `write_to`'s loop AFTER it has written what is
/// queued; then outcomes are collected as they arrive rather than in
/// handle order, because a stalled writer must not hide the peers
/// behind it that finished long ago. Only when every handle — the
/// writers and the cancelled producers alike — has been observed to
/// end does the caller release the lease.
async fn retire(
    clients: Vec<Client>,
    leaving: Vec<tokio::task::JoinHandle<Wrote>>,
    producers: Vec<tokio::task::JoinHandle<()>>,
    budget: Duration,
    settle: Duration,
    undelivered: usize,
) -> Shutdown {
    // Producers arrive already cancelled, by the caller: stopping
    // them is what makes the source buffer finite, and that has to
    // happen before the final presentation rather than here.
    // The already-retired first: a client woken on the very frame that
    // retired it is the likeliest to still have bytes in flight.
    let mut writers: Vec<tokio::task::JoinHandle<Wrote>> = leaving;
    writers.extend(clients.into_iter().filter_map(|c| {
        // Dropping the sender is what ends `write_to`'s loop, AFTER it
        // has written what is already queued.
        let Client { out, writer, .. } = c;
        drop(out);
        writer
    }));

    // A set this function OWNS across the deadline, rather than a
    // combinator future it hands the handles to. `select_all` takes
    // them by value, and `tokio::select!` builds its arms eagerly — so
    // when the deadline won, that future was dropped with the
    // unfinished handles inside it and those tasks DETACHED instead of
    // being cancelled. The retirement then reported them as neither
    // delivered nor abandoned. Found by the test for the stalled peer.
    use futures_util::StreamExt;
    let mut running: futures_util::stream::FuturesUnordered<_> = writers.into_iter().collect();

    let mut got = Retired {
        undelivered,
        ..Default::default()
    };
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);
    while !running.is_empty() {
        tokio::select! {
            _ = &mut deadline => break,
            Some(ended) = running.next() => match ended {
                Ok(Wrote::Drained) => got.drained += 1,
                Ok(Wrote::Failed | Wrote::TimedOut) => got.failed += 1,
                // Panicked, or cancelled by something that is not this
                // retirement. Either way it never reported.
                Err(_) => got.lost += 1,
            },
        }
    }

    got.abandoned = running.len();
    for w in running.iter_mut() {
        w.abort();
    }
    // `abort` only REQUESTS cancellation. Awaiting afterwards is what
    // establishes that the future was dropped and its resources with
    // it — which has to happen before the lease frees, or a successor
    // takes the repo while this one is still winding down.
    let settling = async {
        while running.next().await.is_some() {}
        for p in producers {
            let _ = p.await;
        }
    };
    got.settled = tokio::time::timeout(settle, settling).await.is_ok();
    if got.settled {
        Shutdown::ReadyToRelease(got)
    } else {
        Shutdown::RequiresProcessExit(got)
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
/// Test-only: production always has a pending buffer to pass, and
/// passing a fresh one would be the bug this plan spent two rounds on.
#[cfg(test)]
fn serve(repo: &std::path::Path, clients: &mut Vec<Client>, state: &crate::repo_state::RepoState) {
    // Dropped: this is for the tests that assert on what a client
    // RECEIVED, and they hold the runtime open around the call.
    drop(serve_to(repo, clients, state, None, &mut Vec::new()));
}

/// [`serve`], optionally scoped to ONE author's clients.
///
/// Scoped on a source signal, because a client's queue is only
/// `CLIENT_QUEUE` deep: presenting to everybody whenever one author's
/// source logged something would put a frame in every other client's
/// queue, and a burst would retire readers that were keeping up
/// perfectly well.
#[must_use = "a departing client's last frame is still in its writer; drop these only \
              where the runtime is certain to outlive them"]
#[cfg(test)]
fn serve_to(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: &crate::repo_state::RepoState,
    only: Option<&AgentLabel>,
    pending: &mut Vec<(AgentLabel, WaitItem)>,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    serve_to_cached(
        repo,
        clients,
        state,
        only,
        pending,
        &mut Default::default(),
        &Default::default(),
    )
}

fn serve_to_cached(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: &crate::repo_state::RepoState,
    only: Option<&AgentLabel>,
    pending: &mut Vec<(AgentLabel, WaitItem)>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    if clients.is_empty() {
        return Vec::new();
    }
    let mut leaving: Vec<tokio::task::JoinHandle<Wrote>> = Vec::new();
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);

    let mut keep = Vec::with_capacity(clients.len());
    // Whose client was WOKEN in this pass, and so received whatever was
    // held for them.
    let mut delivered: Vec<AgentLabel> = Vec::new();
    for mut client in clients.drain(..) {
        if client.report.is_some() {
            keep.push(client);
            continue;
        }
        if let Some(why) = client_refusal(repo, &client) {
            send_note(&client, &Note::Refused { why });
            if let Some(writer) = client.writer {
                leaving.push(writer);
            }
            continue;
        }
        if let Some(only) = only
            && !matches!(&client.who, Served::Agent { author, .. } if author == only)
        {
            keep.push(client);
            continue;
        }
        let note = match &client.who {
            // No projection is derived for an observer, which is the
            // point: it asked to know the fold moved, and deriving
            // somebody's work to tell it that would be the duplicated
            // refold this plan exists to delete.
            Served::Observer | Served::Helper { .. } => Note::Beat,
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
                // The author's un-shown, unacked source rows go in as
                // EXTERNALS, through the same decision as the
                // projection. A source item delivered on its own would
                // reach the agent without the block context the local
                // path gives it, and without the durable eligibility
                // that says it is still owed at all (codex on ca2ea69).
                // Presentation uses the same accepted spec as production. A
                // rejected edit cannot hide the retained drivers' WAL rows or
                // apply unaccepted GitHub prompt changes to their delivery.
                let current;
                let sources = match hosted.get(author) {
                    Some(host) => &host.spec,
                    None => {
                        current = hostable(repo, author).unwrap_or_default();
                        &current
                    }
                };
                let mut external = crate::cli::event_source::owed_from_sources(
                    repo,
                    author,
                    sources,
                    &mut client.shown,
                );
                external.extend(
                    timer_backlogs
                        .entry(author.clone())
                        .or_insert_with(|| {
                            crate::cli::timer_source::Backlog::new(
                                crate::agent_store::agents_root(repo)
                                    .join(author.as_str())
                                    .join("events"),
                                Default::default(),
                            )
                        })
                        .owed(&mut client.shown),
                );
                // An emission no log holds exists nowhere else, so it
                // is merged into THIS presentation rather than sent
                // after it. A client returns on its first non-empty
                // frame, so a second frame carrying the transients
                // reaches a client that has already gone — and they
                // have no row to replay (codex on 328677d). Each
                // client of this author gets them, exactly as its own
                // local wait would have.
                external.extend(
                    pending
                        .iter()
                        .filter(|(whose, _)| whose == author)
                        .map(|(_, item)| item.clone()),
                );

                let items = crate::cli::wait::owed_for(
                    repo,
                    author,
                    &inputs,
                    &status,
                    &state.fold,
                    &mut external,
                );
                if items.is_empty() {
                    Note::Beat
                } else {
                    Note::Items {
                        items,
                        source_cwd: std::env::current_dir().ok(),
                    }
                }
            }
        };
        // An agent connection is ONE delivery opportunity. A client
        // returns on its first `Items` and never reads another, so a
        // frame queued behind that one is written into a socket nobody
        // will read — and anything it carried is gone (codex on
        // 1552f66). Retiring here is what keeps a later event for the
        // next admission instead. The writer task drains what is
        // already queued as its sender drops, so this frame still
        // reaches the agent.
        //
        // The note alone decides, with no second check on who this is
        // or whether the list is empty: `Items` is built just above,
        // only for an agent and only when something is owed. Repeating
        // those conditions here would be two more branches saying what
        // the construction already says, and neither could be driven.
        let woke = matches!(&note, Note::Items { .. });
        let author = match &client.who {
            Served::Agent { author, .. } => Some(author.clone()),
            Served::Observer | Served::Helper { .. } => None,
        };
        let sent = send_note(&client, &note);
        if sent && !woke {
            keep.push(client);
            continue;
        }
        if sent
            && woke
            && let Some(author) = author
            && !delivered.contains(&author)
        {
            delivered.push(author);
        }
        // This client is going, and its LAST frame may still be in its
        // queue. The writer drains it as the sender drops — while the
        // runtime lives. Handing the handle back is what lets a
        // retirement wait for that instead of assuming it (codex on
        // 294c18d).
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
    }
    *clients = keep;
    // Held for an author with nobody to receive them, or whose client
    // was retired on this very frame: a transient that reached no
    // client must keep waiting rather than be dropped, since nothing
    // else will ever hold it.
    pending.retain(|(author, _)| !delivered.contains(author));
    leaving
}

/// Where a check-in could reach each agent, as the monitor knows it.
pub(crate) type Routes = std::collections::BTreeMap<AgentLabel, Result<(), String>>;

/// The route `author`'s check-in would take, or why it has none.
fn route_of(routes: &Routes, author: &AgentLabel) -> Result<(), String> {
    routes
        .get(author)
        .cloned()
        .unwrap_or_else(|| Err("its harness has not been identified yet".into()))
}

/// What `author` is owed now, from `state`, with what is held for it from
/// a source no log keeps.
#[allow(clippy::too_many_arguments)]
fn owed_now(
    repo: &std::path::Path,
    state: &crate::repo_state::RepoState,
    author: &AgentLabel,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(AgentLabel, WaitItem)],
) -> Vec<WaitItem> {
    let reviews = crate::fs_plan_state_lookup::FsPlanStateLookup::new(repo, state.head.as_ref());
    let head = crate::git_io::head_commit_at(repo, state);
    owed_items(
        repo,
        state,
        author,
        hosted,
        timer_backlogs,
        pending,
        &reviews,
        head.as_ref(),
    )
}

/// `author`'s facts now: owing what `state` says, when there is a state.
#[allow(clippy::too_many_arguments)]
fn gather(
    repo: &std::path::Path,
    state: Option<&crate::repo_state::RepoState>,
    author: &AgentLabel,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    pending: &[(AgentLabel, WaitItem)],
    routes: &Routes,
) -> crate::cli::turn_custody::Gathered {
    let owed = state.map(|state| owed_now(repo, state, author, hosted, timer_backlogs, pending));
    let cwd = std::env::current_dir().ok();
    crate::cli::turn_custody::Gathered::of(
        repo,
        author,
        owed.as_deref().map(|owed| (owed, cwd.as_deref())),
        route_of(routes, author),
    )
}

/// Step each held report — every author's, or `only`'s — and answer the
/// ones whose verdict is terminal, retiring them. A parked report is told
/// so once and then kept alive with beats.
#[allow(clippy::too_many_arguments)]
fn serve_reports(
    repo: &std::path::Path,
    clients: &mut Vec<Client>,
    state: Option<&crate::repo_state::RepoState>,
    only: Option<&AgentLabel>,
    pending: &[(AgentLabel, WaitItem)],
    timer_backlogs: &mut std::collections::BTreeMap<AgentLabel, crate::cli::timer_source::Backlog>,
    hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
    turns: &mut crate::cli::turn_custody::Turns,
    routes: &Routes,
) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let mut leaving = Vec::new();
    let mut keep = Vec::with_capacity(clients.len());
    for mut client in clients.drain(..) {
        let (Served::Agent { author }, Some(reporting)) = (&client.who, client.report.as_mut())
        else {
            keep.push(client);
            continue;
        };
        if only.is_some_and(|only| only != author) {
            keep.push(client);
            continue;
        }
        let author = author.clone();
        if !reporting.stepped {
            turns.observe_questions(&author, &reporting.report);
        }
        let g = gather(
            repo,
            state,
            &author,
            hosted,
            timer_backlogs,
            pending,
            routes,
        );
        let first = !reporting.stepped;
        let Some(frame) = turns.answer(&author, client.conn, &reporting.report, first, &g) else {
            keep.push(client);
            continue;
        };
        reporting.stepped = true;
        let parks = reporting.report.parks();
        if !frame.terminal(parks) {
            let note = if first {
                Note::Verdict(frame)
            } else {
                Note::Beat
            };
            if send_note(&client, &note) {
                keep.push(client);
            } else if let Some(writer) = client.writer {
                leaving.push(writer);
            }
            continue;
        }
        let presents = frame.presentation().is_some();
        // Nothing reached the carrier, so nothing can answer for the
        // presentation: it fails now, and what it held stays owed.
        if !send_note(&client, &Note::Verdict(frame)) && presents {
            turns.closed(&author, client.conn, &g);
        }
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
    }
    *clients = keep;
    leaving
}

/// The parked reports whose hook can wait no longer: each gets the verdict
/// its tool takes at its deadline, and is retired.
fn expire_reports(clients: &mut Vec<Client>, now: u64) -> Vec<tokio::task::JoinHandle<Wrote>> {
    let mut leaving = Vec::new();
    let (expired, kept): (Vec<Client>, Vec<Client>) =
        std::mem::take(clients).into_iter().partition(|c| {
            c.report
                .as_ref()
                .is_some_and(|r| r.stepped && r.report.deadline() <= now)
        });
    *clients = kept;
    for client in expired {
        let tool = client
            .report
            .as_ref()
            .map(|r| r.report.tool())
            .unwrap_or(clank_core::Tool::Claude);
        send_note(
            &client,
            &Note::Verdict(crate::cli::turn_custody::Turns::deadline_frame(tool)),
        );
        if let Some(writer) = client.writer {
            leaving.push(writer);
        }
    }
    leaving
}

/// The soonest deadline a held report's hook has.
fn report_deadline(clients: &[Client]) -> Option<u64> {
    clients
        .iter()
        .filter_map(|c| c.report.as_ref().filter(|r| r.stepped))
        .map(|r| r.report.deadline())
        .min()
}

/// Carry out a recovery transport's request: record a turn the harness
/// reported, or apply a helper's answer, for a session still bound. What
/// to tell the requester, and what recovery decided.
fn recovery_control(
    repo: &std::path::Path,
    recovery: &mut crate::cli::recovery::Driver,
    request: &Control,
    now: u64,
) -> Result<(String, Vec<crate::cli::recovery::Act>), String> {
    let bound = |author: &str, tool, session: &str| {
        let author =
            AgentLabel::parse(author).map_err(|e| format!("invalid author `{author}`: {e}"))?;
        match crate::agent_store::binding_matches(repo, &author, tool, session) {
            true => Ok(author),
            false => Err("this session no longer holds the label".to_string()),
        }
    };
    match request {
        Control::Observe {
            author,
            tool,
            session,
            turn,
        } => {
            let author = bound(author, *tool, session)?;
            let dir = crate::agent_store::agents_root(repo).join(author.as_str());
            crate::cli::recovery::observe(&dir, turn)
                .map_err(|e| format!("the turn could not be recorded: {e}"))?;
            Ok((
                crate::cli::recovery::retry_announcement(*tool).to_string(),
                Vec::new(),
            ))
        }
        Control::RecoveryReply {
            author,
            tool,
            reply,
        } => {
            let author = bound(author, *tool, &reply.session)?;
            Ok((
                "recorded".into(),
                recovery.answered(&author, reply.clone(), now),
            ))
        }
        _ => Err("not a recovery request".into()),
    }
}

/// Carry out an attend or clear request against `author`'s turn state.
fn turn_control(
    repo: &std::path::Path,
    turns: &mut crate::cli::turn_custody::Turns,
    request: &Control,
    gathered: impl FnOnce(&AgentLabel) -> crate::cli::turn_custody::Gathered,
) -> Result<(AgentLabel, String), String> {
    use clank_core::turn_policy::AttendRequest;
    let (author, request) = match request {
        Control::Attend {
            author,
            session,
            selector,
            desc,
            expect_ms,
            shell,
        } => (
            author,
            AttendRequest::Attend {
                session: session.clone(),
                selector: selector.clone(),
                desc: desc.clone(),
                expect: *expect_ms,
                shell: shell.clone(),
            },
        ),
        Control::Clear { author, session } => (
            author,
            AttendRequest::Clear {
                session: session.clone(),
            },
        ),
        Control::Restart { .. } | Control::Observe { .. } | Control::RecoveryReply { .. } => {
            return Err("not a turn request".into());
        }
    };
    let author =
        AgentLabel::parse(author).map_err(|e| format!("invalid author `{author}`: {e}"))?;
    crate::agent_store::resolve_role(repo, &author)
        .map_err(|e| format!("`{author}` is not on this repo's roster: {e:#}"))?;
    let g = gathered(&author);
    turns
        .attend(&author, request, &g)
        .map(|said| (author, said))
}

/// Send one note, and say whether that client may be kept.
///
/// `try_send`, never `send`: the owner must not wait on a client. A
/// full queue means this reader has fallen a whole `CLIENT_QUEUE` of
/// ticks behind, and a closed one means its writer task has already
/// given up — both are retirement. A frame that will not serialise is
/// this process's fault rather than the client's, so it costs nothing.
/// Whether `old` is the same agent session's wait as `new`: both an
/// agent's, for one author, with one and the same holder.
fn same_wait(old: &Client, new: &Client) -> bool {
    matches!(
        (&old.who, &new.who),
        (Served::Agent { author: a }, Served::Agent { author: b }) if a == b
    ) && old.holder.is_some()
        && old.holder == new.holder
}

/// Retire `client` because a newer wait of its session was admitted: its
/// writer sends what was queued, then `Superseded`, then ends.
fn supersede(client: Client) -> Option<tokio::task::JoinHandle<Wrote>> {
    if let (Some(farewell), Ok(line)) = (client.farewell, serde_json::to_string(&Note::Superseded))
    {
        let _ = farewell.send(format!("{line}\n"));
    }
    drop(client.out);
    client.writer
}

fn send_note(client: &Client, note: &Note) -> bool {
    let Ok(line) = serde_json::to_string(note) else {
        return true;
    };
    match client.out.try_send(format!("{line}\n")) {
        Ok(()) => true,
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
            eprintln!(
                "monitor: `{}` is {CLIENT_QUEUE} frames behind; dropping it",
                client.who.name()
            );
            false
        }
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
            eprintln!("monitor: `{}` is gone; dropping it", client.who.name());
            false
        }
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::cli::recovery::{Act, Answer, AttemptRef, Gate, Transport};

    fn client(who: Served, session: &str) -> (Client, tokio::sync::mpsc::Receiver<String>) {
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        (
            Client {
                who,
                out,
                writer: None,
                holder: Some(Holder {
                    tool: clank_core::Tool::OpenCode,
                    session: session.into(),
                }),
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            },
            queued,
        )
    }

    fn attempt() -> AttemptRef {
        AttemptRef {
            session: "session-one".into(),
            opened_by: "t1".into(),
            n: 1,
        }
    }

    fn dispatch(author: &AgentLabel) -> Act {
        Act::Dispatch {
            author: author.clone(),
            attempt: attempt(),
            transport: Transport::Helper,
            tag: "tag-1".into(),
            target: "t1".into(),
            text: crate::cli::recovery::wake_text("tag-1"),
        }
    }

    const STARTED: &str = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"T"}}"#;
    const DONE: &str =
        r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"T","error":null}}"#;
    /// Codex's empty input on its band, between blank band rows, above its
    /// footer.
    const CODEX_EMPTY: &str = "• done\n\n\u{1b}[48;5;236m   \u{1b}[49m\n\u{1b}[48;5;236m›    \u{1b}[49m\n\u{1b}[48;5;236m   \u{1b}[49m\n\u{1b}[m  ? for shortcuts";

    fn append(path: &std::path::Path, line: &str) {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    /// This test process, as an agent's running harness.
    fn this_process(version: &str) -> crate::cli::harness_identity::Running {
        let pid = std::process::id() as i32;
        crate::cli::harness_identity::Running {
            process: crate::cli::pane_target::Process {
                pid,
                birth: crate::proc_identity::token_for(pid).unwrap(),
                harness: Ok(version.into()),
            },
            version: version.into(),
        }
    }

    /// A repo whose codex master is bound, auto, idle and owing work (an
    /// event in its inbox), its first nudge claimed by the driver: what a
    /// typed send's last look is made against.
    struct Claimed {
        repo: tempfile::TempDir,
        codex: AgentLabel,
        record: std::path::PathBuf,
        driver: crate::cli::recovery::Driver,
        attempt: AttemptRef,
        /// The owed event, by its row.
        owed: (u64, String),
    }

    impl Claimed {
        fn new() -> Self {
            Self::with(Transport::Typed)
        }

        /// Claimed for `transport`.
        fn with(transport: Transport) -> Self {
            use crate::cli::github::log::{EventLog, Transport as Wal};
            crate::cli::recovery::tests::enable(clank_core::Tool::Codex, Some(transport));
            let repo = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
            std::fs::write(
                repo.path().join(".clank/config.json"),
                r#"{"agents":{"codex":{"tool":"codex","role":"master"}}}"#,
            )
            .unwrap();
            let codex = AgentLabel::parse("codex").unwrap();
            let timer = clank_core::agent_config::TimerSource {
                interrupt: false,
                interrupt_message: None,
                name: "check".into(),
                repeat_every: None,
                due_at: "1970-01-01T00:01:40Z".into(),
                content: Some("scheduled work".into()),
            };
            crate::agent_store::update_agent_config(repo.path(), &codex, |c| {
                c.auto_mode = Some(clank_core::AutoMode::On);
                c.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
                    timer.clone(),
                )];
            })
            .unwrap();
            crate::agent_store::bind_session_to_agent(
                repo.path(),
                &codex,
                clank_core::Tool::Codex,
                &clank_core::ids::SessionId::parse("session-one").unwrap(),
            )
            .unwrap();
            let events = crate::agent_store::agents_root(repo.path()).join("codex/events");
            std::fs::create_dir_all(&events).unwrap();
            let mut log = EventLog::open(&events, &timer.source_key(None)).unwrap();
            let seq = log
                .append_inbox(
                    Wal::Timer,
                    Some("timer/100"),
                    None,
                    Some(100),
                    &WaitItem::TimerEvent {
                        presentation: None,
                        name: "check".into(),
                        recurrence: None,
                        id: "timer/100".into(),
                        due_at: 100,
                        content: "scheduled work".into(),
                        selector: None,
                    },
                )
                .unwrap();
            let ident = log
                .read_rows()
                .unwrap()
                .and_then(|read| read.rows.into_iter().find(|row| row.seq == seq))
                .unwrap()
                .ident;
            let record = repo.path().join("rollout.jsonl");
            append(&record, &STARTED.replace("\"T\"", "\"t1\""));
            append(&record, &DONE.replace("\"T\"", "\"t1\""));
            let mut driver = crate::cli::recovery::Driver::new(
                repo.path().to_path_buf(),
                std::path::PathBuf::from("/nonexistent-home"),
            );
            let mut claimed = Claimed {
                repo,
                codex,
                record,
                driver: crate::cli::recovery::Driver::new("/".into(), "/".into()),
                attempt: attempt(),
                owed: (seq, ident),
            };
            driver.reconcile(vec![claimed.facts()], 0);
            let asks = driver.unidentified();
            let [(author, binding)] = asks.as_slice() else {
                panic!("{asks:?}");
            };
            driver.identified(author, binding, Ok(this_process("0.161.0")), 1);
            let acts = driver.due(10_000, |_, _| Gate::Go);
            let [Act::Dispatch { attempt, .. }] = acts.as_slice() else {
                panic!("{acts:?}");
            };
            claimed.attempt = attempt.clone();
            claimed.driver = driver;
            claimed
        }

        fn facts(&self) -> crate::cli::recovery::Facts {
            crate::cli::recovery::Facts {
                author: self.codex.clone(),
                agent_dir: crate::agent_store::agents_root(self.repo.path()).join("codex"),
                session: {
                    let bound =
                        crate::agent_store::load_agent_config(self.repo.path(), &self.codex)
                            .unwrap()
                            .unwrap()
                            .session
                            .unwrap();
                    let mut session = serde_json::to_value(bound).unwrap();
                    session["transcript"] = serde_json::json!(self.record);
                    serde_json::from_value(session).unwrap()
                },
                auto: true,
                delivering: false,
                obligated: true,
                role: "master".into(),
                work: vec!["the timer".into()],
                blind_nudge: None,
                attending: false,
                asking: false,
                identify: false,
                projection: Default::default(),
            }
        }

        /// A transport's request after its preparation, the repo folded as
        /// it is now, and the receiver of the loop's answer.
        async fn ask(
            &self,
            screen: Option<(clank_core::Tool, Option<String>)>,
        ) -> (Authorize, tokio::sync::oneshot::Receiver<bool>) {
            let (reply, answer) = tokio::sync::oneshot::channel();
            let a = Authorize {
                author: self.codex.clone(),
                attempt: self.attempt.clone(),
                process: screen.is_some().then(|| this_process("0.161.0").process),
                transport: match screen {
                    Some(_) => Transport::Typed,
                    None => Transport::CodexStart,
                },
                screen,
                state: folded(self.repo.path()).await.ok(),
                reply,
            };
            (a, answer)
        }

        /// The loop's answer to a typed send that read `screen`.
        async fn may_type(&mut self, clients: &[Client], screen: Option<&str>) -> bool {
            let typed = Some((clank_core::Tool::Codex, screen.map(str::to_string)));
            self.answer(clients, typed).await
        }

        /// The loop's answer to Codex's own `turn/start`, which reads no
        /// screen.
        async fn may_start(&mut self, clients: &[Client]) -> bool {
            self.answer(clients, None).await
        }

        async fn answer(
            &mut self,
            clients: &[Client],
            screen: Option<(clank_core::Tool, Option<String>)>,
        ) -> bool {
            let (a, answer) = self.ask(screen).await;
            authorize(
                self.repo.path(),
                clients,
                &Default::default(),
                &[],
                &mut self.driver,
                &mut crate::cli::turn_custody::Turns::new(self.repo.path().to_path_buf()),
                a,
            );
            answer.await.unwrap()
        }
    }

    /// The last look before typing is made from the live state on the loop
    /// and the repo as it is on disk: every change already made by then,
    /// folded or not, vetoes the send. What changes after it is the
    /// acknowledged race.
    #[tokio::test(flavor = "multi_thread")]
    async fn any_change_before_the_last_look_vetoes_a_typed_send() {
        assert!(Claimed::new().may_type(&[], Some(CODEX_EMPTY)).await);
        assert!(Claimed::with(Transport::CodexStart).may_start(&[]).await);
        assert!(
            !Claimed::new().may_start(&[]).await,
            "a typed claim is not sent by turn/start"
        );
        assert!(
            !Claimed::with(Transport::CodexStart)
                .may_type(&[], Some(CODEX_EMPTY))
                .await,
            "nor a turn/start claim by typing"
        );
        for (route, change) in [Transport::Typed, Transport::CodexStart]
            .into_iter()
            .flat_map(|route| {
                [
                    "draft",
                    "no screen",
                    "newer turn",
                    "auto off",
                    "deliverer",
                    "rebound",
                    "relaunched",
                    "block",
                    "off the roster",
                    "role on disk",
                    "work done on disk",
                    "work cleared at a fold",
                ]
                .map(|change| (route, change))
            })
            .filter(|(route, change)| {
                *route == Transport::Typed || !matches!(*change, "draft" | "no screen")
            })
        {
            let mut c = Claimed::with(route);
            let mut clients = Vec::new();
            let mut screen = Some(CODEX_EMPTY.to_string());
            match change {
                "draft" => screen = Some(CODEX_EMPTY.replace("›    ", "› half")),
                "no screen" => screen = None,
                "newer turn" => append(&c.record, &STARTED.replace("\"T\"", "\"t2\"")),
                "auto off" => {
                    crate::agent_store::update_agent_config(c.repo.path(), &c.codex, |cfg| {
                        cfg.auto_mode = Some(clank_core::AutoMode::Off);
                    })
                    .unwrap();
                }
                "deliverer" => clients.push(
                    client(
                        Served::Agent {
                            author: c.codex.clone(),
                        },
                        "session-one",
                    )
                    .0,
                ),
                "rebound" => {
                    crate::agent_store::bind_session_to_agent(
                        c.repo.path(),
                        &c.codex,
                        clank_core::Tool::Codex,
                        &clank_core::ids::SessionId::parse("session-two").unwrap(),
                    )
                    .unwrap();
                }
                "relaunched" => {
                    crate::agent_store::bind_session_to_agent(
                        c.repo.path(),
                        &c.codex,
                        clank_core::Tool::Codex,
                        &clank_core::ids::SessionId::parse("session-one").unwrap(),
                    )
                    .unwrap();
                }
                "block" => {
                    let blocks =
                        crate::agent_store::agents_root(c.repo.path()).join("codex/blocks");
                    std::fs::create_dir_all(&blocks).unwrap();
                    std::fs::write(blocks.join("which.md"), "Which way?").unwrap();
                }
                "off the roster" => std::fs::write(
                    c.repo.path().join(".clank/config.json"),
                    r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
                )
                .unwrap(),
                "role on disk" => std::fs::write(
                    c.repo.path().join(".clank/config.json"),
                    r#"{"agents":{"codex":{"tool":"codex","role":"commit"},"claude":{"tool":"claude","role":"master"}}}"#,
                )
                .unwrap(),
                "work done on disk" => {
                    let events = crate::agent_store::agents_root(c.repo.path()).join("codex/events");
                    let key = clank_core::agent_config::TimerSource {
                        interrupt: false,
                        interrupt_message: None,
                        name: "check".into(),
                        repeat_every: None,
                        due_at: "1970-01-01T00:01:40Z".into(),
                        content: Some("scheduled work".into()),
                    }
                    .source_key(None);
                    let mut log = crate::cli::github::log::EventLog::open(&events, &key).unwrap();
                    log.append_checked_ack(c.owed.0, &c.owed.1).unwrap();
                }
                _ => {
                    let f = crate::cli::recovery::Facts {
                        obligated: false,
                        ..c.facts()
                    };
                    c.driver.reconcile(vec![f], 10_001);
                }
            }
            let allowed = match route {
                Transport::Typed => c.may_type(&clients, screen.as_deref()).await,
                _ => c.may_start(&clients).await,
            };
            assert!(!allowed, "{route:?}: {change}");
        }
    }

    /// Drafts already in the input at the last look, on the prompt row or
    /// under blank rows of the input, are refused: a typed send writes only
    /// on the loop's yes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_draft_in_the_input_is_never_written_over() {
        let band = |l: &str| format!("\u{1b}[48;5;236m{l}   \u{1b}[49m");
        let screen = |rows: &[&str]| {
            let rows: Vec<String> = [""]
                .iter()
                .chain(rows)
                .chain(&[""])
                .map(|l| band(l))
                .collect();
            format!("• done\n\n{}\n\u{1b}[m  ? for shortcuts", rows.join("\n"))
        };
        for draft in [
            screen(&["› half"]),
            screen(&["› ", "", "  unfinished draft"]),
            screen(&["› ", "  draft", ""]),
        ] {
            let mut c = Claimed::new();
            assert!(!c.may_type(&[], Some(&draft)).await, "{draft:?}");
        }
    }

    /// A transport may mutate only on the loop's yes: a no, or a loop that
    /// is gone, is a refusal.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transport_waits_for_the_loops_yes() {
        let c = Claimed::new();
        for allow in [Some(true), Some(false), None] {
            let (asks, mut heard) = tokio::sync::mpsc::unbounded_channel::<Authorize>();
            let loop_side = tokio::spawn(async move {
                let a = heard.recv().await.unwrap();
                if let Some(allow) = allow {
                    let _ = a.reply.send(allow);
                }
            });
            let go = authorized(
                &asks,
                c.repo.path(),
                c.codex.clone(),
                c.attempt.clone(),
                None,
                None,
                Transport::CodexStart,
            )
            .await;
            loop_side.await.unwrap();
            assert_eq!(go, allow == Some(true), "{allow:?}");
        }
    }

    /// A recurring occurrence acknowledged on disk is no longer owed at the
    /// last look, though the monitor's own backlog would still list it
    /// until its next invalidation: the look reads the backlog afresh.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_acknowledged_recurring_occurrence_is_not_owed_at_the_last_look() {
        use crate::cli::github::log::EventLog;
        let mut c = Claimed::new();
        let events = crate::agent_store::agents_root(c.repo.path()).join("codex/events");
        let mut timer = clank_core::agent_config::TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: "check".into(),
            repeat_every: None,
            due_at: "1970-01-01T00:01:40Z".into(),
            content: Some("scheduled work".into()),
        };
        EventLog::open(&events, &timer.source_key(None))
            .unwrap()
            .append_checked_ack(c.owed.0, &c.owed.1)
            .unwrap();
        timer.name = "recurring".into();
        timer.repeat_every = Some("60s".into());
        let r = crate::cli::timer_source::recurrence::Recurrence::parse(&timer, None).unwrap();
        let mut log = EventLog::open(&events, &r.source_key()).unwrap();
        log.accept_recurring(&r, "recurring work", 100).unwrap();
        let row = log.read_rows().unwrap().unwrap().rows[0].clone();
        assert!(c.may_type(&[], Some(CODEX_EMPTY)).await, "owed");
        log.append_checked_ack(row.seq, &row.ident).unwrap();
        assert!(!c.may_type(&[], Some(CODEX_EMPTY)).await, "acknowledged");
    }

    fn records() -> crate::cli::recovery::RecordWatch {
        crate::cli::recovery::RecordWatch::spawn(tokio::sync::mpsc::unbounded_channel().0)
    }

    /// The authorized attempt reaches that session's helper and nobody
    /// else: not the agent's ordinary wait, and not a helper for another
    /// session of the same agent.
    #[tokio::test]
    async fn a_dispatch_reaches_only_that_sessions_helper() {
        let repo = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("opencode").unwrap();
        let (agent, mut agent_rx) = client(
            Served::Agent {
                author: author.clone(),
            },
            "session-one",
        );
        let (stale, mut stale_rx) = client(
            Served::Helper {
                author: author.clone(),
            },
            "session-two",
        );
        let (helper, mut helper_rx) = client(
            Served::Helper {
                author: author.clone(),
            },
            "session-one",
        );
        let clients = vec![agent, stale, helper];
        let (tx, mut answers) = tokio::sync::mpsc::unbounded_channel();
        let mut owned = OwnedTasks::default();
        act_on_recovery(
            repo.path(),
            &clients,
            vec![dispatch(&author)],
            &tx,
            &tokio::sync::mpsc::unbounded_channel().0,
            &records(),
            &mut owned,
        );

        let note: Note = serde_json::from_str(helper_rx.try_recv().unwrap().trim_end()).unwrap();
        assert_eq!(
            note,
            Note::Recover {
                opened_by: "t1".into(),
                n: 1,
                tag: "tag-1".into(),
                wake: crate::cli::recovery::wake_text("tag-1"),
            }
        );
        assert!(
            agent_rx.try_recv().is_err(),
            "an ordinary wait is never handed a retry"
        );
        assert!(stale_rx.try_recv().is_err(), "nor another session's helper");
        assert!(
            answers.try_recv().is_err(),
            "a delivered dispatch answers through its helper"
        );

        act_on_recovery(
            repo.path(),
            &[],
            vec![dispatch(&author)],
            &tx,
            &tokio::sync::mpsc::unbounded_channel().0,
            &records(),
            &mut owned,
        );
        assert_eq!(
            answers.try_recv().unwrap(),
            (author, attempt(), Answer::Unavailable)
        );
    }

    /// A helper is kept alive with beats and is never handed inbox work,
    /// so its presence is never mistaken for an agent waiting for some.
    #[tokio::test]
    async fn a_helper_is_served_beats_never_work() {
        let repo = tempfile::tempdir().unwrap();
        let author = AgentLabel::parse("opencode").unwrap();
        let held = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "x".into(),
            id: "x".into(),
            event: "x".into(),
            content: "work".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        let mut pending = vec![(author.clone(), held)];
        let (mut helper, mut helper_rx) = client(
            Served::Helper {
                author: author.clone(),
            },
            "session-one",
        );
        helper.holder = None;
        let mut clients = vec![helper];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let _ = serve_to(repo.path(), &mut clients, &state, None, &mut pending);
        let note: Note = serde_json::from_str(helper_rx.try_recv().unwrap().trim_end()).unwrap();
        assert_eq!(note, Note::Beat);
        assert_eq!(clients.len(), 1, "it stays parked");
        assert_eq!(pending.len(), 1, "the work is still held for a deliverer");

        // A helper for a session that does not own the agent is refused,
        // like any other client of a superseded session.
        let (stale, mut stale_rx) = client(
            Served::Helper {
                author: author.clone(),
            },
            "session-one",
        );
        let mut clients = vec![stale];
        let _ = serve_to(repo.path(), &mut clients, &state, None, &mut pending);
        let note: Note = serde_json::from_str(stale_rx.try_recv().unwrap().trim_end()).unwrap();
        assert!(matches!(note, Note::Refused { .. }), "{note:?}");
        assert!(clients.is_empty());
    }

    /// Obligation is what a newly arriving waiter would be handed, a
    /// deliverer is an ordinary wait, and a recovery helper is not one.
    #[tokio::test(flavor = "multi_thread")]
    async fn facts_name_who_owes_work_and_who_is_waiting_for_it() {
        use crate::cli::github::log::{EventLog, Transport as Wal};
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"},"codex":{"tool":"codex","role":"commit"}}}"#,
        )
        .unwrap();
        let codex = AgentLabel::parse("codex").unwrap();
        let timer = clank_core::agent_config::TimerSource {
            interrupt: false,
            interrupt_message: None,
            name: "check".into(),
            repeat_every: None,
            due_at: "1970-01-01T00:01:40Z".into(),
            content: Some("scheduled work".into()),
        };
        crate::agent_store::update_agent_config(repo.path(), &codex, |config| {
            config.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
                timer.clone(),
            )];
        })
        .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &codex,
            clank_core::Tool::Codex,
            &clank_core::ids::SessionId::parse("session-one").unwrap(),
        )
        .unwrap();
        let events = crate::agent_store::agents_root(repo.path()).join("codex/events");
        std::fs::create_dir_all(&events).unwrap();
        let mut log = EventLog::open(&events, &timer.source_key(None)).unwrap();
        let seq = log
            .append_inbox(
                Wal::Timer,
                Some("timer/100"),
                None,
                Some(100),
                &WaitItem::TimerEvent {
                    presentation: None,
                    name: "check".into(),
                    recurrence: None,
                    id: "timer/100".into(),
                    due_at: 100,
                    content: "scheduled work".into(),
                    selector: None,
                },
            )
            .unwrap();
        let state = folded(repo.path()).await.unwrap();
        let turns = std::cell::RefCell::new(crate::cli::turn_custody::Turns::new(
            repo.path().to_path_buf(),
        ));
        let routes = std::cell::RefCell::new(Routes::new());
        let facts = |clients: &[Client]| {
            recovery_facts(
                repo.path(),
                &state,
                clients,
                &Default::default(),
                &mut Default::default(),
                &[],
                &mut turns.borrow_mut(),
                &routes.borrow(),
            )
        };

        let alone = facts(&[]);
        assert_eq!(
            alone.len(),
            1,
            "an agent with no bound session is not considered"
        );
        assert!(alone[0].obligated && !alone[0].delivering, "{alone:?}");
        assert_eq!(alone[0].blind_nudge, None, "off unless configured");
        assert!(!alone[0].attending);
        // A question from a session no longer the agent's is over at the
        // next fold.
        let qdir = crate::agent_store::agents_root(repo.path()).join("codex");
        let bind = |tool, session| {
            crate::agent_store::bind_session_to_agent(
                repo.path(),
                &codex,
                tool,
                &clank_core::ids::SessionId::parse(session).unwrap(),
            )
            .unwrap()
        };
        bind(clank_core::Tool::Claude, "session-zero");
        crate::cli::question_state::observe(
            &qdir,
            [crate::cli::question_state::Observation::Asked {
                key: crate::cli::question_state::Key {
                    session: "session-zero".into(),
                    prompt: "p".into(),
                    tool_use: "t".into(),
                },
                header: "old".into(),
                input: "h".into(),
            }],
        );
        assert!(facts(&[])[0].asking, "its record is read for what ends it");
        bind(clank_core::Tool::Codex, "session-one");
        facts(&[]);
        assert!(
            crate::cli::question_state::load(&qdir)
                .unwrap()
                .pending
                .is_empty()
        );

        // A count that still describes the agent survives folds, a standing
        // block included; a change of session or of block clears it.
        let dir = crate::agent_store::agents_root(repo.path()).join("codex");
        std::fs::create_dir_all(dir.join("blocks")).unwrap();
        std::fs::write(dir.join("blocks/mic.md"), "Which microphone?").unwrap();
        let owed_now = || {
            owed_items(
                repo.path(),
                &state,
                &codex,
                &Default::default(),
                &mut Default::default(),
                &[],
                &crate::fs_plan_state_lookup::FsPlanStateLookup::new(
                    repo.path(),
                    state.head.as_ref(),
                ),
                crate::git_io::head_commit_at(repo.path(), &state).as_ref(),
            )
        };
        let owed = owed_now();
        assert!(
            identities(&owed).iter().any(|id| id.starts_with("blocked")) && owed.len() > 1,
            "the block and the event it does not withhold: {owed:?}"
        );
        let count = || {
            turns
                .borrow()
                .state(&codex)
                .and_then(|s| s.repeat.as_ref())
                .map(|r| r.count)
        };
        let deliver = |session: &str, n: u64| {
            let report = crate::cli::turn_wire::Report::TurnEnd {
                stop: clank_core::turn_policy::TurnEndReport {
                    tool: clank_core::Tool::Codex,
                    session: clank_core::SessionId::parse(session).unwrap(),
                    place: clank_core::turn_policy::TurnPlace {
                        turn: None,
                        position: Some(n),
                        human_turn: false,
                    },
                    tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
                },
                deadline: u64::MAX,
                questions: Vec::new(),
            };
            let g = crate::cli::turn_custody::Gathered::of(
                repo.path(),
                &codex,
                Some((&owed_now(), None)),
                Err("no route".into()),
            );
            let frame = turns
                .borrow_mut()
                .answer(&codex, n, &report, true, &g)
                .expect("folded");
            let presentation = frame.presentation().expect("a delivery").clone();
            turns
                .borrow_mut()
                .result(
                    &codex,
                    n,
                    crate::cli::turn_wire::TurnResult::Delivered { presentation },
                    &g,
                )
                .unwrap();
        };
        for n in 1..=4 {
            deliver("session-one", n);
            facts(&[]);
        }
        assert_eq!(count(), Some(4));
        let (waiter, _rx) = client(
            Served::Agent {
                author: codex.clone(),
            },
            "session-one",
        );
        facts(&[waiter]);
        assert_eq!(
            count(),
            Some(4),
            "an attached waiter does not make an unchanged count stale"
        );
        std::fs::remove_file(dir.join("blocks/mic.md")).unwrap();
        facts(&[]);
        assert_eq!(count(), None, "the block was answered");
        bind(clank_core::Tool::Codex, "session-two");
        facts(&[]);
        assert_eq!(count(), None, "another session's count");
        bind(clank_core::Tool::Codex, "session-one");

        // A qualifying attendance holds recovery back; a codex one never
        // qualifies, since nothing wakes it when its work completes.
        bind(clank_core::Tool::Claude, "session-one");
        routes.borrow_mut().insert(codex.clone(), Ok(()));
        let g = crate::cli::turn_custody::Gathered::of(
            repo.path(),
            &codex,
            Some((&owed_now(), None)),
            Ok(()),
        );
        turns
            .borrow_mut()
            .attend(
                &codex,
                clank_core::turn_policy::AttendRequest::Attend {
                    session: clank_core::SessionId::parse("session-one").unwrap(),
                    selector: clank_core::turn_policy::Selector::Name("sleeper".into()),
                    desc: None,
                    expect: 600_000,
                    shell: None,
                },
                &g,
            )
            .unwrap();
        let listing: Vec<clank_core::hook_io::BackgroundTask> = vec![
            serde_json::from_value(serde_json::json!({
                "id": "a1", "type": "subagent", "status": "running", "description": "sleeper",
            }))
            .unwrap(),
        ];
        let stop = crate::cli::turn_wire::Report::TurnEnd {
            stop: clank_core::turn_policy::TurnEndReport {
                tool: clank_core::Tool::Claude,
                session: clank_core::SessionId::parse("session-one").unwrap(),
                place: Default::default(),
                tasks: clank_core::turn_policy::TaskSnapshot::Known(listing),
            },
            deadline: u64::MAX,
            questions: Vec::new(),
        };
        let frame = turns
            .borrow_mut()
            .answer(&codex, 99, &stop, true, &g)
            .unwrap();
        let presentation = frame
            .presentation()
            .unwrap_or_else(|| panic!("work never delivered is not quieted: {frame:?}"))
            .clone();
        assert!(
            !facts(&[])[0].attending,
            "nor does it hold recovery back before it is received"
        );
        turns
            .borrow_mut()
            .result(
                &codex,
                99,
                crate::cli::turn_wire::TurnResult::Delivered { presentation },
                &g,
            )
            .unwrap();
        assert!(
            facts(&[])[0].attending,
            "a qualifying attendance holds recovery back over received work"
        );
        assert!(facts(&[])[0].projection.attended.is_some(), "and is shown");
        bind(clank_core::Tool::Codex, "session-three");
        assert!(
            !facts(&[])[0].attending,
            "a new binding drops the old one's attendance"
        );
        bind(clank_core::Tool::Codex, "session-one");

        let (helper, _rx) = client(
            Served::Helper {
                author: codex.clone(),
            },
            "session-one",
        );
        let with_helper = facts(&[helper]);
        assert!(
            with_helper[0].obligated && !with_helper[0].delivering,
            "a helper is not a deliverer"
        );

        let (waiter, _rx) = client(
            Served::Agent {
                author: codex.clone(),
            },
            "session-one",
        );
        let with_waiter = facts(&[waiter]);
        assert!(with_waiter[0].delivering, "an ordinary wait is a deliverer");

        let ident = log.read_rows().unwrap().unwrap().rows[0].ident.clone();
        log.append_checked_ack(seq, &ident).unwrap();
        let state = folded(repo.path()).await.unwrap();
        let handled = recovery_facts(
            repo.path(),
            &state,
            &[],
            &Default::default(),
            &mut Default::default(),
            &[],
            &mut turns.borrow_mut(),
            &routes.borrow(),
        );
        assert!(
            !handled[0].obligated,
            "handled work owes nothing: {handled:?}"
        );
    }

    /// A due attempt is decided against the agent as it is now: its
    /// binding, its deliverer, its auto mode and its attendance.
    #[test]
    fn the_gate_reads_the_agent_as_it_is_now() {
        let repo = tempfile::tempdir().unwrap();
        let codex = AgentLabel::parse("codex").unwrap();
        let turns = std::cell::RefCell::new(crate::cli::turn_custody::Turns::new(
            repo.path().to_path_buf(),
        ));
        let routes: Routes = [(codex.clone(), Ok(()))].into_iter().collect();
        let gate_owing = |clients: &[Client], session: &str, owed: &[WaitItem]| {
            recovery_gate(
                repo.path(),
                clients,
                &mut turns.borrow_mut(),
                &routes,
                &codex,
                session,
                owed,
            )
        };
        let gate_for = |clients: &[Client], session: &str| gate_owing(clients, session, &[]);
        let gate = |clients: &[Client]| gate_for(clients, "session-one");
        assert_eq!(gate(&[]), Gate::SessionChanged, "never bound");
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &codex,
            clank_core::Tool::Codex,
            &clank_core::ids::SessionId::parse("session-one").unwrap(),
        )
        .unwrap();
        crate::agent_store::set_auto_mode(repo.path(), &codex, clank_core::AutoMode::On).unwrap();
        assert_eq!(gate(&[]), Gate::Go);
        assert_eq!(gate_for(&[], "session-two"), Gate::SessionChanged);

        let (waiter, _rx) = client(
            Served::Agent {
                author: codex.clone(),
            },
            "session-one",
        );
        assert_eq!(gate(&[waiter]), Gate::Deliverer);

        crate::agent_store::set_auto_mode(repo.path(), &codex, clank_core::AutoMode::Off).unwrap();
        assert_eq!(gate(&[]), Gate::AutoOff);

        // A qualifying attendance holds recovery: a Claude session's
        // subagent, listed running at its last Stop.
        crate::agent_store::set_auto_mode(repo.path(), &codex, clank_core::AutoMode::On).unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &codex,
            clank_core::Tool::Claude,
            &clank_core::ids::SessionId::parse("session-one").unwrap(),
        )
        .unwrap();
        assert_eq!(gate(&[]), Gate::Go);
        let g =
            crate::cli::turn_custody::Gathered::of(repo.path(), &codex, Some((&[], None)), Ok(()));
        turns
            .borrow_mut()
            .attend(
                &codex,
                clank_core::turn_policy::AttendRequest::Attend {
                    session: clank_core::SessionId::parse("session-one").unwrap(),
                    selector: clank_core::turn_policy::Selector::Name("sleeper".into()),
                    desc: None,
                    expect: 600_000,
                    shell: None,
                },
                &g,
            )
            .unwrap();
        assert_eq!(
            gate(&[]),
            Gate::Go,
            "an intent quiets nothing until a Stop proves it"
        );
        let stop = crate::cli::turn_wire::Report::TurnEnd {
            stop: clank_core::turn_policy::TurnEndReport {
                tool: clank_core::Tool::Claude,
                session: clank_core::SessionId::parse("session-one").unwrap(),
                place: Default::default(),
                tasks: clank_core::turn_policy::TaskSnapshot::Known(vec![
                    serde_json::from_value(serde_json::json!({
                        "id": "a1", "type": "subagent", "status": "running",
                        "description": "sleeper",
                    }))
                    .unwrap(),
                ]),
            },
            deadline: u64::MAX,
            questions: Vec::new(),
        };
        turns
            .borrow_mut()
            .answer(&codex, 1, &stop, true, &g)
            .unwrap();
        assert_eq!(gate(&[]), Gate::Hold);
        turns
            .borrow_mut()
            .attend(
                &codex,
                clank_core::turn_policy::AttendRequest::Clear {
                    session: clank_core::SessionId::parse("session-one").unwrap(),
                },
                &g,
            )
            .unwrap();
        assert_eq!(gate(&[]), Gate::Go);

        // A question pending in the bound session holds recovery, with no
        // viewer anywhere; its answer releases it.
        let dir = crate::agent_store::agents_root(repo.path()).join("codex");
        let key = crate::cli::question_state::Key {
            session: "session-one".into(),
            prompt: "p1".into(),
            tool_use: "t1".into(),
        };
        crate::cli::question_state::observe(
            &dir,
            [crate::cli::question_state::Observation::Asked {
                key: key.clone(),
                header: "Which?".into(),
                input: "h".into(),
            }],
        );
        assert_eq!(gate(&[]), Gate::Hold);
        crate::cli::question_state::observe(
            &dir,
            [crate::cli::question_state::Observation::Result { key }],
        );
        assert_eq!(gate(&[]), Gate::Go);
    }

    #[test]
    fn a_helpers_hello_round_trips() {
        let who = Who::RecoveryHelper {
            author: "opencode".into(),
            holder: Holder {
                tool: clank_core::Tool::OpenCode,
                session: "session-one".into(),
            },
        };
        let wire = serde_json::to_string(&who).unwrap();
        assert!(wire.contains(r#""as":"recovery_helper""#), "{wire}");
        assert_eq!(serde_json::from_str::<Who>(&wire).unwrap(), who);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The descriptor is read VERSION FIRST, so a monitor of another
    /// version is a candidate rather than an absence.
    ///
    /// Serde parses a struct whole: a body whose shape changed fails
    /// before its `proto` can be read, and the reader cannot then tell
    /// "something is here that I cannot serve" from "nothing is here".
    /// The first is a holder to replace, the second a reason to start
    /// one, and a successor that confused them would spawn against a
    /// live monitor for ever.
    #[test]
    fn a_descriptor_of_another_version_is_a_holder_not_an_absence() {
        let repo = repo_with_roster();
        assert_eq!(published(repo.path()), Published::Nothing, "nothing yet");

        let d = describe(repo.path(), 4242, std::path::Path::new("/tmp/x.sock"));
        publish(repo.path(), &d).unwrap();
        assert_eq!(published(repo.path()), Published::Ours(d));

        // A shape this build does not share, carrying a number it can.
        std::fs::write(
            runtime_path(repo.path(), "json"),
            r#"{"proto":3,"pid":7,"repo":"/w","socket":"/tmp/old.sock","fold":{"tick":1,"at_ms":2}}"#,
        )
        .unwrap();
        assert_eq!(
            published(repo.path()),
            Published::Other {
                proto: 3,
                pid: Some(7),
                socket: Some("/tmp/old.sock".to_string()),
            },
            "an older monitor is something to replace, not nothing"
        );
        assert_eq!(
            read_published(repo.path()),
            None,
            "and an ordinary client still refuses it"
        );

        // Our own number on a body we cannot read is also not an
        // absence: something is there and it is not what it says.
        std::fs::write(
            runtime_path(repo.path(), "json"),
            format!(r#"{{"proto":{PROTO},"wrong":"shape"}}"#),
        )
        .unwrap();
        assert_eq!(
            published(repo.path()),
            Published::Other {
                proto: PROTO,
                pid: None,
                socket: None
            }
        );
    }

    /// A source channel nothing ever produced on, for the fixtures
    /// whose subject is not the source buffer.
    fn no_sources()
    -> tokio::sync::mpsc::UnboundedReceiver<(AgentLabel, crate::cli::event_source::Emission)> {
        tokio::sync::mpsc::unbounded_channel().1
    }

    /// A retirement gets the last frame OUT, transient included.
    ///
    /// The failure it excludes: `serve_to` only `try_send`s into a
    /// client's queue, the writer task does the writing, and a
    /// detached handle dies with the runtime — so returning from
    /// `start` right after serving could lose the frame. A durable row
    /// would replay from the WAL; a TRANSIENT has no row, and
    /// `serve_to` clears it from `pending` on send. An orderly
    /// retirement must not be the thing that manufactures a lost wake
    /// (codex on 294c18d).
    ///
    /// Driven over a real socket pair with the production `write_to`,
    /// and the proof is `drained`: only the writer REPORTING that its
    /// queue emptied says the bytes left.
    #[tokio::test]
    async fn a_retirement_writes_the_last_frame_before_it_goes() {
        use tokio::io::AsyncReadExt;

        let repo = repo_with_roster();
        let claude = AgentLabel::parse("claude").unwrap();
        let held = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "a".into(),
            event: "deploys".into(),
            content: "ci failed".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        let mut pending = vec![(claude.clone(), held)];

        let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let writer = tokio::spawn(write_to(ours, queued, "claude".to_string()));
        let mut clients = vec![Client {
            who: Served::Agent {
                author: claude.clone(),
            },
            out,
            writer: Some(writer),
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];

        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let leaving = serve_to(repo.path(), &mut clients, &state, None, &mut pending);
        assert!(
            pending.is_empty(),
            "the transient left `pending` on send, so only this retirement can still \
             deliver it"
        );
        assert!(
            clients.is_empty() && leaving.len() == 1,
            "the frame that WAKES a client also retires it, so its writer leaves \
             `clients` on that very frame — which is where the last frame was being \
             lost, one step earlier than it looked"
        );

        let decision = retire(
            clients,
            leaving,
            Vec::new(),
            Duration::from_secs(3),
            Duration::from_millis(500),
            0,
        )
        .await;
        assert_eq!(
            decision,
            Shutdown::ReadyToRelease(Retired {
                drained: 1,
                settled: true,
                ..Default::default()
            })
        );

        // Read AFTER the retirement returned: the bytes are already
        // there, which is what `drained` claims.
        let mut buf = Vec::new();
        theirs.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("ci failed"),
            "the transient must be in what was written: {text:?}"
        );
    }

    /// A writer that never finishes cannot hold the repo, and one that
    /// ENDED BADLY is not a delivery.
    ///
    /// The three non-success outcomes, each driven, because the first
    /// version counted every awaited handle as a success — so a
    /// panicked task and a peer that errored both reported as frames
    /// delivered (codex on d2b012d). The writers here are synthetic:
    /// a socket wedged or torn down at exactly the right moment is not
    /// portably arrangeable, and what is under test is the
    /// CLASSIFICATION, not the kernel.
    #[tokio::test]
    async fn a_task_that_ended_badly_is_never_counted_as_delivered() {
        let client = |writer: tokio::task::JoinHandle<Wrote>| {
            let (out, _queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
            Client {
                who: Served::Observer,
                out,
                writer: Some(writer),
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            }
        };

        let began = std::time::Instant::now();
        let decision = retire(
            vec![
                client(tokio::spawn(async { Wrote::Failed })),
                client(tokio::spawn(async { Wrote::TimedOut })),
                client(tokio::spawn(async { panic!("a writer came apart") })),
                client(tokio::spawn(std::future::pending::<Wrote>())),
            ],
            Vec::new(),
            Vec::new(),
            Duration::from_millis(150),
            Duration::from_millis(500),
            0,
        )
        .await;

        assert_eq!(
            decision,
            Shutdown::ReadyToRelease(Retired {
                drained: 0,
                failed: 2,
                lost: 1,
                abandoned: 1,
                undelivered: 0,
                settled: true,
            }),
            "not one of these wrote a frame"
        );
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "and the one that was never going to finish did not hold the repo"
        );
    }

    /// A peer that finished long ago is not abandoned because another
    /// one is stalled.
    ///
    /// Outcomes are collected as they ARRIVE, not in handle order.
    /// Awaiting the vector in sequence would block on the stalled
    /// writer for the whole budget and then class the finished one as
    /// abandoned at the deadline — reporting a delivered frame as a
    /// lost one (codex on 258d979).
    #[tokio::test]
    async fn a_finished_writer_behind_a_stalled_one_is_still_counted() {
        let client = |writer: tokio::task::JoinHandle<Wrote>| {
            let (out, _queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
            Client {
                who: Served::Observer,
                out,
                writer: Some(writer),
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            }
        };

        let decision = retire(
            vec![
                // The stalled one FIRST, so sequence order is the
                // wrong order.
                client(tokio::spawn(std::future::pending::<Wrote>())),
                client(tokio::spawn(async { Wrote::Drained })),
            ],
            Vec::new(),
            Vec::new(),
            Duration::from_millis(150),
            Duration::from_millis(500),
            0,
        )
        .await;

        assert_eq!(
            decision,
            Shutdown::ReadyToRelease(Retired {
                drained: 1,
                abandoned: 1,
                settled: true,
                ..Default::default()
            }),
            "the second writer's frame landed and must be reported as landed"
        );
    }

    /// A retirement with NO clients still waits for its producers.
    ///
    /// `abort` requests cancellation; it does not establish that the
    /// future and the resources it owns have been dropped. With no
    /// writers to await, a retirement could return immediately and
    /// `start` would release the lease while a producer was still
    /// winding down — a successor taking the repo on top of it
    /// (codex on d2b012d). The guard's `Drop` is the evidence.
    #[tokio::test]
    async fn a_retirement_with_no_clients_still_sees_its_producers_stop() {
        struct Guard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = dropped.clone();
        let producer = tokio::spawn(async move {
            let _guard = Guard(held);
            std::future::pending::<()>().await
        });
        // Let it reach its await point, so the guard is live inside the
        // task rather than still on this one's stack.
        tokio::task::yield_now().await;

        // As the transition does: stop them, then hand the handles on.
        let producers = Hosted {
            timers: None,
            spec: Vec::new(),
            tasks: [(
                "producer".to_string(),
                HostedTask {
                    instance: 0,
                    handle: producer,
                    stop: None,
                },
            )]
            .into(),
        }
        .stop();

        let decision = retire(
            Vec::new(),
            Vec::new(),
            producers,
            Duration::from_millis(150),
            Duration::from_millis(500),
            0,
        )
        .await;
        assert_eq!(
            decision,
            Shutdown::ReadyToRelease(Retired {
                settled: true,
                ..Default::default()
            })
        );
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "the producer's resources must be gone BEFORE the lease is released, and \
             nothing but awaiting the cancelled handle establishes that"
        );
    }

    /// A retirement takes EVERY kind of task the owner holds.
    ///
    /// The rule lives here rather than at the call site because the
    /// call site is inside `start`'s loop, which no test reaches: a
    /// mutation that left the handshakes behind survived until this
    /// moved. Both sets are drained, and what comes back is what the
    /// retirement will wait for.
    #[tokio::test]
    async fn the_owner_hands_over_every_kind_of_task() {
        let mut owned = OwnedTasks::default();
        owned.departing.push(tokio::spawn(async {}));
        owned.departing.push(tokio::spawn(async {}));
        owned.greeting.push(tokio::spawn(async {}));
        let repo = tempfile::tempdir().unwrap();
        owned
            .interrupting
            .notify(repo.path(), AgentLabel::parse("grok").unwrap());

        let all = owned.take_all();
        assert_eq!(
            all.len(),
            4,
            "producers, handshakes and interruption tasks all count"
        );
        assert!(
            owned.departing.is_empty()
                && owned.greeting.is_empty()
                && owned.interrupting.is_empty(),
            "and the owner keeps nothing back, or the retirement waits for less than it owns"
        );

        let decision = retire(
            Vec::new(),
            Vec::new(),
            all,
            Duration::from_millis(150),
            Duration::from_millis(500),
            0,
        )
        .await;
        assert!(
            matches!(decision, Shutdown::ReadyToRelease(_)),
            "{decision:?}"
        );
    }

    /// An admission that lands DURING the cutoff is still owned.
    ///
    /// A handshake that completed moments before the stand-down puts
    /// its client in the channel, not in `clients`. Left there it is
    /// dropped with a live writer still owning a socket — the detached
    /// task the whole inventory exists to prevent (codex on 056c991).
    ///
    /// Closing the channel first is what makes the drain finite, and
    /// the retirement afterwards is what proves the claim was worth
    /// making: the late client's writer reports `drained`, which only
    /// a writer this retirement OWNED could do.
    #[tokio::test]
    async fn an_admission_arriving_during_the_cutoff_is_claimed_and_flushed() {
        use tokio::io::AsyncReadExt;

        let (joined, mut arrivals) = tokio::sync::mpsc::unbounded_channel::<Greeted>();
        let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let writer = tokio::spawn(write_to(ours, queued, "late".to_string()));
        out.try_send("{\"t\":\"beat\"}\n".to_string()).unwrap();

        // As a finishing handshake does, after the stand-down was
        // decided but before anything was taken.
        joined
            .send(Greeted::Client(Client {
                who: Served::Observer,
                out,
                writer: Some(writer),
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            }))
            .unwrap();
        // And a second successor, which is answered on its own
        // connection and owes this one nothing.
        joined.send(Greeted::StandDown).unwrap();

        // Through the WHOLE transition, which is what `start` calls:
        // the cutoff and the retirement together. Driving the claim
        // alone left its only caller unproven.
        let decision = stand_down(
            std::path::Path::new("/w/proj"),
            Vec::new(),
            Vec::new(),
            Default::default(),
            OwnedTasks::default(),
            &mut arrivals,
            &mut no_sources(),
            None,
            &mut Vec::new(),
            None,
            Duration::from_secs(3),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            decision,
            Shutdown::ReadyToRelease(Retired {
                drained: 1,
                settled: true,
                ..Default::default()
            }),
            "a client claimed at the cutoff gets the same orderly flush as any other"
        );

        let mut buf = Vec::new();
        theirs.read_to_end(&mut buf).await.unwrap();
        assert!(
            String::from_utf8(buf).unwrap().contains("beat"),
            "and its queued frame actually went out"
        );
    }

    /// An emission the sources already handed over is still presented.
    ///
    /// Acknowledgment of acceptance precedes cancellation of the producer.
    /// A durable row replays from the WAL for
    /// whoever connects next; a transient has no row, so this is the
    /// one the cutoff has to carry (codex on 5f7bc36).
    ///
    /// The ordering is the content: production stops, the buffer is
    /// drained into `pending`, and only then is the presentation made
    /// — so the item is in hand before the set it is presented to is
    /// decided.
    #[tokio::test]
    async fn an_emission_accepted_before_the_cutoff_still_reaches_its_client() {
        use tokio::io::AsyncReadExt;

        let repo = repo_with_roster();
        let claude = AgentLabel::parse("claude").unwrap();
        let (_joined, mut arrivals) = tokio::sync::mpsc::unbounded_channel::<Greeted>();

        let (tx, mut from_sources) = tokio::sync::mpsc::unbounded_channel();
        let sink = crate::cli::event_source::EmissionSink::for_author(tx, claude.clone());
        let (accepted, acceptance) = tokio::sync::oneshot::channel();
        let producer = tokio::spawn(async move {
            sink.send((
                WaitItem::CommandEvent {
                    presentation: None,
                    source_cwd: None,
                    name: "deploys".into(),
                    id: "a".into(),
                    event: "deploys".into(),
                    content: "ci failed".into(),
                    at: None,
                    failure: false,
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
                crate::cli::event_source::Durability::Transient,
            ))
            .unwrap();
            accepted.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        acceptance.await.unwrap();
        let hosted = std::collections::BTreeMap::from([(
            claude.clone(),
            Hosted {
                timers: None,
                spec: Vec::new(),
                tasks: [(
                    "producer".to_string(),
                    HostedTask {
                        instance: 0,
                        handle: producer,
                        stop: None,
                    },
                )]
                .into(),
            },
        )]);

        let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let writer = tokio::spawn(write_to(ours, queued, "claude".to_string()));
        let clients = vec![Client {
            who: Served::Agent { author: claude },
            out,
            writer: Some(writer),
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];

        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let mut pending = Vec::new();
        let decision = stand_down(
            repo.path(),
            clients,
            Vec::new(),
            hosted,
            OwnedTasks::default(),
            &mut arrivals,
            &mut from_sources,
            Some(&state),
            &mut pending,
            None,
            Duration::from_secs(3),
            Duration::from_millis(500),
        )
        .await;

        let got = decision.report();
        assert_eq!(got.drained, 1, "the client's writer flushed: {got:?}");
        assert_eq!(
            got.undelivered, 0,
            "and the accepted emission is not residual work: it was presented"
        );

        let mut buf = Vec::new();
        theirs.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("ci failed"),
            "an emission taken off the channel must actually reach the peer: {text:?}"
        );
    }

    /// A greeting still running at the cutoff cannot welcome a peer
    /// and leave its writer behind.
    ///
    /// The race this excludes: the queue closes while the tasks that
    /// PRODUCE admissions are still live, so one finishes afterwards,
    /// sends `Welcome::Ok`, spawns a socket writer, fails to hand the
    /// client over, and the wrapper drops it — a child task nothing
    /// owns, after admission was supposed to be shut
    /// (codex on b37b6b6).
    ///
    /// Driven with a greeting held at a barrier, which is what an
    /// in-progress handshake IS to this transition: a task that has
    /// not finished and cannot be allowed to.
    #[tokio::test]
    async fn a_greeting_running_at_the_cutoff_leaves_no_writer_behind() {
        struct Guard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let (_joined, mut arrivals) = tokio::sync::mpsc::unbounded_channel::<Greeted>();
        let torn_down = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = torn_down.clone();

        let mut owned = OwnedTasks::default();
        owned.greeting.push(tokio::spawn(async move {
            // Stands for everything a handshake owns while it runs.
            let _socket = Guard(held);
            std::future::pending::<()>().await
        }));
        tokio::task::yield_now().await;

        let decision = stand_down(
            std::path::Path::new("/w/proj"),
            Vec::new(),
            Vec::new(),
            Default::default(),
            owned,
            &mut arrivals,
            &mut no_sources(),
            None,
            &mut Vec::new(),
            None,
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .await;

        assert!(
            matches!(decision, Shutdown::ReadyToRelease(_)),
            "the greeting was cancellable, so the release is orderly: {decision:?}"
        );
        assert!(
            torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "and what it held is gone BEFORE the lease could be released — a greeting \
             still live past the cutoff is an open admission path"
        );
    }

    /// A client admitted DURING the cutoff still receives the
    /// transient it is the only recipient for.
    ///
    /// The ordering error this excludes: the final presentation used
    /// to run in the caller, BEFORE late arrivals were claimed. A
    /// transient whose only eligible client was still in the queue was
    /// retained by that presentation, then dropped unpresented when
    /// the claim took the client and the retirement dropped its
    /// sender. Both the item and its recipient were in hand, and the
    /// item was reported undelivered (codex on b37b6b6).
    ///
    /// The client's queue starts EMPTY, as a real greeting leaves it:
    /// nothing is pre-queued, so the bytes at the peer can only come
    /// from the presentation this transition makes.
    #[tokio::test]
    async fn a_client_admitted_during_the_cutoff_still_gets_its_transient() {
        use tokio::io::AsyncReadExt;

        let repo = repo_with_roster();
        let claude = AgentLabel::parse("claude").unwrap();
        let (joined, mut arrivals) = tokio::sync::mpsc::unbounded_channel::<Greeted>();

        let (ours, mut theirs) = tokio::net::UnixStream::pair().unwrap();
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let writer = tokio::spawn(write_to(ours, queued, "claude".to_string()));
        joined
            .send(Greeted::Client(Client {
                who: Served::Agent {
                    author: claude.clone(),
                },
                out,
                writer: Some(writer),
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            }))
            .unwrap();

        let mut pending = vec![(
            claude,
            WaitItem::CommandEvent {
                presentation: None,
                source_cwd: None,
                name: "deploys".into(),
                id: "a".into(),
                event: "deploys".into(),
                content: "ci failed".into(),
                at: None,
                failure: false,
                requires_reply: false,
                reply_route: None,
                selector: None,
            },
        )];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());

        let decision = stand_down(
            repo.path(),
            // No client attached: the only eligible one is the
            // admission still sitting in the queue.
            Vec::new(),
            Vec::new(),
            Default::default(),
            OwnedTasks::default(),
            &mut arrivals,
            &mut no_sources(),
            Some(&state),
            &mut pending,
            None,
            Duration::from_secs(3),
            Duration::from_millis(500),
        )
        .await;

        let got = decision.report();
        assert_eq!(got.drained, 1, "the late client's writer flushed: {got:?}");
        assert_eq!(
            got.undelivered, 0,
            "and nothing is owed: the transition had both the item and its recipient"
        );

        let mut buf = Vec::new();
        theirs.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(
            text.contains("ci failed"),
            "the transient must actually reach the peer, not merely be counted: {text:?}"
        );
    }

    /// A task cancellation cannot reach forbids an orderly release.
    ///
    /// `settled: false` used to be a boolean: the unsettled path
    /// printed "releasing anyway" and then released anyway, which is a
    /// log line where a contract was wanted (codex on dd67677). The
    /// decision is exhaustive now, and this is the branch `start`
    /// answers by exiting WITH the lease held rather than unwinding
    /// it.
    ///
    /// The producer is a BLOCKING task parked on a channel, because
    /// `abort` only takes effect at an await point and a blocking task
    /// has none — which is precisely the case the contract exists for.
    /// It is released at the end so no thread is left behind.
    #[tokio::test]
    async fn a_task_cancellation_cannot_reach_forbids_an_orderly_release() {
        let (let_go, parked) = std::sync::mpsc::channel::<()>();
        let (running, started) = std::sync::mpsc::channel::<()>();
        let producer = tokio::task::spawn_blocking(move || {
            let _ = running.send(());
            let _ = parked.recv();
        });
        // It must be RUNNING before the retirement: a blocking task
        // the pool has not picked up yet is cancellable like any
        // other, which is the opposite of the case under test.
        tokio::task::spawn_blocking(move || started.recv())
            .await
            .unwrap()
            .unwrap();

        // As the transition does: stop them, then hand the handles on.
        let producers = Hosted {
            timers: None,
            spec: Vec::new(),
            tasks: [(
                "producer".to_string(),
                HostedTask {
                    instance: 0,
                    handle: producer,
                    stop: None,
                },
            )]
            .into(),
        }
        .stop();

        let decision = retire(
            Vec::new(),
            Vec::new(),
            producers,
            Duration::from_millis(50),
            Duration::from_millis(50),
            0,
        )
        .await;

        assert!(
            matches!(decision, Shutdown::RequiresProcessExit(_)),
            "a producer that has not stopped must not permit the orderly release that \
             drops the lease: {decision:?}"
        );
        let _ = let_go.send(());
    }

    /// What a reconcile CANCELS, it hands back.
    ///
    /// Both paths: an author whose spec changed, and one revoked from
    /// the roster. The previous body dropped these handles, so a
    /// source removed moments before a stand-down was still winding
    /// down while the retirement — looking only at the CURRENT map —
    /// reported everything settled (codex on dd67677).
    #[tokio::test]
    async fn a_reconcile_hands_back_the_producers_it_cancels() {
        let repo = repo_with_roster();
        with_source(repo.path(), "codex", "deploys");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hosted = std::collections::BTreeMap::new();

        let first = reconcile_hosting(repo.path(), &mut hosted, &tx);
        assert!(first.is_empty(), "nothing was running to cancel yet");
        assert_eq!(hosted.len(), 1, "and codex's source is hosted");

        // An edited spec replaces that author's set.
        with_source(repo.path(), "codex", "builds");
        let replaced = reconcile_hosting(repo.path(), &mut hosted, &tx);
        assert!(
            !replaced.is_empty(),
            "the producers it just cancelled are the caller's to see out"
        );

        // A revoked author's set goes the same way.
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        let revoked = reconcile_hosting(repo.path(), &mut hosted, &tx);
        assert!(hosted.is_empty(), "a revoked agent hosts nothing");
        assert!(
            !revoked.is_empty(),
            "and its producer is still winding down, so it comes back too"
        );

        // Retiring with that inventory observes them end, which is the
        // whole point of carrying it.
        let decision = retire(
            Vec::new(),
            Vec::new(),
            replaced.into_iter().chain(revoked).collect(),
            Duration::from_millis(150),
            Duration::from_millis(500),
            0,
        )
        .await;
        assert!(
            matches!(decision, Shutdown::ReadyToRelease(_)),
            "they stopped, so the release is orderly: {decision:?}"
        );
    }

    /// A producer removed JUST BEFORE a retirement is still this
    /// monitor's to see out.
    ///
    /// `reconcile_hosting` cancels it and the current hosted map stops
    /// mentioning it, so a retirement that looked only at that map
    /// would report everything settled while the removed one was still
    /// The whole mechanism: a monitor that declines a NEWER client is
    /// what an install replaced, so it goes — and one that declines an
    /// OLDER client keeps the repo.
    ///
    /// Both directions in one test, because getting them backwards is
    /// the only way this can be dangerous. The stale party is whoever
    /// speaks the lower number: older monitor, it leaves; older
    /// client, it does its own local wait exactly as before.
    #[test]
    fn a_monitor_leaves_for_a_newer_client_and_stays_for_an_older_one() {
        let repo = repo_with_roster();
        let hello = |proto: u32| {
            serde_json::to_string(&Hello {
                proto,
                repo: repo.path().to_string_lossy().to_string(),
                who: agent("codex"),
            })
            .unwrap()
        };

        for newer in [PROTO + 1, PROTO + 9] {
            match greeting_of(&hello(newer), repo.path()) {
                Greeting::Obsolete(Welcome::Decline { proto, .. }) => {
                    assert_eq!(proto, PROTO, "it answers as itself before it goes")
                }
                other => panic!("a client on {newer} replaces a monitor on {PROTO}: {other:?}"),
            }
        }

        for older in [PROTO - 1, 1] {
            assert!(
                matches!(
                    greeting_of(&hello(older), repo.path()),
                    Greeting::Decline(_)
                ),
                "an older client is the stale one; the monitor keeps the repo"
            );
        }

        // And our own version is served, which is the ordinary case
        // this must not disturb.
        assert!(matches!(
            greeting_of(&hello(PROTO), repo.path()),
            Greeting::Client(_)
        ));
    }

    /// The `user_command` silent reason is protocol 19: a protocol 18 hook,
    /// whose closed reason enum cannot decode it, is not admitted to a
    /// verdict carrying it, and a 19 client meeting an 18 monitor replaces
    /// it rather than relying on a release that monitor never makes.
    #[test]
    fn the_user_command_verdict_is_negotiated_as_protocol_19() {
        #[derive(Debug, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        #[allow(dead_code)]
        enum Proto18Reason {
            AutoOff,
            StaleBinding,
            OwnerGone,
            WaitArmed,
        }
        let wire = serde_json::to_string(&clank_core::SilentReason::UserCommand).unwrap();
        assert!(serde_json::from_str::<Proto18Reason>(&wire).is_err());
        assert_eq!(PROTO, 19);

        let repo = repo_with_roster();
        let hello = |proto: u32| {
            serde_json::to_string(&Hello {
                proto,
                repo: repo.path().to_string_lossy().to_string(),
                who: agent("codex"),
            })
            .unwrap()
        };
        assert!(
            matches!(greeting_of(&hello(18), repo.path()), Greeting::Decline(_)),
            "an 18 hook is not admitted"
        );
        assert!(
            !admits(PROTO, 18),
            "a 19 client does not share an 18 monitor"
        );
        assert!(matches!(
            greeting_of(&hello(19), repo.path()),
            Greeting::Client(_)
        ));
    }

    /// A higher version alone retires NOTHING.
    ///
    /// The hole this closes, and it was real: the first version
    /// re-read `Envelope.proto` and converted any greater number into
    /// a retirement, so a bare `{"proto":6}` — no repo, no identity,
    /// from anyone who could reach the socket — ended the repo's owner
    /// (codex on c4b5899). Each frame here carries a HIGHER proto, so
    /// the version is never what distinguishes them; only the routing
    /// and the identity differ.
    ///
    /// I had claimed an unknown author could not reach this decision.
    /// That was true of the equal-version path and false of the one
    /// that matters, which is why these all use `PROTO + 1`.
    #[test]
    fn a_higher_version_alone_retires_nothing() {
        let repo = repo_with_roster();
        let newer = PROTO + 1;
        let here = repo.path().to_string_lossy().to_string();

        // No repo and no identity at all.
        assert!(
            matches!(
                greeting_of(&format!(r#"{{"proto":{newer}}}"#), repo.path()),
                Greeting::Decline(_)
            ),
            "a bare version is not a client, and ends nothing"
        );

        // Addressed to a DIFFERENT repo.
        let elsewhere = serde_json::to_string(&Hello {
            proto: newer,
            repo: "/some/other/repo".into(),
            who: agent("codex"),
        })
        .unwrap();
        assert!(
            matches!(greeting_of(&elsewhere, repo.path()), Greeting::Decline(_)),
            "a newer client asking for another repo must not end this one's monitor"
        );

        // This repo, but nobody this repo knows.
        let ghost = format!(
            r#"{{"proto":{newer},"repo":{here:?},"who":{{"as":"agent","author":"ghost","holder":null,"has_cli_sources":false}}}}"#
        );
        match greeting_of(&ghost, repo.path()) {
            Greeting::Decline(Welcome::Decline { why, .. }) => {
                assert!(why.contains("roster"), "{why}")
            }
            other => panic!("an unknown author must not end a monitor: {other:?}"),
        }

        // A client KIND this build does not know.
        let alien =
            format!(r#"{{"proto":{newer},"repo":{here:?},"who":{{"as":"something_later"}}}}"#);
        assert!(
            matches!(greeting_of(&alien, repo.path()), Greeting::Decline(_)),
            "a client we cannot identify is not one whose word ends a repo's owner"
        );

        // And the one that SHOULD: this repo, a known agent, newer.
        let known = serde_json::to_string(&Hello {
            proto: newer,
            repo: here.clone(),
            who: agent("codex"),
        })
        .unwrap();
        assert!(
            matches!(greeting_of(&known, repo.path()), Greeting::Obsolete(_)),
            "addressed here, by somebody this repo knows, on a newer version"
        );
        // An observer too: it has no author to check, and it is still
        // a client of this repo on a newer build.
        let watcher = format!(r#"{{"proto":{newer},"repo":{here:?},"who":{{"as":"observer"}}}}"#);
        assert!(matches!(
            greeting_of(&watcher, repo.path()),
            Greeting::Obsolete(_)
        ));
    }

    /// The client REACHES an older holder, which is the whole point.
    ///
    /// `attach_by` used to refuse a descriptor whose proto differed
    /// before it connected, so the greeting that tells a holder to go
    /// was never sent and the self-exit clause never ran in production
    /// (codex on c4b5899). The descriptor is a hint about where to
    /// knock; the handshake settles the version.
    ///
    /// Driven through `attach_by` — the production discovery path —
    /// rather than by opening a socket by hand, which is what hid
    /// this.
    #[test]
    fn a_client_greets_a_holder_whose_descriptor_is_older() {
        use std::io::{BufRead, BufReader, Write};

        let repo = repo_dir();
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_path(dir.path(), 4499).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        publish(
            repo.path(),
            &Descriptor {
                proto: PROTO - 1,
                pid: 4499,
                repo: repo.path().to_string_lossy().to_string(),
                socket: socket.to_string_lossy().to_string(),
                build: None,
            },
        )
        .unwrap();

        // Bounded, because a test must not be able to WAIT FOR EVER on
        // the thing it is checking. A mutation that stopped the client
        // connecting left this `accept` blocked, the join with it, and
        // the mutation run hung holding the mutant in the tree — which
        // is the state this repo has learned to fear.
        listener
            .set_nonblocking(false)
            .and_then(|()| listener.try_clone())
            .unwrap();
        let heard = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let stream = loop {
                listener.set_nonblocking(true).unwrap();
                match listener.accept() {
                    Ok((s, _)) => break Some(s),
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break None,
                }
            };
            let Some(stream) = stream else {
                return String::new();
            };
            stream.set_nonblocking(false).unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let decline = Welcome::Decline {
                proto: PROTO - 1,
                why: "older".into(),
            };
            let _ = writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&decline).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
            line
        });

        let got = attach_by(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within);
        assert!(
            matches!(&got, Attached::Alone(_)),
            "an older monitor cannot serve us: {got:?}"
        );

        let greeting = heard.join().unwrap();
        assert!(
            !greeting.is_empty(),
            "the client must actually have connected and greeted the older holder"
        );
        let open: Opening = serde_json::from_str(greeting.trim_end())
            .expect("the holder received a greeting it can read at the floor");
        assert_eq!(
            open.proto, PROTO,
            "and it carries OUR version, which is what tells it to go"
        );
    }

    /// ONLY a version mismatch ends it.
    ///
    /// A client turned away for its repo, or for anything else,
    /// says nothing about this monitor — and if such a decline
    /// ended it, any stranger could end a repo's owner by asking
    /// wrongly. The declines here are all for OUR OWN proto, so the
    /// version is never in question and the reason is the only thing
    /// that differs.
    #[test]
    fn only_a_version_mismatch_ends_a_monitor() {
        let repo = repo_with_roster();
        let elsewhere = serde_json::to_string(&Hello {
            proto: PROTO,
            repo: "/some/other/repo".into(),
            who: agent("codex"),
        })
        .unwrap();
        assert!(
            matches!(greeting_of(&elsewhere, repo.path()), Greeting::Decline(_)),
            "asking for another repo must not retire this one's monitor"
        );

        let ghost = serde_json::to_string(&Hello {
            proto: PROTO,
            repo: repo.path().to_string_lossy().to_string(),
            who: agent("ghost"),
        })
        .unwrap();
        // An author off the roster is refused LATER, by `served_for`,
        // so it never reaches this decision at all — the same
        // guarantee arrived at from the other side.
        assert!(
            matches!(greeting_of(&ghost, repo.path()), Greeting::Client(_)),
            "the roster is not this decision's business"
        );

        assert!(
            matches!(
                greeting_of("not a frame at all", repo.path()),
                Greeting::Decline(_)
            ),
            "and an unreadable frame carries no version, so it decides nothing"
        );
    }

    /// Over a real socket: the client is TOLD why, and the accept path
    /// reports a retirement rather than a client.
    ///
    /// The answer goes out before the process does, so a client is not
    /// left reading an abrupt EOF and guessing whether it was refused
    /// or the monitor died.
    #[tokio::test]
    async fn an_obsolete_monitor_answers_before_it_stands_down() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let repo = repo_with_roster();
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4488).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let line = serde_json::to_string(&Hello {
            proto: PROTO + 1,
            repo: repo.path().to_string_lossy().to_string(),
            who: agent("codex"),
        })
        .unwrap();

        let heard = tokio::spawn(async move {
            let mut c = tokio::net::UnixStream::connect(&path).await.unwrap();
            c.write_all(format!("{line}\n").as_bytes()).await.unwrap();
            let mut answer = String::new();
            tokio::io::BufReader::new(c)
                .read_line(&mut answer)
                .await
                .unwrap();
            answer
        });

        let (stream, _) = listener.accept().await.unwrap();
        let greeted = greet(stream, repo.path()).await.expect("it answers");
        assert!(
            matches!(greeted, Greeted::StandDown),
            "a newer client is not a client this monitor can serve: {greeted:?}"
        );

        let answer = heard.await.unwrap();
        match serde_json::from_str::<Welcome>(answer.trim_end()) {
            Ok(Welcome::Decline { proto, why }) => {
                assert_eq!(proto, PROTO);
                assert!(
                    why.contains("proto"),
                    "it says what the mismatch was: {why}"
                );
            }
            other => panic!("the client must be told, not dropped: {other:?}"),
        }
    }

    /// winding down (codex on dd67677). The handles it hands back are
    /// the inventory that closes it.
    #[tokio::test]
    async fn a_producer_removed_just_before_a_retirement_is_still_waited_for() {
        struct Guard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }

        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = dropped.clone();
        let producer = tokio::spawn(async move {
            let _guard = Guard(held);
            std::future::pending::<()>().await
        });
        tokio::task::yield_now().await;

        // Exactly what `reconcile_hosting` returns when an author's
        // sources change or its roster entry goes.
        let departing = Hosted {
            timers: None,
            spec: Vec::new(),
            tasks: [(
                "producer".to_string(),
                HostedTask {
                    instance: 0,
                    handle: producer,
                    stop: None,
                },
            )]
            .into(),
        }
        .stop();

        let decision = retire(
            Vec::new(),
            Vec::new(),
            departing,
            Duration::from_millis(150),
            Duration::from_millis(500),
            0,
        )
        .await;

        assert!(
            matches!(decision, Shutdown::ReadyToRelease(_)),
            "it did stop, so the release is orderly: {decision:?}"
        );
        assert!(
            dropped.load(std::sync::atomic::Ordering::SeqCst),
            "and a producer the CURRENT map no longer mentions must still have been \
             seen out before the lease could go"
        );
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
            build: None,
        };
        let back: Descriptor = serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(back, d);
    }

    fn repo_dir() -> TempRepo {
        TempRepo::new()
    }

    fn descriptor_for(repo: &std::path::Path) -> Descriptor {
        Descriptor {
            proto: PROTO,
            pid: 4242,
            repo: repo.to_string_lossy().to_string(),
            socket: "/tmp/clank-socks-501/4242.sock".into(),
            build: None,
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
                    Ok(_) => Welcome::Ok { proto: PROTO },
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
                build: None,
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
            serde_json::to_string(&Welcome::Ok { proto: PROTO }).unwrap()
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
                turn_end: None,
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
                turn_end: None,
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
                build: None,
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
                build: None,
            },
        )
        .unwrap();
        let foreign = attach(repo.path(), agent("codex"));
        assert!(
            matches!(&foreign, Attached::Alone(why) if why.contains("serves /w/elsewhere")),
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
            source_cwd: None,
            items: vec![WaitItem::PromoteFromQueue {
                name: "next".into(),
                priority: 500,
            }],
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

        let first = serde_json::to_string(&Note::Beat).unwrap();
        let second = serde_json::to_string(&Note::Beat).unwrap();
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
            Note::Beat,
            "the prefix survived the deadline"
        );
        assert_eq!(
            conn.note(std::time::Instant::now() + Duration::from_secs(2))
                .expect("and the next frame is intact"),
            Note::Beat,
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
        assert!(crate::signal::alive(std::process::id() as i32));
        assert!(!crate::signal::alive(0), "no process is pid 0");
        assert!(
            !crate::signal::alive(-1),
            "and -1 is every process, not one"
        );
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
            build: None,
        };
        (dir, d)
    }

    /// An agent client, for the tests whose subject is not identity.
    fn agent(author: &str) -> Who {
        Who::Agent {
            author: author.into(),
            holder: None,
            has_cli_sources: false,
            turn_end: None,
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
            |r| acquire_lease(r).is_ok(),
        )
        .await;

        assert!(matches!(got, Attached::Shared(_)), "{got:?}");
        assert_eq!(
            starts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one start is enough when it works: the retry attaches rather than \
             starting again"
        );
    }

    /// The runtime namespace is derived, not created, by a LOOKUP.
    ///
    /// Discovery is read-only: it must be able to ask where a
    /// descriptor would be without making a directory to find out, and
    /// without write access (codex on 2a0c59a).
    #[test]
    fn a_lookup_creates_nothing() {
        let repo = repo_dir();
        let at = runtime_path(repo.path(), "json");
        assert!(at.is_absolute());
        assert!(!at.starts_with(repo.path()), "and never inside the repo");

        // Against a namespace that does NOT exist, because the real
        // one does: other tests created it, so asking there proves
        // nothing either way.
        let fresh = tempfile::tempdir().unwrap();
        let absent = fresh.path().join("not-made-yet");
        let at = runtime_path_in(&absent, repo.path(), "json");
        assert!(
            !absent.exists(),
            "deriving a path must not create the directory it is in"
        );
        assert!(at.starts_with(&absent));
        // And a read of a missing descriptor is an absence, not a
        // reason to make anything.
        assert_eq!(published(repo.path()), Published::Nothing);
    }

    /// A descriptor from a namespace that is not ours never becomes an
    /// address.
    ///
    /// The read half of the trust boundary, and the half this plan
    /// nearly shipped without. The descriptor used to live in a repo
    /// we owned; it now lives at a PREDICTABLE path under
    /// world-writable `/tmp`, so another uid can get there first and
    /// leave one naming the right repo and proto with `socket`
    /// pointing at theirs. Matching fields are not evidence of who
    /// wrote them, and refusing to PUBLISH into a planted namespace
    /// does nothing about consuming what is already in it (codex on
    /// 9dfc3e3).
    ///
    /// Driven through `look_at_in` with a connect SPY, because the vet
    /// returning the right answer proves nothing about whether
    /// discovery asks it: the bug is a client that connects.
    #[test]
    fn a_planted_descriptor_never_reaches_connect() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let repo = repo_dir();
        let me = unsafe { libc::getuid() };
        let d = describe(repo.path(), 4242, std::path::Path::new("/tmp/planted.sock"));
        let home = tempfile::tempdir().unwrap();

        let tries = AtomicUsize::new(0);
        let spy = |_: &std::path::Path, _: std::time::Instant| {
            tries.fetch_add(1, SeqCst);
            Err::<std::os::unix::net::UnixStream, _>(std::io::Error::other("the spy never dials"))
        };
        let budget = Duration::from_millis(50);
        let plant = |dir: &std::path::Path, mode: u32| {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                runtime_path_in(dir, repo.path(), "json"),
                serde_json::to_vec(&d).unwrap(),
            )
            .unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
        };

        // Writable by anyone, so anyone could have left that descriptor.
        let open = home.path().join("open");
        plant(&open, 0o777);
        assert!(
            matches!(
                published_in(&open, repo.path(), me),
                Published::Untrusted(_)
            ),
            "a namespace others can write is not evidence"
        );
        assert!(matches!(
            look_at_in(&open, repo.path(), agent("codex"), budget, &spy),
            Seen::Unproven(_)
        ));

        // A symlink INTO a directory we own, which reads back as ours
        // unless the final component is left unfollowed — and whose
        // owner may replace it later even under a sticky `/tmp`.
        let real = home.path().join("real");
        plant(&real, 0o700);
        let link = home.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(
            matches!(
                published_in(&link, repo.path(), me),
                Published::Untrusted(_)
            ),
            "an indirection is refused on the strength of the link, not its target"
        );
        assert!(matches!(
            look_at_in(&link, repo.path(), agent("codex"), budget, &spy),
            Seen::Unproven(_)
        ));

        // Missing is an ordinary absence, and reading makes nothing.
        let absent = home.path().join("absent");
        assert_eq!(published_in(&absent, repo.path(), me), Published::Nothing);
        assert!(!absent.exists(), "a read must not create the namespace");
        assert!(matches!(
            look_at_in(&absent, repo.path(), agent("codex"), budget, &spy),
            Seen::Vacant(_)
        ));

        assert_eq!(
            tries.load(SeqCst),
            0,
            "no descriptor from an untrusted or absent namespace may be dialled"
        );

        // And one we DO own is still used, so the refusals above are
        // the boundary working rather than discovery being broken.
        assert!(matches!(
            published_in(&real, repo.path(), me),
            Published::Ours(_)
        ));
        let _ = look_at_in(&real, repo.path(), agent("codex"), budget, &spy);
        assert!(
            tries.load(SeqCst) > 0,
            "a trusted descriptor is an address, and gets knocked on"
        );
    }

    /// EVERY mutation boundary in the runtime namespace vets it.
    ///
    /// One rule, driven at all three call sites rather than at the
    /// helper, because a boundary that skips the vet is the bug and the
    /// helper cannot tell you whether anyone called it.
    ///
    /// It matters more here than it did in the repo. `/tmp` is
    /// world-writable, so a directory another uid plants there is a
    /// directory we would otherwise write into — and the descriptor is
    /// the DISCOVERY record, so theirs would name their socket and
    /// every client would connect to it. 0o777 is also what a 002
    /// umask leaves behind, so the same fixture is both the attack and
    /// the accident: an unvetted `create_dir_all` could make the
    /// namespace at a mode every vetted caller then refuses forever.
    #[test]
    fn every_mutation_boundary_refuses_a_namespace_it_cannot_vet() {
        let repo = repo_dir();
        let me = unsafe { libc::getuid() };
        let d = describe(repo.path(), 4242, std::path::Path::new("/tmp/x.sock"));

        let home = tempfile::tempdir().unwrap();
        let open = home.path().join("open");
        std::fs::create_dir(&open).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            !dir_is_acceptable(me, 0o777, me),
            "the fixture plants a mode the vet rejects"
        );

        let refused = publish_in(repo.path(), &open, me, &d).expect_err("no descriptor");
        assert!(
            format!("{refused}").contains("mode"),
            "and it says what was wrong: {refused}"
        );
        let refused = acquire_lease_in(repo.path(), &open, me).expect_err("no lease");
        assert!(
            matches!(&refused, LeaseError::Unverifiable(why) if why.contains("mode")),
            "an unreadable namespace is never owned: {refused:?}"
        );
        let refused = take_repair_permit_in(repo.path(), &open, me);
        assert!(
            matches!(&refused, Repair::Unavailable(why) if why.contains("mode")),
            "{refused:?}"
        );

        assert!(
            std::fs::read_dir(&open).unwrap().next().is_none(),
            "and not one of them left a file there: no client can be pointed at a \
             planted socket, and no lease was taken beside somebody else's"
        );

        // A namespace they accept gets everything, so the refusals
        // above are the vet working and not these three being broken.
        let mine = home.path().join("mine");
        publish_in(repo.path(), &mine, me, &d).expect("its own 0o700 namespace is fine");
        let _lease = acquire_lease_in(repo.path(), &mine, me).expect("and the lease");
        assert!(matches!(
            take_repair_permit_in(repo.path(), &mine, me),
            Repair::Acquired(_)
        ));
        assert!(runtime_path_in(&mine, repo.path(), "json").exists());
    }

    /// The supported upgrade, and the rollback's HONEST limit.
    ///
    /// This is a cutover: after it, a binary predating it is not
    /// supported running concurrently, and two monitors could own one
    /// repo in that configuration. The plan does not claim otherwise,
    /// so the test does not either — it records what rollback actually
    /// does rather than leaving the limitation to a sentence somebody
    /// may not read (codex on c0ebe23).
    #[test]
    fn the_cutover_is_exclusive_among_supported_binaries_and_not_across_it() {
        let repo = repo_dir();
        ensure_runtime_dir().unwrap();

        // Supported: two of THIS version cannot both own a repo.
        let held = acquire_lease(repo.path()).expect("the first owns it");
        assert!(
            acquire_lease(repo.path()).is_err(),
            "one monitor per repo, which is the whole authority"
        );
        drop(held);
        assert!(
            acquire_lease(repo.path()).is_ok(),
            "and it frees when the owner goes"
        );

        // Across the cutover: a pre-cutover binary locked
        // `.clank/monitor.lock`, which this one never touches. Both
        // can hold their own. Asserted so the limitation is a recorded
        // fact rather than a claim nobody checked.
        let legacy = repo.path().join(".clank").join("monitor.lock");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let old = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&legacy)
            .unwrap();
        use std::os::fd::AsRawFd;
        // SAFETY: valid owned fd; flock has no memory effects.
        let rc = unsafe { libc::flock(old.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "a pre-cutover binary takes the old path");
        assert!(
            acquire_lease(repo.path()).is_ok(),
            "and this one takes the new one regardless — which is why a pre-cutover \
             binary must be retired at install rather than left running"
        );
    }

    /// The gate has ONE identity, whatever the environment says.
    ///
    /// Socket allocation may try candidates because the published
    /// descriptor then tells everyone which was chosen. A coordination
    /// lock has no such discovery, so two clients differing only in
    /// `XDG_RUNTIME_DIR` each took a permit in a different directory,
    /// locked different inodes, and both forked (codex on 88b6234).
    #[test]
    fn the_runtime_namespace_does_not_move_with_the_environment() {
        assert_eq!(
            runtime_dir(501),
            std::path::PathBuf::from("/tmp/clank-run-501"),
            "fixed per uid, and derived from nothing else"
        );
        assert_ne!(
            runtime_dir(501),
            runtime_dir(502),
            "but not shared between users"
        );

        // The socket's candidates DO move with the environment, which
        // is exactly why the gate must not be derived from them.
        let with = socket_dirs(501, Some(std::path::Path::new("/run/user/501")));
        let without = socket_dirs(501, None);
        assert_ne!(with, without, "the socket list is environment-dependent");

        // And the production path USES that fixed namespace: the gate
        // file lands there. Asserting only that `repair_dir` is fixed
        // left the wiring free to derive the directory some other way,
        // which a mutation duly did.
        let repo = repo_dir();
        let me = unsafe { libc::getuid() };
        let gate = runtime_dir(me).join(format!("{}.repair", repo_tag(repo.path())));
        let _ = std::fs::remove_file(&gate);
        let held = take_repair_permit(repo.path());
        assert!(matches!(held, Repair::Acquired(_)), "{held:?}");
        assert!(
            gate.exists(),
            "the gate every contender must find is at {}",
            gate.display()
        );
    }

    /// A namespace that cannot be trusted is UNAVAILABLE, not busy.
    ///
    /// The vetting is part of this lock's correctness rather than a
    /// security aside: a gate file replaced while a holder keeps its
    /// old fd lets the next caller lock a different inode at the same
    /// name, and then both are repairing. And "unusable" is not
    /// "somebody else has it" — reporting one as the other is a guess
    /// stated as a fact.
    #[test]
    fn an_untrusted_namespace_is_unavailable_rather_than_busy() {
        let repo = repo_dir();
        let me = unsafe { libc::getuid() };

        let dir = tempfile::tempdir().unwrap();
        let shared = dir.path().join("shared");
        std::fs::create_dir_all(&shared).unwrap();
        // World-writable: anyone could replace the gate beside us.
        std::fs::set_permissions(&shared, std::os::unix::fs::PermissionsExt::from_mode(0o777))
            .unwrap();

        match take_repair_permit_in(repo.path(), &shared, me) {
            Repair::Unavailable(why) => assert!(!why.is_empty(), "it says what was wrong"),
            other => panic!("a directory anyone can write is not a gate: {other:?}"),
        }
    }

    /// The repo digest is stable across builds.
    ///
    /// `DefaultHasher` was wrong twice over: Rust leaves its algorithm
    /// unspecified between releases — which bites exactly when two
    /// installed binaries contend during an upgrade — and
    /// `to_string_lossy` is not the path. Pinned to a known value so a
    /// change to the scheme has to be deliberate.
    #[test]
    fn the_repo_digest_is_pinned_and_byte_exact() {
        let tag = repo_tag(std::path::Path::new("/w/proj"));
        assert_eq!(tag.len(), 32);
        assert_eq!(
            tag,
            blake3::hash(b"/w/proj").to_hex()[..32].to_string(),
            "blake3 over the path's raw bytes, and nothing else"
        );
        assert_ne!(tag, repo_tag(std::path::Path::new("/w/proj2")));
    }

    /// An older peer's reply must be READABLE before it means
    /// anything.
    ///
    /// A bare `{"proto":4}` and an unknown body are not evidence about
    /// retirement, and the first version read every older envelope as
    /// "standing down" — then started a monitor on the strength of it.
    /// Even a well-formed decline only establishes an older peer:
    /// proto 3 and 4 cannot self-exit, and the wire does not
    /// distinguish refusal from retirement progress.
    #[test]
    fn an_unreadable_older_reply_establishes_nothing() {
        let repo = repo_dir();
        let older = PROTO - 1;

        for answer in [
            format!("{{\"proto\":{older}}}\n"),
            format!("{{\"proto\":{older},\"t\":\"something_later\"}}\n"),
        ] {
            let (_dir, _served) = fake_monitor(repo.path(), Some(answer.clone()));
            match look_at(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within) {
                Seen::Unproven(why) => assert!(why.contains("cannot read"), "{answer}: {why}"),
                other => panic!("{answer} establishes nothing: {other:?}"),
            }
        }

        // A decline we CAN read is an older peer — named for what it
        // is, not for a retirement nobody confirmed.
        let decline = format!(
            "{}\n",
            serde_json::to_string(&Welcome::Decline {
                proto: older,
                why: "older".into(),
            })
            .unwrap()
        );
        let (_dir, _served) = fake_monitor(repo.path(), Some(decline));
        assert!(matches!(
            look_at(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within),
            Seen::OlderPeer { .. }
        ));
    }

    /// A proto-16 monitor is retired, never served. Its `unblocked` work
    /// carries no answer identity, which this build can neither decode nor
    /// receive on delivery, so the handshake must turn it away before any
    /// of its frames is read and treat it as an older peer to replace.
    #[test]
    fn a_proto_16_monitor_is_an_older_peer_to_replace() {
        const OLD: u32 = 16;
        assert!(!admits(PROTO, OLD));
        let old_items = r#"{"t":"items","items":[{"kind":"unblocked","name":"q","answer":"yes"}]}"#;
        assert!(
            serde_json::from_str::<Note>(old_items).is_err(),
            "a 16 monitor's answer is not one this build could deliver"
        );

        let repo = repo_dir();
        let dir = tempfile::tempdir().unwrap();
        let path = socket_path(dir.path(), 4616).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        publish(
            repo.path(),
            &Descriptor {
                proto: OLD,
                pid: 4616,
                repo: repo.path().to_string_lossy().to_string(),
                socket: path.to_string_lossy().to_string(),
                build: None,
            },
        )
        .unwrap();
        let served = std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let open: Opening = serde_json::from_str(line.trim_end()).unwrap();
            let decline = Welcome::Decline {
                proto: OLD,
                why: obsolete_why(OLD, open.proto),
            };
            writeln!(
                reader.get_mut(),
                "{}",
                serde_json::to_string(&decline).unwrap()
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(200));
            open.proto
        });
        match look_at(repo.path(), agent("codex"), ATTACH_BUDGET, connect_within) {
            Seen::OlderPeer { obsolete, .. } => {
                assert!(obsolete, "it agreed it is obsolete, so it is replaced")
            }
            other => panic!("a proto-16 monitor must not be shared: {other:?}"),
        }
        assert_eq!(
            served.join().unwrap(),
            PROTO,
            "greeted with this build's version"
        );
    }

    /// ONE repair per repo: a second client does not also start one.
    ///
    /// The canonical lease stops two monitors OWNING a repo and says
    /// nothing about how many clients may poll for it or fork a child
    /// at it — each could take and drop it and each then start one,
    /// which is a poller and a fork per waiting client (codex on
    /// cf29219). The permit is flocked so it coalesces ACROSS
    /// processes, every parked wait being its own.
    #[tokio::test]
    async fn only_one_client_repairs_a_repo_at_a_time() {
        let repo = repo_dir();
        let dir = tempfile::tempdir().unwrap();

        let mine = repair_permit(repo.path(), dir.path())
            .expect("the gate is usable")
            .expect("nobody else is on it");
        assert!(
            repair_permit(repo.path(), dir.path()).unwrap().is_none(),
            "a second client must not become a second repairer"
        );

        // Another repo is another repair, which is the point of the
        // tag: one in flight must not block an unrelated one.
        let other = repo_dir();
        assert!(repair_permit(other.path(), dir.path()).unwrap().is_some());

        drop(mine);
        assert!(
            repair_permit(repo.path(), dir.path()).unwrap().is_some(),
            "and it frees when the repairer is done"
        );
    }

    /// The permit is HELD while the startup it covers is happening.
    ///
    /// Releasing it when the lease is taken rather than when the work
    /// is done lets the next client in while this one is still
    /// starting a monitor — which is the fork-per-client this exists
    /// to prevent, arriving a few milliseconds later. Asked from
    /// INSIDE the spawn, which is the only moment that distinguishes
    /// the two.
    #[tokio::test]
    async fn the_permit_is_still_held_while_the_startup_runs() {
        let repo = repo_dir();
        let free_during_spawn = std::sync::atomic::AtomicBool::new(false);

        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            Duration::from_millis(40),
            Duration::from_millis(40),
            SHARE_ATTEMPTS,
            |r| {
                if matches!(take_repair_permit(r), Repair::Acquired(_)) {
                    free_during_spawn.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
            |r| acquire_lease(r).is_ok(),
        )
        .await;

        assert!(matches!(got, Attached::Alone(_)), "{got:?}");
        assert!(
            !free_during_spawn.load(std::sync::atomic::Ordering::SeqCst),
            "another client could have become a second repairer mid-startup"
        );
    }

    /// A deadline already gone is never readiness.
    ///
    /// The wait took the lease BEFORE checking the time, so an
    /// expired deadline with a free lease returned Ready and the
    /// caller started a child with no further check — as could a timer
    /// that resumed after the deadline with the lease freed meanwhile
    /// (codex on cf29219).
    #[tokio::test]
    async fn an_expired_deadline_is_never_ready_however_free_the_lease_is() {
        let repo = repo_dir();
        // Nothing holds it: the ONLY thing that can say Busy here is
        // the expiry, which is the point.
        assert!(acquire_lease(repo.path()).is_ok());

        let gone = std::time::Instant::now() - Duration::from_millis(1);
        assert_eq!(
            wait_for_ownership(repo.path(), gone, Duration::from_millis(5)).await,
            Ownership::Busy,
            "a free lease past the deadline is still not permission to start"
        );
    }

    /// Waiting for ownership is bounded by the DEADLINE, with no
    /// count in it.
    ///
    /// A retry count ended the wait after about 450ms with production
    /// settings, while a holder standing down legitimately has its
    /// retirement budget plus settlement. Here the lease is released
    /// well past any small number of steps, and the wait must still
    /// see it.
    #[tokio::test]
    async fn waiting_for_ownership_is_bounded_by_time_not_by_tries() {
        let repo = repo_dir();
        let held = acquire_lease(repo.path()).expect("the test owns it first");
        let releasing = tokio::task::spawn_blocking(move || {
            std::thread::sleep(Duration::from_millis(400));
            drop(held);
        });

        // Twenty-odd steps before the release: a wait that gave up on
        // a handful of tries would never reach it.
        let by = std::time::Instant::now() + Duration::from_secs(3);
        let got = wait_for_ownership(repo.path(), by, Duration::from_millis(15)).await;
        assert_eq!(got, Ownership::Ready);
        releasing.await.unwrap();

        // And a lease that never frees is Busy rather than a wait
        // without end.
        let still = acquire_lease(repo.path()).unwrap();
        let began = std::time::Instant::now();
        let by = std::time::Instant::now() + Duration::from_millis(80);
        assert_eq!(
            wait_for_ownership(repo.path(), by, Duration::from_millis(15)).await,
            Ownership::Busy
        );
        assert!(began.elapsed() < Duration::from_secs(1));
        drop(still);
    }

    /// The whole handoff fits ONE deadline, including its last
    /// handshake.
    ///
    /// Each attach used to take a fresh budget, so an attempt entering
    /// its handshake just before the deadline could block for another
    /// whole `ATTACH_BUDGET` — and an outer check on the deadline
    /// could not make that bounded (codex on 1c0ce9d). The connect
    /// here outlasts any budget a test sets, so a fresh one is
    /// measurable.
    #[tokio::test]
    async fn the_handoff_fits_one_deadline_including_its_last_handshake() {
        let repo = repo_dir();
        let (_dir, d) = unannounced_monitor(repo.path());

        let began = std::time::Instant::now();
        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            // A big per-attach budget against a small overall one, so
            // the two are far apart.
            Duration::from_millis(400),
            Duration::from_millis(60),
            SHARE_ATTEMPTS,
            |r| publish(r, &d).map(|()| Box::new(NotStarted) as Box<dyn Started>),
            |_path, deadline: std::time::Instant| {
                // Stands in for a connect against a full backlog: it
                // uses the whole deadline it was GIVEN and then
                // reports the timeout. That is what makes a fresh
                // budget measurable — a connect that slept a fixed
                // time would take the same either way, which is how
                // the first version of this fixture missed it.
                std::thread::sleep(deadline.saturating_duration_since(std::time::Instant::now()));
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the connect did not finish",
                ))
            },
            |r| acquire_lease(r).is_ok(),
        )
        .await;
        let took = began.elapsed();

        assert!(matches!(got, Attached::Alone(_)), "{got:?}");
        assert!(
            took < Duration::from_millis(650),
            "the retries share what is LEFT of one deadline; a fresh 400ms budget each \
             would run far past it: {took:?}"
        );
    }

    /// Ownership that frees and then the deadline passing starts
    /// NOTHING — through the production path, not just the probe.
    ///
    /// The probe is a seam because taking the real lease is too fast
    /// to arrange that gap by scheduling, and a guard nothing can
    /// drive is a guard nobody has checked.
    #[tokio::test]
    async fn readiness_arriving_past_the_deadline_starts_no_monitor() {
        let repo = repo_dir();
        let started = std::sync::atomic::AtomicUsize::new(0);

        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            Duration::from_millis(20),
            Duration::from_millis(20),
            SHARE_ATTEMPTS,
            |_r| {
                started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
            // Succeeds, but only after the deadline has gone.
            |_r| {
                std::thread::sleep(Duration::from_millis(120));
                true
            },
        )
        .await;

        match got {
            Attached::Alone(why) => assert!(why.contains("ran out of time"), "{why}"),
            other => panic!("nothing was started, so nothing can be shared: {other:?}"),
        }
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "ownership confirmed after the deadline is not permission to start"
        );
    }

    /// Ownership that frees and then the deadline passing starts
    /// NOTHING.
    ///
    /// Checking expiry before the probe is necessary and does not
    /// cover a probe that FINISHED after the deadline, or this task
    /// resuming past it before startup (codex on 06021ee). The probe
    /// is a seam because taking the real lease is too fast to arrange
    /// that gap by scheduling, and a guard nothing can drive is a
    /// guard nobody has checked.
    #[tokio::test]
    async fn readiness_that_arrives_too_late_is_not_permission() {
        let repo = repo_dir();
        let by = std::time::Instant::now() + Duration::from_millis(40);

        // Succeeds — but only after the deadline has gone.
        let got = wait_for_ownership_by(repo.path(), by, Duration::from_millis(5), |_| {
            std::thread::sleep(Duration::from_millis(60));
            true
        })
        .await;
        assert_eq!(
            got,
            Ownership::Ready,
            "the probe did take it, which is what makes the guard after it necessary"
        );
        assert!(
            by.saturating_duration_since(std::time::Instant::now())
                .is_zero(),
            "and the deadline is gone by the time it says so"
        );
    }

    /// The whole handoff, end to end: an older holder goes and this
    /// client ends up SHARING a current one.
    ///
    /// The acceptance demonstrated here, in order: the old holder is
    /// NOTIFIED; it keeps the canonical lease well beyond the obsolete
    /// count-based give-up for the parameters actually supplied; it
    /// releases; exactly one useful replacement starts; the client
    /// gets `Shared`.
    ///
    /// Both halves of that mattered and the first version had neither.
    /// It released at 250ms while passing `settle = 2000` — so the
    /// old four-step wait would have had 500ms steps and reached it
    /// on the first one, meaning a restored retry count would still
    /// have passed (codex on 685e022). And it started its countdown
    /// from the test's own clock, so a slow start could find the
    /// holder already gone before any notification: the interval is
    /// tied to an observed GREETING now.
    #[tokio::test]
    async fn an_older_holder_goes_and_the_client_shares_its_replacement() {
        use std::io::{BufRead, BufReader, Write};

        // Ordered, not timed. This used to be budget 800ms + settle
        // 400ms against a release at 600ms, leaving ~600ms for the
        // client to take the lease, bind, publish and handshake — and
        // it duly failed once in four full-suite runs, on the run that
        // coincided with two heavy concurrent builds. A margin chosen
        // to survive contention asserts this machine's arithmetic, not
        // the rule, and widening it would have hidden a real slowdown
        // (codex on fbb84e3).
        //
        // The holder now stands down because the CLIENT asked one
        // time more than the old retry count would have, so the thing
        // under test is the ordering and the deadline is only a
        // backstop against a wedge.
        let budget = Duration::from_secs(5);
        let settle = Duration::from_millis(40);
        let stand_down_on = SHARE_ATTEMPTS as usize + 1;

        let repo = repo_dir();
        let dir = tempfile::tempdir().unwrap();
        let old_sock = socket_path(dir.path(), 4801).unwrap();
        let old = std::os::unix::net::UnixListener::bind(&old_sock).unwrap();
        publish(
            repo.path(),
            &Descriptor {
                proto: PROTO - 1,
                pid: 4801,
                repo: repo.path().to_string_lossy().to_string(),
                socket: old_sock.to_string_lossy().to_string(),
                build: None,
            },
        )
        .unwrap();
        // The old monitor OWNS the repo, as a real one does.
        let lease = acquire_lease(repo.path()).expect("the old holder owns it");

        let greeted_at = std::sync::Arc::new(std::sync::Mutex::new(None));
        let noticed = greeted_at.clone();
        // Raised by the client's own ownership probe, below.
        let retire_now = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let asked = retire_now.clone();
        let serving = std::thread::spawn(move || {
            // It keeps answering until it goes: a client greets it
            // more than once — on discovery, and again after taking
            // the repair gate — and a holder that answered only the
            // first would look like one that died mid-frame.
            old.set_nonblocking(true).unwrap();
            let decline = serde_json::to_string(&Welcome::Decline {
                proto: PROTO - 1,
                why: "older".into(),
            })
            .unwrap();
            loop {
                match old.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let mut reader = BufReader::new(stream);
                        let mut line = String::new();
                        let _ = reader.read_line(&mut line);
                        // A valid greeting from a newer client is what
                        // starts the clock: the retirement interval is
                        // measured from NOTIFICATION, not from the
                        // test's own start.
                        if serde_json::from_str::<Opening>(line.trim_end())
                            .is_ok_and(|o| o.proto > PROTO - 1)
                        {
                            let mut at = noticed.lock().unwrap();
                            at.get_or_insert_with(std::time::Instant::now);
                        }
                        let _ = writeln!(reader.get_mut(), "{decline}");
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
                // It goes when the client has asked for ownership
                // once more than the old count would have allowed —
                // and only after it has actually been greeted, so the
                // retirement is still something this monitor LEARNED
                // rather than something the test did to it.
                let greeted = noticed.lock().unwrap().is_some();
                if greeted && asked.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
            }
            // Stands down: the lease is what says it has gone.
            drop(lease);
            drop(old);
        });

        // The replacement: takes ownership and serves this version.
        let new_dir = tempfile::tempdir().unwrap();
        let new_sock = socket_path(new_dir.path(), 4802).unwrap();
        let started = std::sync::atomic::AtomicUsize::new(0);
        let probes = std::sync::atomic::AtomicUsize::new(0);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let replacement = std::sync::Arc::new(std::sync::Mutex::new(None));
        let keep = replacement.clone();
        let halt = stop.clone();
        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            budget,
            settle,
            SHARE_ATTEMPTS,
            |r| {
                started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let held = acquire_lease(r).map_err(std::io::Error::other)?;
                let listener = std::os::unix::net::UnixListener::bind(&new_sock)?;
                listener.set_nonblocking(true)?;
                publish(
                    r,
                    &Descriptor {
                        proto: PROTO,
                        pid: 4802,
                        repo: r.to_string_lossy().to_string(),
                        socket: new_sock.to_string_lossy().to_string(),
                        build: None,
                    },
                )
                .map_err(std::io::Error::other)?;
                let halt = halt.clone();
                let handle = std::thread::spawn(move || {
                    let _own = held;
                    while !halt.load(std::sync::atomic::Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                stream.set_nonblocking(false).unwrap();
                                let mut reader = BufReader::new(stream);
                                let mut line = String::new();
                                let _ = reader.read_line(&mut line);
                                let ok =
                                    serde_json::to_string(&Welcome::Ok { proto: PROTO }).unwrap();
                                let _ = writeln!(reader.get_mut(), "{ok}");
                            }
                            Err(_) => std::thread::sleep(Duration::from_millis(5)),
                        }
                    }
                });
                *keep.lock().unwrap() = Some(handle);
                Ok(Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
            |r| {
                // The ASK is the signal: one probe past the count the
                // old implementation stopped at.
                if probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == stand_down_on {
                    retire_now.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                acquire_lease(r).is_ok()
            },
        )
        .await;

        assert!(
            matches!(got, Attached::Shared(_)),
            "the client must end up sharing the replacement: {got:?}"
        );
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "and exactly one monitor was started to get there"
        );
        assert!(
            greeted_at.lock().unwrap().is_some(),
            "the holder retired because a newer client GREETED it, not because the \
             test reached in and dropped its lease"
        );
        let asks = probes.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            asks > SHARE_ATTEMPTS as usize,
            "and the client kept asking past the old retry count, which is the bound \
             the deadline replaced: {asks} ask(s)"
        );

        // Both fixture servers are owned and ended, so neither
        // outlives the test holding a lease or a socket.
        serving.join().unwrap();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(h) = replacement.lock().unwrap().take() {
            h.join().unwrap();
        }
    }

    /// A second contender does not become a second repairer.
    ///
    /// It takes the documented local fallback while the first holds
    /// the gate, and starts nothing — the whole point of coalescing.
    #[tokio::test]
    async fn a_second_contender_starts_nothing_while_the_first_repairs() {
        let repo = repo_dir();
        let me = unsafe { libc::getuid() };
        let gate = match take_repair_permit_in(repo.path(), &runtime_dir(me), me) {
            Repair::Acquired(p) => p,
            other => panic!("the first contender takes it: {other:?}"),
        };

        let started = std::sync::atomic::AtomicUsize::new(0);
        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            Duration::from_millis(50),
            Duration::from_millis(50),
            SHARE_ATTEMPTS,
            |_r| {
                started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
            |r| acquire_lease(r).is_ok(),
        )
        .await;

        match got {
            Attached::Alone(why) => assert!(why.contains("already starting one"), "{why}"),
            other => panic!("it has nothing to share yet: {other:?}"),
        }
        assert_eq!(
            started.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a second repairer is the fork-per-client this gate exists to prevent"
        );
        drop(gate);
    }

    /// Ownership that never frees starts NOTHING.
    ///
    /// This fixture used to hold the lease for the whole call and
    /// REQUIRE a start, which is exactly the bug: all three ways out
    /// of the wait — acquired, retries exhausted, deadline gone — fell
    /// through to the same spawn, so a failed ownership confirmation
    /// started a monitor that could not own anything (codex on
    /// 1c0ce9d). The right answer for a lease that stays held is a
    /// bounded local fallback.
    #[tokio::test]
    async fn ownership_that_never_frees_starts_nothing() {
        let repo = repo_dir();
        let held = acquire_lease(repo.path()).expect("the test owns it first");
        let starts = std::sync::atomic::AtomicUsize::new(0);

        let began = std::time::Instant::now();
        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            Duration::from_millis(50),
            Duration::from_millis(40),
            SHARE_ATTEMPTS,
            |_r| {
                starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
            |r| acquire_lease(r).is_ok(),
        )
        .await;
        let took = began.elapsed();

        match got {
            Attached::Alone(why) => assert!(why.contains("lease is still held"), "{why}"),
            other => panic!("nothing can serve this repo yet: {other:?}"),
        }
        assert_eq!(
            starts.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a monitor started against a held lease loses it and exits, having \
             helped nobody"
        );
        assert!(
            took < Duration::from_secs(2),
            "and it gave up inside its own deadline: {took:?}"
        );
        drop(held);
    }

    /// Ownership that frees DURING the wait is taken.
    ///
    /// The case the wait exists for: a holder standing down may keep
    /// its lease well past the old retry COUNT, and the client must
    /// still be the one that starts the replacement once it goes.
    ///
    /// The release is ordered, not timed. It used to be a 250ms
    /// `thread::sleep` racing a 650ms deadline, which asserted the
    /// arithmetic of this machine rather than the rule: the lease
    /// frees on the probe after the old count would have given up, so
    /// a build that went back to count-bounding returns `Busy` before
    /// the release can happen and this test fails for the right reason
    /// (codex on fbb84e3).
    #[tokio::test]
    async fn ownership_that_frees_during_the_wait_starts_one() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

        let repo = repo_dir();
        let held = std::sync::Mutex::new(Some(
            acquire_lease(repo.path()).expect("the test owns it first"),
        ));
        let starts = AtomicUsize::new(0);
        let probes = AtomicUsize::new(0);

        // One past the count, so "waited longer than the old budget"
        // is a fact about ORDER rather than about how loaded the
        // machine is.
        let release_on = SHARE_ATTEMPTS as usize + 1;

        let got = attach_or_spawn_by(
            repo.path(),
            agent("codex"),
            // Generous and bounded: the deadline exists so a hung
            // handoff cannot wedge the suite, and it is deliberately
            // far from the decision this test makes.
            Duration::from_secs(5),
            Duration::from_millis(40),
            SHARE_ATTEMPTS,
            |_r| {
                starts.fetch_add(1, SeqCst);
                Ok(Box::new(NotStarted) as Box<dyn Started>)
            },
            connect_within,
            |r| {
                if probes.fetch_add(1, SeqCst) + 1 == release_on {
                    drop(held.lock().unwrap().take());
                }
                acquire_lease(r).is_ok()
            },
        )
        .await;

        assert!(matches!(got, Attached::Alone(_)), "{got:?}");
        assert_eq!(
            starts.load(SeqCst),
            1,
            "the repo became free, so this client is the one that starts its monitor"
        );
        assert!(
            probes.load(SeqCst) > SHARE_ATTEMPTS as usize,
            "and it kept asking past the old retry count, which is the bound this \
             replaced: {} probe(s)",
            probes.load(SeqCst)
        );
    }

    /// A monitor that should KEEP the repo gets no replacement at all.
    ///
    /// Newer than us, or refusing this client on its merits: either
    /// way a second monitor would own nothing and refuse us the same
    /// way. Starting one per park against a healthy newer owner was
    /// the cost that made this classification necessary.
    #[tokio::test]
    async fn a_holder_that_keeps_the_repo_gets_no_replacement() {
        use std::io::{BufRead, BufReader, Write};

        for answer in [
            Welcome::Decline {
                proto: PROTO + 1,
                why: "we are newer".into(),
            },
            Welcome::Decline {
                proto: PROTO,
                why: "`codex` is not on this repo's roster".into(),
            },
        ] {
            let repo = repo_dir();
            let dir = tempfile::tempdir().unwrap();
            let socket = socket_path(dir.path(), 4777).unwrap();
            let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            publish(
                repo.path(),
                &Descriptor {
                    proto: PROTO,
                    pid: 4777,
                    repo: repo.path().to_string_lossy().to_string(),
                    socket: socket.to_string_lossy().to_string(),
                    build: None,
                },
            )
            .unwrap();
            let line = serde_json::to_string(&answer).unwrap();
            std::thread::spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    let mut reader = BufReader::new(stream);
                    let mut discard = String::new();
                    let _ = reader.read_line(&mut discard);
                    let _ = writeln!(reader.get_mut(), "{line}");
                    std::thread::sleep(Duration::from_millis(30));
                }
            });

            let starts = std::sync::atomic::AtomicUsize::new(0);
            let got = attach_or_spawn_by(
                repo.path(),
                agent("codex"),
                Duration::from_millis(200),
                Duration::from_millis(40),
                SHARE_ATTEMPTS,
                |_r| {
                    starts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Box::new(NotStarted) as Box<dyn Started>)
                },
                connect_within,
                |r| acquire_lease(r).is_ok(),
            )
            .await;

            assert!(matches!(got, Attached::Alone(_)), "{got:?}");
            assert_eq!(
                starts.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{answer:?} means the repo has an owner; starting one helps nobody"
            );
        }
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
            |r| acquire_lease(r).is_ok(),
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
            |r| acquire_lease(r).is_ok(),
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
            |r| acquire_lease(r).is_ok(),
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
                build: None,
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
            |r| acquire_lease(r).is_ok(),
        )
        .await;

        assert!(matches!(got, Attached::Alone(_)), "{got:?}");
        assert_eq!(
            tries.load(std::sync::atomic::Ordering::SeqCst),
            SHARE_ATTEMPTS as usize + 1,
            "bounded by the limit, not by the park's patience — plus the ONE re-read \
             after taking the repair permit, which exists because a winner may have \
             appeared while this client queued for it"
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

    fn with_timer(repo: &std::path::Path, name: &str, due: u64) {
        let author = AgentLabel::parse("codex").unwrap();
        crate::agent_store::update_agent_config(repo, &author, |config| {
            config.wait_events = vec![clank_core::agent_config::WaitEventSource::Timer(
                clank_core::agent_config::TimerSource {
                    interrupt: false,
                    interrupt_message: None,
                    name: name.into(),
                    repeat_every: None,
                    due_at: time::OffsetDateTime::from_unix_timestamp(due as i64)
                        .unwrap()
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap(),
                    content: Some("scheduled work".into()),
                },
            )];
        })
        .unwrap();
    }

    #[tokio::test]
    async fn recurring_timer_host_rearms_on_ack_invalidation_and_cancels_on_removal() {
        use crate::cli::github::log::EventLog;
        let repo = repo_with_roster();
        let author = AgentLabel::parse("codex").unwrap();
        with_timer(repo.path(), "recurring", 100);
        crate::agent_store::update_agent_config(repo.path(), &author, |config| {
            let clank_core::agent_config::WaitEventSource::Timer(timer) =
                &mut config.wait_events[0]
            else {
                unreachable!()
            };
            timer.repeat_every = Some("10s".into());
        })
        .unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hosted = Default::default();
        assert!(reconcile_hosting(repo.path(), &mut hosted, &tx).is_empty());
        let (_, (item, _)) = rx.try_recv().unwrap();
        let selector = item.event_view().unwrap().selector.unwrap();
        let key = selector.split_once('@').unwrap().0;
        let dir = crate::agent_store::agents_root(repo.path()).join("codex/events");
        let mut log = EventLog::open(&dir, key).unwrap();
        let row = log.read_rows().unwrap().unwrap().rows.remove(0);
        assert!(matches!(
            &row.item,
            WaitItem::TimerEvent {
                selector: None,
                recurrence: Some(_),
                ..
            }
        ));
        assert!(
            hosted[&author]
                .timers
                .as_ref()
                .unwrap()
                .next_deadline()
                .is_none()
        );
        // Record an ack while the wall clock is forward, then refresh after
        // retreat; proven coverage must still arm a future anchored deadline.
        log.ack_recurring_at(
            row.seq,
            &row.ident,
            crate::cli::timer_source::wall_now() + 60,
        )
        .unwrap();
        assert!(reconcile_hosting_changed(repo.path(), &mut hosted, &tx, true).is_empty());
        let deadline = hosted[&author]
            .timers
            .as_ref()
            .unwrap()
            .next_deadline()
            .unwrap();
        assert!(deadline > tokio::time::Instant::now());
        assert!(reconcile_hosting(repo.path(), &mut hosted, &tx).is_empty());
        assert_eq!(
            hosted[&author].timers.as_ref().unwrap().next_deadline(),
            Some(deadline)
        );
        crate::agent_store::update_agent_config(repo.path(), &author, |config| {
            config.wait_events.clear();
        })
        .unwrap();
        assert!(reconcile_hosting_changed(repo.path(), &mut hosted, &tx, true).is_empty());
        assert!(hosted.is_empty());
        assert!(rx.try_recv().is_err());
        assert_eq!(log.read_rows().unwrap().unwrap().rows.len(), 1);
    }

    #[tokio::test]
    async fn rejected_timer_edits_preserve_drivers_and_their_accepted_delivery_spec() {
        use crate::cli::event_source::{EmissionSink, SourceOrigin};
        use crate::cli::github::log::{EventLog, Transport};
        use clank_core::agent_config::WaitEventSource;
        let repo = repo_with_roster();
        let author = AgentLabel::parse("codex").unwrap();
        with_source(repo.path(), "codex", "accepted-command");
        let mut spec = hostable(repo.path(), &author).unwrap();
        let github: WaitEventSource = serde_json::from_value(serde_json::json!({
            "kind":"github", "repo":"o/r", "events":["pr_opened"], "prompt":"accepted instructions"
        }))
        .unwrap();
        spec.push((github, SourceOrigin::Config));
        let timer: WaitEventSource = serde_json::from_value(serde_json::json!({
            "kind":"timer", "name":"future", "due_at":"2040-01-01T00:00:00Z"
        }))
        .unwrap();
        spec.push((timer, SourceOrigin::Config));
        crate::agent_store::update_agent_config(repo.path(), &author, |config| {
            config.wait_events = spec.iter().map(|(source, _)| source.clone()).collect();
        })
        .unwrap();
        let events = crate::agent_store::agents_root(repo.path()).join("codex/events");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let timers = crate::cli::timer_source::TimerSet::new(
            &spec,
            &events,
            None,
            EmissionSink::for_author(tx.clone(), author.clone()),
        )
        .unwrap();
        let deadline = timers.next_deadline();
        // Lifecycle stand-ins for the two ordinary drivers: no real GitHub
        // request is appropriate in this regression test. Their guards prove
        // that even cancellation after an await would be observed.
        struct Ended(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Ended {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let ended = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let (started, ready) = tokio::sync::oneshot::channel();
            let held = ended.clone();
            tasks.push(tokio::spawn(async move {
                let _guard = Ended(held);
                started.send(()).unwrap();
                std::future::pending::<()>().await;
            }));
            ready.await.unwrap();
        }
        let ids: Vec<_> = tasks.iter().map(|task| task.id()).collect();
        let tasks: std::collections::BTreeMap<String, _> = tasks
            .into_iter()
            .enumerate()
            .map(|(i, handle)| {
                (
                    format!("source-{i}"),
                    HostedTask {
                        instance: 0,
                        handle,
                        stop: None,
                    },
                )
            })
            .collect();
        let command_key = match &spec[0].0 {
            WaitEventSource::Command(source) => source.source_key(),
            _ => unreachable!(),
        };
        let github_key = match &spec[1].0 {
            WaitEventSource::Github(source) => crate::cli::github::supervisor::source_key(source),
            _ => unreachable!(),
        };
        for (key, transport, item) in [
            (
                &command_key,
                Transport::Command,
                serde_json::json!({"kind":"command_event", "name":"accepted-command", "id":"one", "event":"work", "content":"still owed"}),
            ),
            (
                &github_key,
                Transport::Poll,
                serde_json::json!({"kind":"github_event", "repo":"o/r", "event":"pr_opened", "title":"new PR"}),
            ),
        ] {
            let item: WaitItem = serde_json::from_value(item).unwrap();
            EventLog::open(&events, key)
                .unwrap()
                .append_inbox(transport, Some("one"), None, None, &item)
                .unwrap();
        }
        let mut hosts = std::collections::BTreeMap::from([(
            author.clone(),
            Hosted {
                spec: spec.clone(),
                tasks,
                timers: Some(timers),
            },
        )]);
        for bad in [
            serde_json::json!([{"kind":"timer", "name":"broken", "due_at":"invalid"}]),
            serde_json::json!([
                {"kind":"timer", "name":"duplicate", "due_at":"2040-01-01T00:00:00Z"},
                {"kind":"timer", "name":"duplicate", "due_at":"2040-01-02T00:00:00Z"}
            ]),
        ] {
            let mut attempted = vec![
                serde_json::json!({"kind":"command", "name":"unaccepted-command", "command":["true"]}),
                serde_json::json!({"kind":"github", "repo":"different/repo", "events":["pr_opened"], "prompt":"unaccepted instructions"}),
            ];
            attempted.extend(bad.as_array().unwrap().iter().cloned());
            std::fs::write(
                crate::agent_store::agents_root(repo.path()).join("codex/config.json"),
                serde_json::to_vec(&serde_json::json!({"wait_events":attempted})).unwrap(),
            )
            .unwrap();
            assert!(
                hostable(repo.path(), &author).is_err(),
                "invalid edit is distinct from clearing sources"
            );
            assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
            assert_eq!(hosts[&author].spec, spec);
            assert_eq!(
                hosts[&author]
                    .tasks
                    .values()
                    .map(|task| task.handle.id())
                    .collect::<Vec<_>>(),
                ids
            );
            assert_eq!(
                hosts[&author].timers.as_ref().unwrap().next_deadline(),
                deadline
            );
            assert_eq!(ended.load(std::sync::atomic::Ordering::SeqCst), 0);

            let (out, mut received) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
            let client = Client {
                writer: None,
                who: Served::Agent {
                    author: author.clone(),
                },
                out,
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            };
            let mut clients = Vec::new();
            let state = crate::repo_state::RepoState::empty(repo.path().to_owned());
            assert!(
                admit_into(
                    repo.path(),
                    &mut clients,
                    client,
                    Some(&state),
                    &mut Vec::new(),
                    &hosts
                )
                .is_empty()
            );
            let note: Note = serde_json::from_str(&received.recv().await.unwrap()).unwrap();
            let Note::Items { items, .. } = note else {
                panic!("retained drivers' rows must stay deliverable")
            };
            assert_eq!(items.len(), 2);
            assert!(items.iter().any(|item| matches!(item, WaitItem::CommandEvent { name, .. } if name == "accepted-command")));
            assert!(items.iter().any(|item| matches!(item, WaitItem::GithubEvent { instructions: Some(prompt), .. } if prompt == "accepted instructions")));
        }
        // A genuinely empty, valid replacement still revokes every driver.
        crate::agent_store::update_agent_config(repo.path(), &author, |config| {
            config.wait_events.clear()
        })
        .unwrap();
        let departing = reconcile_hosting(repo.path(), &mut hosts, &tx);
        assert!(hosts.is_empty());
        assert_eq!(departing.len(), 2);
        for task in departing {
            let _ = task.await;
        }
        assert_eq!(ended.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn timer_host_is_passive_unchanged_folds_preserve_deadline_and_removal_cancels() {
        let repo = repo_with_roster();
        with_timer(
            repo.path(),
            "future",
            crate::cli::timer_source::wall_now() + 60,
        );
        let author = AgentLabel::parse("codex").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hosts = Default::default();
        assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
        let deadline = hosts[&author].timers.as_ref().unwrap().next_deadline();
        assert!(deadline.is_some());
        assert!(
            hosts[&author].tasks.is_empty(),
            "native timer starts no tasks or producers"
        );
        for _ in 0..10 {
            assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
            assert_eq!(
                hosts[&author].timers.as_ref().unwrap().next_deadline(),
                deadline
            );
        }
        crate::agent_store::update_agent_config(repo.path(), &author, |config| {
            config.wait_events.clear()
        })
        .unwrap();
        assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
        assert!(hosts.is_empty());
        assert!(rx.try_recv().is_err());
        assert!(
            !crate::agent_store::agents_root(repo.path())
                .join("codex/events")
                .exists(),
            "future hosting acquires no idle ingest lease"
        );
    }

    #[tokio::test]
    async fn timer_removed_definition_is_owed_to_two_admissions_and_ack_retires_it() {
        let repo = repo_with_roster();
        let author = AgentLabel::parse("codex").unwrap();
        with_timer(repo.path(), "past", 1);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hosts = Default::default();
        assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
        assert!(matches!(
            rx.recv().await.unwrap().1,
            (
                WaitItem::TimerEvent { .. },
                crate::cli::event_source::Durability::Logged
            )
        ));
        crate::agent_store::update_agent_config(repo.path(), &author, |config| {
            config.wait_events.clear()
        })
        .unwrap();
        assert!(reconcile_hosting(repo.path(), &mut hosts, &tx).is_empty());
        assert!(hosts.is_empty());
        let fresh = |out| Client {
            writer: None,
            who: Served::Agent {
                author: author.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        };
        let (one, mut first) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let (two, mut second) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![fresh(one), fresh(two)];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let mut cache = Default::default();
        assert!(
            serve_to_cached(
                repo.path(),
                &mut clients,
                &state,
                None,
                &mut Vec::new(),
                &mut cache,
                &hosts,
            )
            .is_empty()
        );
        let mut selectors = Vec::new();
        for receiver in [&mut first, &mut second] {
            let note: Note = serde_json::from_str(&receiver.recv().await.unwrap()).unwrap();
            let Note::Items { items, .. } = note else {
                panic!("accepted timer is owed")
            };
            assert_eq!(items.len(), 1);
            selectors.push(items[0].event_view().unwrap().selector.unwrap().to_owned());
        }
        assert_eq!(
            selectors[0], selectors[1],
            "one WAL occurrence, independent presentations"
        );
        assert!(clients.is_empty());
        crate::cli::events::run(crate::cli::EventsArgs {
            command: crate::cli::EventsCmd::Ack(crate::cli::EventsAckArgs {
                repo: Some(repo.path().to_owned()),
                author: Some("codex".into()),
                ids: vec![selectors.remove(0)],
                reply: None,
            }),
        })
        .await
        .unwrap();
        let (out, mut later) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        clients.push(fresh(out));
        assert!(
            serve_to_cached(
                repo.path(),
                &mut clients,
                &state,
                None,
                &mut Vec::new(),
                &mut cache,
                &hosts,
            )
            .is_empty()
        );
        let note: Note = serde_json::from_str(&later.recv().await.unwrap()).unwrap();
        assert!(
            matches!(note, Note::Beat),
            "cached inventory still validates durable ack state"
        );
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
            .expect("a fragmented hello is still a hello")
            .client();
        assert_eq!(
            client.who,
            Served::Agent {
                author: AgentLabel::parse("codex").unwrap(),
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
        let admitted = greet(healthy, repo.path())
            .await
            .expect("admitted")
            .client();
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

    /// A fold that FAILED must not advertise progress.
    ///
    /// The heartbeat is what the watchdog reads, and a failing loop
    /// that kept it fresh is exactly the wedge the watchdog could then
    /// never see. Tested on the ORDERING with the fold's outcome
    /// supplied, because the rebuild is fail-soft — it returns `Ok`
    /// for a repo with no git, a broken `.git` file and an empty
    /// `.git` directory — so a failing fold cannot be provoked
    /// through it.
    #[test]
    fn a_failed_fold_does_not_advertise_progress() {
        let at_zero = 0;
        let heartbeat = std::sync::atomic::AtomicU64::new(0);
        let beat = || heartbeat.load(std::sync::atomic::Ordering::Acquire);

        let failed = advance_with(
            at_zero,
            &heartbeat,
            Err(anyhow::anyhow!("the fold did not complete")),
        );
        assert!(failed.is_none(), "a failed fold yields nothing to serve");
        assert_eq!(beat(), 0, "and the watchdog's heartbeat does not move");

        for _ in 0..3 {
            let _ = advance_with(at_zero, &heartbeat, Err(anyhow::anyhow!("still failing")));
        }
        assert_eq!(beat(), 0, "repeated failure must not creep it either");

        let ok = advance_with(
            at_zero,
            &heartbeat,
            Ok(crate::repo_state::RepoState::empty(
                std::path::PathBuf::from("/w/proj"),
            )),
        )
        .expect("a completed fold serves");
        assert_eq!(ok.0, 1);
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
                writer: None,
                who: Served::Agent { author: label },
                out,
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
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
            None,
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
            None,
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

        // More ticks than the queue holds, so a reader that cannot keep
        // up is retired instead of buffered forever.
        let began = std::time::Instant::now();
        for _ in 0..(CLIENT_QUEUE + 2) {
            serve(repo.path(), &mut clients, &state);
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

        serve(repo.path(), &mut clients, &state);
        assert_eq!(clients.len(), 1, "a registered author is served");

        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        serve(repo.path(), &mut clients, &state);
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
        async fn admitted(repo: &std::path::Path, who: Who, pid: i32) -> anyhow::Result<Greeted> {
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
            .expect("an observer is welcome")
            .client();
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

    /// Give an agent a config source, as `wait_events` would.
    fn with_source(repo: &std::path::Path, author: &str, name: &str) {
        let dir = crate::agent_store::agents_root(repo).join(author);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_vec(&serde_json::json!({
                "wait_events": [
                    {"kind": "command", "name": name, "command": ["true"]}
                ]
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// The account says, for every hosted source, the configuration the
    /// monitor accepted and the lifecycle its producer last reported, and
    /// it is published outside the repo: a producer starting or ending is
    /// no repo write, so it wakes no wait and no gate.
    #[tokio::test]
    async fn the_sources_account_is_what_runs_and_lives_outside_the_repo() {
        use crate::cli::event_source::{Lifecycle, SourceOrigin};
        let repo = repo_with_roster();
        let codex = AgentLabel::parse("codex").unwrap();
        let config = |name: &str| {
            clank_core::agent_config::WaitEventSource::Command(
                clank_core::agent_config::CommandSource {
                    interrupt: false,
                    interrupt_message: None,
                    name: name.into(),
                    command: vec!["true".into()],
                    reply_command: None,
                    receipt_command: None,
                    glyph: None,
                    stop_grace: None,
                },
            )
        };
        let key =
            |name: &str| crate::cli::source_status::ConfiguredSource::of(&codex, &config(name)).key;
        let one = key("one");
        let task = |instance| HostedTask {
            instance,
            handle: tokio::spawn(std::future::pending()),
            stop: None,
        };
        let hosted = std::collections::BTreeMap::from([(
            codex.clone(),
            Hosted {
                spec: vec![
                    (config("one"), SourceOrigin::Config),
                    (config("two"), SourceOrigin::Config),
                ],
                tasks: [(one.clone(), task(5)), (key("two"), task(6))].into(),
                timers: None,
            },
        )]);
        // `two`'s report is from the task its current one replaced.
        let lifecycles = std::collections::BTreeMap::from([
            (
                (codex.clone(), one.clone()),
                (5, Lifecycle::Running { pid: 9, since: 0 }),
            ),
            (
                (codex.clone(), key("two")),
                (4, Lifecycle::Running { pid: 8, since: 0 }),
            ),
        ]);
        let birth = crate::proc_identity::token_for(std::process::id() as i32).unwrap();
        let account = sources_account(&hosted, &lifecycles, birth);
        assert_eq!(account.monitor, std::process::id() as i32);
        assert_eq!(account.sources.len(), 2);
        assert_eq!(account.sources[0].key, one);
        assert_eq!(
            account.sources[0].accepted,
            crate::cli::source_status::fingerprint(&config("one"))
        );
        assert_eq!(
            account.sources[0].lifecycle,
            Some(Lifecycle::Running { pid: 9, since: 0 })
        );
        assert_eq!(
            account.sources[1].lifecycle, None,
            "a replaced task's report"
        );
        for host in hosted.into_values() {
            for handle in host.stop() {
                let _ = handle.await;
            }
        }
        let published = crate::cli::source_status::path(repo.path());
        assert!(
            !published.starts_with(repo.path()),
            "{}",
            published.display()
        );
    }

    /// Two sources resolving to one storage key are refused, command or
    /// github, and an edit that introduces them leaves the last accepted
    /// host running as it was rather than losing hold of a task.
    #[tokio::test]
    async fn sources_sharing_a_storage_key_are_refused_and_the_host_kept() {
        let repo = repo_with_roster();
        let codex = AgentLabel::parse("codex").unwrap();
        let dir = crate::agent_store::agents_root(repo.path()).join("codex");
        std::fs::create_dir_all(&dir).unwrap();
        let write = |sources: serde_json::Value| {
            std::fs::write(
                dir.join("config.json"),
                serde_json::to_vec(&serde_json::json!({ "wait_events": sources })).unwrap(),
            )
            .unwrap()
        };
        let command =
            serde_json::json!({"kind": "command", "name": "watch", "command": ["sleep", "300"]});
        let github = serde_json::json!({"kind": "github", "repo": "o/r", "events": ["pr_comment"]});
        for twice in [command.clone(), github] {
            write(serde_json::json!([twice.clone(), twice]));
            let refused = hostable(repo.path(), &codex).unwrap_err().to_string();
            assert!(refused.contains("share the storage key"), "{refused}");
        }
        write(serde_json::json!([command.clone()]));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hosted = std::collections::BTreeMap::new();
        let mut departing: Vec<_> = reconcile_hosting(repo.path(), &mut hosted, &tx);
        let before: Vec<_> = hosted[&codex]
            .tasks
            .values()
            .map(|t| t.handle.id())
            .collect();
        write(serde_json::json!([command.clone(), command]));
        departing.extend(reconcile_hosting(repo.path(), &mut hosted, &tx));
        let after: Vec<_> = hosted[&codex]
            .tasks
            .values()
            .map(|t| t.handle.id())
            .collect();
        assert_eq!(before, after, "the accepted host runs on, holding its task");
        for host in hosted.into_values() {
            departing.extend(host.stop());
        }
        for handle in departing {
            let _ = handle.await;
        }
    }

    /// A source's task replaced under the same key starts with nothing
    /// said: the old task's state is forgotten and its late reports are
    /// refused, and what the new one reports, even that it stopped before
    /// running anything, is what is published.
    #[tokio::test]
    async fn a_replaced_task_is_never_spoken_for_by_the_old_one() {
        use crate::cli::event_source::{Lifecycle, SourceOrigin};
        let codex = AgentLabel::parse("codex").unwrap();
        let config = clank_core::agent_config::WaitEventSource::Command(
            clank_core::agent_config::CommandSource {
                interrupt: false,
                interrupt_message: None,
                name: "watch".into(),
                command: vec!["true".into()],
                reply_command: None,
                receipt_command: None,
                glyph: None,
                stop_grace: None,
            },
        );
        let key = crate::cli::source_status::ConfiguredSource::of(&codex, &config).key;
        let host = |instance| Hosted {
            spec: vec![(config.clone(), SourceOrigin::Config)],
            tasks: [(
                key.clone(),
                HostedTask {
                    instance,
                    handle: tokio::spawn(std::future::pending()),
                    stop: None,
                },
            )]
            .into(),
            timers: None,
        };
        let mut hosted = std::collections::BTreeMap::from([(codex.clone(), host(1))]);
        let mut lifecycles = Lifecycles::default();
        let running = Lifecycle::Running { pid: 9, since: 0 };
        assert!(record_lifecycle(
            &hosted,
            &mut lifecycles,
            (codex.clone(), key.clone(), 1, running.clone())
        ));
        let old = hosted.insert(codex.clone(), host(2)).unwrap();
        forget_replaced(&hosted, &mut lifecycles);
        assert!(lifecycles.is_empty(), "the new task has said nothing");
        assert!(!record_lifecycle(
            &hosted,
            &mut lifecycles,
            (codex.clone(), key.clone(), 1, running)
        ));
        let stopped = Lifecycle::Stopped {
            why: "cannot take its ingest lease".into(),
        };
        assert!(record_lifecycle(
            &hosted,
            &mut lifecycles,
            (codex.clone(), key.clone(), 2, stopped.clone())
        ));
        let birth = crate::proc_identity::token_for(std::process::id() as i32).unwrap();
        assert_eq!(
            sources_account(&hosted, &lifecycles, birth).sources[0].lifecycle,
            Some(stopped)
        );
        for host in hosted.into_values().chain([old]) {
            for handle in host.stop() {
                let _ = handle.await;
            }
        }
    }

    /// A restart stops one source's whole process group, a grandchild
    /// included, and starts it again, leaving the author's other source
    /// running as it was; a request for a configuration the monitor no
    /// longer runs, or a source it does not have, restarts nothing.
    #[tokio::test]
    async fn a_restart_is_one_source_whole_and_only_it() {
        let repo = repo_with_roster();
        let marks = repo.path().join("marks");
        std::fs::create_dir_all(&marks).unwrap();
        let mark = |name: &str| marks.join(name).display().to_string();
        let dir = crate::agent_store::agents_root(repo.path()).join("codex");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_vec(&serde_json::json!({"wait_events": [
                {"kind": "command", "name": "restartable", "command": [
                    "sh", "-c", "sleep 300 & echo $! > \"$0.gc\"; echo $$ >> \"$0.runs\"; wait", mark("a")]},
                {"kind": "command", "name": "sibling", "command": [
                    "sh", "-c", "echo $$ > \"$0.pid\"; exec sleep 300", mark("b")]},
            ]}))
            .unwrap(),
        )
        .unwrap();
        let codex = AgentLabel::parse("codex").unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (life, _heard) = tokio::sync::mpsc::unbounded_channel();
        let mut hosted = std::collections::BTreeMap::new();
        let mut departing = futures_util::stream::FuturesUnordered::new();
        departing.extend(reconcile_hosting_reporting(
            repo.path(),
            &mut hosted,
            &tx,
            false,
            Some(&life),
        ));
        let read = |name: &str| std::fs::read_to_string(mark(name)).unwrap_or_default();
        let pid = |name: &str| {
            read(name)
                .lines()
                .last()
                .and_then(|l| l.trim().parse::<i32>().ok())
        };
        async fn until(what: impl Fn() -> bool) -> bool {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            while !what() && std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            what()
        }
        assert!(
            until(|| pid("a.gc").is_some() && pid("b.pid").is_some()).await,
            "both started"
        );
        let grandchild = pid("a.gc").unwrap();
        let sibling = pid("b.pid").unwrap();
        let alive = |pid: i32| crate::signal::probe_process(pid) != crate::signal::Probe::Gone;
        let config = hosted[&codex]
            .spec
            .iter()
            .map(|(c, _)| c.clone())
            .find(|c| matches!(c, clank_core::agent_config::WaitEventSource::Command(s) if s.name == "restartable"))
            .unwrap();
        let clank_core::agent_config::WaitEventSource::Command(command) = &config else {
            unreachable!()
        };
        let key = command.source_key();
        let ask = |key: &str, accepted: &str| Control::Restart {
            author: "codex".into(),
            key: key.into(),
            accepted: accepted.into(),
        };
        for refused in [
            ask(&key, "an earlier config"),
            ask("command-nothing-0", "x"),
        ] {
            assert!(
                restart_source(
                    repo.path(),
                    &mut hosted,
                    &tx,
                    &life,
                    &mut departing,
                    &refused
                )
                .await
                .is_err()
            );
        }
        assert!(alive(grandchild), "a refused restart touches nothing");
        let accepted = crate::cli::source_status::fingerprint(&config);
        restart_source(
            repo.path(),
            &mut hosted,
            &tx,
            &life,
            &mut departing,
            &ask(&key, &accepted),
        )
        .await
        .unwrap();
        assert!(
            until(|| !alive(grandchild)).await,
            "the grandchild went with its group"
        );
        assert!(
            until(|| read("a.runs").lines().count() == 2).await,
            "and it started again"
        );
        assert!(
            hosted[&codex].tasks.contains_key(&key) && hosted[&codex].tasks.len() == 2,
            "and the monitor holds its new task, to stop or restart again"
        );
        assert!(
            alive(sibling) && pid("b.pid") == Some(sibling),
            "the sibling ran on"
        );
        for host in std::mem::take(&mut hosted).into_values() {
            departing.extend(host.stop());
        }
        while let Some(_) = futures_util::StreamExt::next(&mut departing).await {}
        assert!(until(|| !alive(sibling)).await);
    }

    #[tokio::test]
    async fn hosted_producers_deliver_with_their_author_and_release_their_leases() {
        use crate::cli::event_source::Durability;
        use crate::cli::github::log::IngestLease;

        let repo = repo_with_roster();
        for who in ["claude", "codex"] {
            with_source(repo.path(), who, "builds");
            let config = crate::agent_store::agents_root(repo.path())
                .join(who)
                .join("config.json");
            std::fs::write(config, serde_json::to_vec(&serde_json::json!({
                "wait_events": [{"kind": "command", "name": "builds", "command": [
                    "sh", "-c", format!(r#"printf '{{"id":"{who}","content":"{who}"}}\n'; sleep 300"#)
                ]}]
            })).unwrap()).unwrap();
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hosted = std::collections::BTreeMap::new();
        assert!(reconcile_hosting(repo.path(), &mut hosted, &tx).is_empty());
        assert_eq!(hosted.len(), 2);
        assert!(hosted.values().all(|host| host.tasks.len() == 1));

        let mut heard = tokio::time::timeout(Duration::from_secs(10), async {
            let mut heard = Vec::new();
            for _ in 0..2 {
                let (author, (item, durability)) = rx.recv().await.unwrap();
                assert_eq!(durability, Durability::Logged);
                let WaitItem::CommandEvent {
                    content, failure, ..
                } = item
                else {
                    panic!("expected the hosted command's event");
                };
                assert!(!failure);
                assert_eq!(content, author.as_str());
                heard.push(author);
            }
            heard
        })
        .await
        .expect("both hosted producers must deliver into the monitor queue");
        heard.sort();
        assert_eq!(
            heard.iter().map(AgentLabel::as_str).collect::<Vec<_>>(),
            vec!["claude", "codex"]
        );
        for author in &heard {
            let spec = &hosted[author].spec;
            let clank_core::agent_config::WaitEventSource::Command(command) = &spec[0].0 else {
                unreachable!()
            };
            let events = crate::agent_store::agents_root(repo.path())
                .join(author.as_str())
                .join("events");
            assert!(
                IngestLease::try_acquire(&events, &command.source_key())
                    .unwrap()
                    .is_none()
            );
        }
        let mut stopped = Vec::new();
        for (_, host) in hosted {
            stopped.extend(host.stop());
        }
        for task in stopped {
            let _ = task.await;
        }
        for author in &heard {
            let sources = hostable(repo.path(), author).unwrap();
            let clank_core::agent_config::WaitEventSource::Command(command) = &sources[0].0 else {
                unreachable!()
            };
            let events = crate::agent_store::agents_root(repo.path())
                .join(author.as_str())
                .join("events");
            assert!(
                IngestLease::try_acquire(&events, &command.source_key())
                    .unwrap()
                    .is_some()
            );
        }
    }

    /// Everything the monitor hosts is an author's CONFIG source.
    ///
    /// The assumption that makes passing no invocation scope safe: a
    /// pooled host has none to give, and `EventSources::spawn` refuses
    /// an invocation's source without one. This is the other half —
    /// that such a source can never arrive here, because `hostable`
    /// resolves with no CLI items at all.
    #[test]
    fn nothing_the_monitor_hosts_belongs_to_an_invocation() {
        use crate::cli::event_source::SourceOrigin;
        let repo = repo_with_roster();
        with_source(repo.path(), "codex", "deploys");
        let codex = AgentLabel::parse("codex").unwrap();

        let hosted = hostable(repo.path(), &codex).unwrap();
        assert!(!hosted.is_empty(), "the fixture declares one");
        assert!(
            hosted
                .iter()
                .all(|(_, origin)| matches!(origin, SourceOrigin::Config)),
            "an invocation's source cannot reach a pooled host: {:?}",
            hosted.iter().map(|(_, o)| *o).collect::<Vec<_>>()
        );
    }

    /// Hosting follows the ROSTER, not whoever happens to be connected.
    ///
    /// Tying a producer to an attached client is tying it to a delivery
    /// client again, and the churn that causes — the user's command
    /// respawned on every wake, because the lease holder was a parked
    /// wait that returned — is the whole reason for this plan.
    #[tokio::test]
    async fn hosting_follows_the_roster_and_reconciles_on_a_config_change() {
        let repo = repo_with_roster();
        with_source(repo.path(), "codex", "deploys");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut hosted = std::collections::BTreeMap::new();

        // No client has connected, and the producer runs anyway.
        reconcile_hosting(repo.path(), &mut hosted, &tx);
        assert_eq!(
            hosted.keys().map(|a| a.to_string()).collect::<Vec<_>>(),
            vec!["codex".to_string()],
            "an author with no configured source hosts nothing"
        );
        let codex = AgentLabel::parse("codex").unwrap();
        let first: Vec<_> = hosted[&codex]
            .tasks
            .values()
            .map(|t| t.handle.id())
            .collect();
        assert_eq!(first.len(), 1, "one source driver, no author forwarder");

        // An unrelated fold leaves a healthy producer alone.
        reconcile_hosting(repo.path(), &mut hosted, &tx);
        let again: Vec<_> = hosted[&codex]
            .tasks
            .values()
            .map(|t| t.handle.id())
            .collect();
        assert_eq!(first, again, "an unchanged spec must not restart anything");

        // An edited `wait_events` takes effect without a restart: the
        // renamed source is a different one, so the old stops and the new
        // starts.
        with_source(repo.path(), "codex", "builds");
        let departing = reconcile_hosting(repo.path(), &mut hosted, &tx);
        assert_eq!(departing.len(), 1, "the source the edit removed is stopped");
        let after: Vec<_> = hosted[&codex]
            .tasks
            .values()
            .map(|t| t.handle.id())
            .collect();
        assert_eq!(after.len(), 1);
        assert_ne!(first, after, "the source the edit added is started");

        // And an author off the roster keeps nothing running.
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        reconcile_hosting(repo.path(), &mut hosted, &tx);
        assert!(
            hosted.is_empty(),
            "a revoked agent's producer can serve nobody"
        );
    }

    /// Stand-in source tasks for driving the reconcile itself: no
    /// process, only a task per source that runs until it is asked to
    /// stop, and notes by its key that it was.
    fn stand_in_spawner(
        asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> impl Fn(
        &clank_core::agent_config::WaitEventSource,
        crate::cli::event_source::SourceOrigin,
        &std::path::Path,
        crate::cli::event_source::EmissionSink,
    ) -> Option<crate::cli::wait::SourceTask> {
        move |source, origin, _, _| {
            let key = crate::cli::wait::task_key(source, origin, "")?;
            let (stop, mut requested) = tokio::sync::watch::channel(false);
            let asked = asked.clone();
            let noted = key.clone();
            let handle = tokio::spawn(async move {
                if requested.wait_for(|stop| *stop).await.is_ok() {
                    asked.lock().unwrap().push(noted);
                }
            });
            Some(crate::cli::wait::SourceTask {
                key,
                handle,
                stop: Some(stop),
            })
        }
    }

    fn write_sources(repo: &std::path::Path, author: &str, sources: serde_json::Value) {
        let dir = crate::agent_store::agents_root(repo).join(author);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            serde_json::to_vec(&serde_json::json!({ "wait_events": sources })).unwrap(),
        )
        .unwrap();
    }

    fn instances(
        hosted: &std::collections::BTreeMap<AgentLabel, Hosted>,
        author: &AgentLabel,
    ) -> std::collections::BTreeMap<String, u64> {
        hosted[author]
            .tasks
            .iter()
            .map(|(key, task)| (key.clone(), task.instance))
            .collect()
    }

    /// What is hosted is a source, not an author's set of sources: a
    /// reconcile leaves every source whose definition is unchanged
    /// running as the same instance — through a timer added or removed,
    /// a reorder, an edit to a sibling, or to another author — and stops
    /// and restarts only the source an edit changed, under the same
    /// storage key, so its log, checkpoint and lease carry over. A timer
    /// edit leaves an unchanged timer's deadline where it was, and a
    /// replaced task's late report is not taken for its successor's.
    #[tokio::test]
    async fn a_source_restarts_only_when_it_changes() {
        let repo = repo_with_roster();
        let (codex, claude) = (
            AgentLabel::parse("codex").unwrap(),
            AgentLabel::parse("claude").unwrap(),
        );
        let due = |secs: u64| {
            time::OffsetDateTime::from_unix_timestamp(
                (crate::cli::timer_source::wall_now() + secs) as i64,
            )
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
        };
        let (soon, later) = (due(600), due(1200));
        let one = serde_json::json!({"kind": "command", "name": "one", "command": ["true"]});
        let two = serde_json::json!({"kind": "command", "name": "two", "command": ["true"]});
        let timer = serde_json::json!({"kind": "timer", "name": "soon", "due_at": soon});
        let added = serde_json::json!({"kind": "timer", "name": "later", "due_at": later});
        write_sources(repo.path(), "codex", serde_json::json!([one, two, timer]));
        write_sources(
            repo.path(),
            "claude",
            serde_json::json!([{"kind": "command", "name": "theirs", "command": ["true"]}]),
        );
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawn = stand_in_spawner(asked.clone());
        let (into, _rx) = tokio::sync::mpsc::unbounded_channel();
        let starts = Starts {
            into: &into,
            lifecycle: None,
            spawn: &spawn,
        };
        let mut hosted = std::collections::BTreeMap::new();
        assert!(reconcile_hosting_with(repo.path(), &mut hosted, false, &starts).is_empty());
        let first = instances(&hosted, &codex);
        let theirs = instances(&hosted, &claude);
        assert_eq!(first.len(), 2);
        let deadline = hosted[&codex].timers.as_ref().unwrap().next_deadline();
        assert!(deadline.is_some());
        let key = |name: &str| {
            first
                .keys()
                .find(|key| key.starts_with(&format!("command-{name}-")))
                .unwrap()
                .clone()
        };
        let reconcile = |hosted: &mut std::collections::BTreeMap<AgentLabel, Hosted>| {
            reconcile_hosting_with(repo.path(), hosted, false, &starts)
        };

        for unrelated in [
            serde_json::json!([one, two, timer, added]),
            serde_json::json!([added, timer, two, one]),
            serde_json::json!([one, two, timer]),
        ] {
            write_sources(repo.path(), "codex", unrelated.clone());
            assert!(reconcile(&mut hosted).is_empty(), "{unrelated}");
            assert_eq!(instances(&hosted, &codex), first, "{unrelated}");
            assert_eq!(
                hosted[&codex].timers.as_ref().unwrap().next_deadline(),
                deadline,
                "an unchanged timer keeps its deadline: {unrelated}"
            );
        }

        let mut edited = two.clone();
        edited["command"] = serde_json::json!(["false"]);
        write_sources(
            repo.path(),
            "codex",
            serde_json::json!([one, edited, timer]),
        );
        let departing = reconcile(&mut hosted);
        assert_eq!(departing.len(), 1);
        for task in departing {
            task.await.unwrap();
        }
        let now = instances(&hosted, &codex);
        assert_eq!(now[&key("one")], first[&key("one")], "the sibling ran on");
        assert_ne!(
            now[&key("two")],
            first[&key("two")],
            "the edited source restarted, under the key it had"
        );
        assert_eq!(*asked.lock().unwrap(), vec![key("two")]);

        let mut graced = one.clone();
        graced["stop_grace"] = serde_json::json!("30s");
        write_sources(repo.path(), "codex", serde_json::json!([graced, edited]));
        let departing = reconcile(&mut hosted);
        assert_eq!(departing.len(), 1);
        for task in departing {
            task.await.unwrap();
        }
        let after = instances(&hosted, &codex);
        assert_ne!(after[&key("one")], now[&key("one")]);
        assert_eq!(after[&key("two")], now[&key("two")], "a timer removed");
        assert!(hosted[&codex].timers.is_none());
        assert_eq!(
            instances(&hosted, &claude),
            theirs,
            "another author's edits"
        );

        let mut lifecycles = Lifecycles::default();
        let report = |instance| {
            (
                codex.clone(),
                key("one"),
                instance,
                crate::cli::event_source::Lifecycle::Running { pid: 9, since: 0 },
            )
        };
        assert!(!record_lifecycle(
            &hosted,
            &mut lifecycles,
            report(now[&key("one")])
        ));
        assert!(record_lifecycle(
            &hosted,
            &mut lifecycles,
            report(after[&key("one")])
        ));

        for host in std::mem::take(&mut hosted).into_values() {
            for task in host.stop() {
                task.await.unwrap();
            }
        }
    }

    /// An author off the roster has every source it hosted asked to
    /// stop, and handed back to be observed ending, while another
    /// author's run on.
    #[tokio::test]
    async fn a_removed_authors_sources_are_stopped() {
        let repo = repo_with_roster();
        write_sources(
            repo.path(),
            "codex",
            serde_json::json!([
                {"kind": "command", "name": "one", "command": ["true"]},
                {"kind": "command", "name": "two", "command": ["true"]},
            ]),
        );
        write_sources(
            repo.path(),
            "claude",
            serde_json::json!([{"kind": "command", "name": "theirs", "command": ["true"]}]),
        );
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawn = stand_in_spawner(asked.clone());
        let (into, _rx) = tokio::sync::mpsc::unbounded_channel();
        let starts = Starts {
            into: &into,
            lifecycle: None,
            spawn: &spawn,
        };
        let mut hosted = std::collections::BTreeMap::new();
        assert!(reconcile_hosting_with(repo.path(), &mut hosted, false, &starts).is_empty());
        let codex = AgentLabel::parse("codex").unwrap();
        let claude = AgentLabel::parse("claude").unwrap();
        let mut keys: Vec<String> = instances(&hosted, &codex).into_keys().collect();
        let theirs = instances(&hosted, &claude);

        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#,
        )
        .unwrap();
        let departing = reconcile_hosting_with(repo.path(), &mut hosted, false, &starts);
        assert_eq!(departing.len(), 2);
        for task in departing {
            task.await.unwrap();
        }
        assert!(!hosted.contains_key(&codex));
        let mut stopped = asked.lock().unwrap().clone();
        stopped.sort();
        keys.sort();
        assert_eq!(stopped, keys);
        assert_eq!(instances(&hosted, &claude), theirs);
        for host in std::mem::take(&mut hosted).into_values() {
            for task in host.stop() {
                task.await.unwrap();
            }
        }
    }

    /// A row is presented ONCE, however many times its arrival is
    /// signalled — and not at all once somebody has acked it.
    ///
    /// The failure this excludes is two delivery paths with different
    /// rules: a backlog replayed from the WAL at admission and a live
    /// item broadcast straight off the producer channel. A row logged
    /// before a client arrived would then be sent twice, and a row
    /// another client acked in between would be sent anyway. Both are
    /// gone because the channel carries a SIGNAL and the log decides
    /// (codex on ca2ea69).
    #[tokio::test]
    async fn a_logged_row_is_presented_once_and_never_after_an_ack() {
        use crate::cli::github::log::{EventLog, Transport};

        let repo = repo_with_roster();
        with_source(repo.path(), "codex", "deploys");
        let codex = AgentLabel::parse("codex").unwrap();
        let events = crate::agent_store::agents_root(repo.path())
            .join("codex")
            .join("events");
        let key = match &hostable(repo.path(), &codex).unwrap()[0].0 {
            clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
            other => panic!("the fixture declares a command source, got {other:?}"),
        };

        // A row logged BEFORE anyone connects, as a monitor's producer
        // does: it has been running since the monitor started.
        let mut log = EventLog::open(&events, &key).unwrap();
        let seq = log
            .append_inbox(
                Transport::Command,
                None,
                None,
                None,
                &WaitItem::CommandEvent {
                    presentation: None,
                    source_cwd: None,
                    name: "deploys".into(),
                    id: "a".into(),
                    event: "deploys".into(),
                    content: "shipped".into(),
                    at: None,
                    failure: false,
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();

        let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: codex.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let present = |clients: &mut Vec<Client>| {
            serve_to(repo.path(), clients, &state, Some(&codex), &mut Vec::new());
        };

        // The first presentation owes it, and WAKES the client — which
        // ends that connection, because a wait returns on its first
        // non-empty frame.
        present(&mut clients);
        let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("the backlog is owed, got {note:?}")
        };
        assert_eq!(items.len(), 1, "{items:?}");
        assert!(
            clients.is_empty(),
            "a woken agent connection is spent: it will not read another frame"
        );

        // Its successor is owed the same row, because being shown to a
        // client that has since gone is not being handled.
        let (out, mut later) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let fresh = |out| Client {
            writer: None,
            who: Served::Agent {
                author: codex.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        };
        clients.push(fresh(out));
        present(&mut clients);
        let note: Note = serde_json::from_str(later.recv().await.unwrap().trim_end()).unwrap();
        assert!(
            matches!(note, Note::Items { ref items, .. } if items.len() == 1),
            "an unacked row is owed to the next client too: {note:?}"
        );

        // Until it is acked. Then nobody is owed it, including a client
        // that never saw it.
        let (out, mut after) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        clients.push(fresh(out));
        crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .append_ack(seq)
            .unwrap();
        present(&mut clients);
        let note: Note = serde_json::from_str(after.recv().await.unwrap().trim_end()).unwrap();
        assert!(
            matches!(note, Note::Beat { .. }),
            "an acked row is owed to nobody: {note:?}"
        );
    }

    /// A source event reaching a BLOCKED agent arrives with its block
    /// context, exactly as a local wait delivers it.
    ///
    /// The local path merges externals INSIDE `present_with_blocks`
    /// deliberately: deciding before the merge left the channel route
    /// outside the decision, so a blocked park returned a source item
    /// with no `Blocked` context to explain the silence around it
    /// (codex on 344280f). A monitor that appended its source rows
    /// after the decision would reintroduce exactly that, which is why
    /// `owed_for` takes the externals rather than the caller merging
    /// them (codex on ca2ea69).
    #[tokio::test]
    async fn a_source_event_wakes_a_blocked_agent_with_its_block() {
        use crate::cli::github::log::{EventLog, Transport};

        let repo = repo_with_roster();
        with_source(repo.path(), "codex", "deploys");
        let codex = AgentLabel::parse("codex").unwrap();
        let agent_dir = crate::agent_store::agents_root(repo.path()).join("codex");
        std::fs::create_dir_all(agent_dir.join("blocks")).unwrap();
        std::fs::write(
            agent_dir.join("blocks/why-is-this-here.md"),
            "# why is this here\n\nasking the human\n",
        )
        .unwrap();

        let events = agent_dir.join("events");
        let key = match &hostable(repo.path(), &codex).unwrap()[0].0 {
            clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
            other => panic!("the fixture declares a command source, got {other:?}"),
        };
        EventLog::open(&events, &key)
            .unwrap()
            .append_inbox(
                Transport::Command,
                None,
                None,
                None,
                &WaitItem::CommandEvent {
                    presentation: None,
                    source_cwd: None,
                    name: "deploys".into(),
                    id: "a".into(),
                    event: "deploys".into(),
                    content: "ci failed".into(),
                    at: None,
                    failure: false,
                    requires_reply: false,
                    reply_route: None,
                    selector: None,
                },
            )
            .unwrap();

        let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: codex.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        serve_to(
            repo.path(),
            &mut clients,
            &state,
            Some(&codex),
            &mut Vec::new(),
        );

        let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("a source event wakes a blocked agent, got {note:?}")
        };
        assert!(
            items.iter().any(|i| matches!(i, WaitItem::Blocked { .. })),
            "the block LEADS as context, or the agent has no way to know \
             why its plan work went quiet: {items:?}"
        );
        assert!(
            items
                .iter()
                .any(|i| matches!(i, WaitItem::CommandEvent { .. })),
            "and the event that woke it is still there: {items:?}"
        );
    }

    /// An emission no log holds still reaches the client, once.
    ///
    /// Github's WAL warns once on an append failure and keeps
    /// delivering, so the agent still learns about the event. A monitor
    /// that presented only from the log would throw that away — the
    /// channel is the only record of it (codex on 0555c8f). Driven by
    /// the real discriminator rather than a flag: the item is simply
    /// not in any row.
    #[tokio::test]
    async fn an_emission_no_log_holds_is_still_presented() {
        let repo = repo_with_roster();
        with_source(repo.path(), "codex", "deploys");
        let codex = AgentLabel::parse("codex").unwrap();
        let unlogged = WaitItem::GithubEvent {
            presentation: None,
            repo: "o/n".into(),
            event: "pr_opened".into(),
            detail: None,
            number: Some(7),
            title: None,
            actor: None,
            url: None,
            instructions: None,
            content: None,
        };

        let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: codex.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        serve_to(
            repo.path(),
            &mut clients,
            &state,
            Some(&codex),
            &mut vec![(codex.clone(), unlogged.clone())],
        );

        let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("an unlogged emission is still owed, got {note:?}")
        };
        assert_eq!(items, vec![unlogged.clone()]);

        // And provenance, not inspection, is what decides. The
        // producer saw the append succeed or fail; nothing a reader
        // can measure tells it apart, which is why two attempts to
        // infer it were wrong.
        use crate::cli::event_source::Durability;
        assert!(
            transient_of((unlogged.clone(), Durability::Logged)).is_empty(),
            "a logged emission is carried by the log, not beside it"
        );
        assert_eq!(
            transient_of((unlogged.clone(), Durability::Transient)),
            vec![unlogged],
            "and only a transient is carried here"
        );
    }

    /// A startup transient rides the SAME frame as the work that fold
    /// already owed.
    ///
    /// A client returns on its first non-empty `Items`, so sending the
    /// projection first and the held transients second reaches a
    /// client that has already gone — and a transient has no row to
    /// replay, because the append is what failed. One presentation per
    /// completed fold, carrying everything (codex on 328677d).
    #[tokio::test]
    async fn a_held_transient_rides_the_first_frame_with_the_work_it_found() {
        let repo = repo_with_roster();
        // Something the fold itself owes master, so the first frame is
        // non-empty on its own — which is the case that used to lose
        // the transient.
        std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
        std::fs::write(
            repo.path().join(".clank/queue/500-next.md"),
            "# next\n\nsomething to promote\n",
        )
        .unwrap();

        let claude = AgentLabel::parse("claude").unwrap();
        let held = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "a".into(),
            event: "deploys".into(),
            content: "ci failed".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        let mut pending = vec![(claude.clone(), held.clone())];

        let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: claude.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        serve_to(repo.path(), &mut clients, &state, None, &mut pending);

        let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("the fold owes work and a transient is held, got {note:?}")
        };
        assert!(
            items
                .iter()
                .any(|i| matches!(i, WaitItem::PromoteFromQueue { .. })),
            "the work the fold found: {items:?}"
        );
        assert!(
            items.contains(&held),
            "AND the transient, in the same frame — a second frame would \
             reach a client that already returned: {items:?}"
        );
        assert!(pending.is_empty(), "delivered, so no longer held");
        assert!(heard.try_recv().is_err(), "and nothing follows it");
    }

    /// A transient for an author with nobody connected keeps waiting.
    #[tokio::test]
    async fn a_transient_with_no_client_is_held_rather_than_dropped() {
        let repo = repo_with_roster();
        let codex = AgentLabel::parse("codex").unwrap();
        let held = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "a".into(),
            event: "deploys".into(),
            content: "ci failed".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        let mut pending = vec![(codex.clone(), held.clone())];

        // Only ANOTHER author is connected.
        let (out, mut other) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: AgentLabel::parse("claude").unwrap(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        serve_to(repo.path(), &mut clients, &state, None, &mut pending);

        assert_eq!(
            pending,
            vec![(codex, held.clone())],
            "nothing else will ever hold it, so it waits for its author"
        );
        // And it did not reach the author who happened to be there:
        // a transient is one author's event, like any other.
        let note: Note = serde_json::from_str(other.recv().await.unwrap().trim_end()).unwrap();
        match note {
            Note::Done { what } => panic!("unexpected control answer: {what}"),
            Note::Refused { why } => panic!("unexpected refusal: {why}"),
            Note::Beat { .. } => {}
            Note::Recover { .. } => panic!("a wait is never sent a recovery"),
            Note::Superseded => panic!("nothing superseded this wait"),
            Note::Verdict(frame) => panic!("a plain wait is never sent a verdict: {frame:?}"),
            Note::Items { items, .. } => assert!(
                !items.contains(&held),
                "another author's transient must not be delivered here: {items:?}"
            ),
        }
    }

    /// An admission carries what was held for that author.
    ///
    /// A transient retained while its author had no client is owed to
    /// the first client that author gets. Admitting on ordinary work
    /// without it would let the newcomer return on that work, leaving
    /// the held event waiting for a tick that may never find an
    /// eligible client (codex on 1552f66).
    #[tokio::test]
    async fn an_admission_carries_what_was_held_for_its_author() {
        let repo = repo_with_roster();
        // Ordinary work the fold owes master on its own.
        std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
        std::fs::write(
            repo.path().join(".clank/queue/500-next.md"),
            "# next\n\nsomething to promote\n",
        )
        .unwrap();

        let claude = AgentLabel::parse("claude").unwrap();
        let held = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "a".into(),
            event: "deploys".into(),
            content: "ci failed".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        let mut pending = vec![(claude.clone(), held.clone())];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());

        let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = Vec::new();
        admit_into(
            repo.path(),
            &mut clients,
            Client {
                writer: None,
                who: Served::Agent {
                    author: claude.clone(),
                },
                out,
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            },
            Some(&state),
            &mut pending,
            &Default::default(),
        );

        let note: Note = serde_json::from_str(heard.recv().await.unwrap().trim_end()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("admission owes both, got {note:?}")
        };
        assert!(
            items
                .iter()
                .any(|i| matches!(i, WaitItem::PromoteFromQueue { .. })),
            "the work admission found: {items:?}"
        );
        assert!(
            items.contains(&held),
            "AND what was held for this author, in the same frame: {items:?}"
        );
        assert!(pending.is_empty(), "delivered, so no longer held");
    }

    /// A transient arriving after a client was woken stays pending.
    ///
    /// A successful enqueue is not evidence anyone will read it: the
    /// wait returned on its first non-empty frame, so a second one is
    /// written into a socket nobody is listening to. Clearing the
    /// pending buffer against that frame loses the event (codex on
    /// 1552f66).
    #[tokio::test]
    async fn a_transient_after_a_wake_waits_for_the_next_admission() {
        let repo = repo_with_roster();
        std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
        std::fs::write(
            repo.path().join(".clank/queue/500-next.md"),
            "# next\n\nsomething to promote\n",
        )
        .unwrap();

        let claude = AgentLabel::parse("claude").unwrap();
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        // The receiver is HELD open, so an enqueue would succeed — which
        // is exactly what made this look safe.
        let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: claude.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];
        let mut pending = Vec::new();

        // One ordinary wake.
        serve_to(repo.path(), &mut clients, &state, None, &mut pending);
        assert!(
            matches!(
                serde_json::from_str::<Note>(heard.recv().await.unwrap().trim_end()).unwrap(),
                Note::Items { .. }
            ),
            "the first frame wakes it"
        );
        assert!(clients.is_empty(), "and ends that connection");

        // Now a transient. There is no eligible client, so it waits.
        let later = WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "deploys".into(),
            id: "b".into(),
            event: "deploys".into(),
            content: "and then this".into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        pending.push((claude.clone(), later.clone()));
        serve_to(repo.path(), &mut clients, &state, None, &mut pending);
        assert_eq!(
            pending,
            vec![(claude.clone(), later.clone())],
            "it must not be cleared against a frame nobody will read"
        );
        assert!(
            heard.try_recv().is_err(),
            "and nothing was queued behind the wake"
        );

        // The next admission is what receives it.
        let (out, mut next) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        admit_into(
            repo.path(),
            &mut clients,
            Client {
                writer: None,
                who: Served::Agent {
                    author: claude.clone(),
                },
                out,
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            },
            Some(&state),
            &mut pending,
            &Default::default(),
        );
        let note: Note = serde_json::from_str(next.recv().await.unwrap().trim_end()).unwrap();
        let Note::Items { items, .. } = note else {
            panic!("the next admission is owed it, got {note:?}")
        };
        assert!(items.contains(&later), "{items:?}");
        assert!(pending.is_empty());
    }

    /// The monitor serves an author's CONFIGURED logs and no others.
    ///
    /// Both logs sit in one directory and only the configured row is
    /// served, because the monitor opens the keys it resolves from
    /// config rather than whatever is in the directory — so a scoped
    /// log is never one it reads.
    ///
    /// Exclusion only. It starts no producer and has no invocation
    /// receiver, and an earlier version of this comment claimed both;
    /// the delivery half is
    /// `wait::a_cli_author_keeps_its_own_channel_while_another_shares`
    /// (codex on 1756c68).
    #[tokio::test]
    async fn the_monitor_reads_only_an_authors_configured_logs() {
        use crate::cli::github::log::{EventLog, Transport};

        let repo = repo_with_roster();
        with_source(repo.path(), "claude", "builds");
        let claude = AgentLabel::parse("claude").unwrap();
        let events = crate::agent_store::agents_root(repo.path())
            .join("claude")
            .join("events");

        // What the monitor hosts: this author's configured source, with
        // a row in it.
        let configured = match &hostable(repo.path(), &claude).unwrap()[0].0 {
            clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
            other => panic!("a command source, got {other:?}"),
        };
        let row = |content: &str| WaitItem::CommandEvent {
            presentation: None,
            source_cwd: None,
            name: "builds".into(),
            id: content.into(),
            event: "builds".into(),
            content: content.into(),
            at: None,
            failure: false,
            requires_reply: false,
            reply_route: None,
            selector: None,
        };
        EventLog::open(&events, &configured)
            .unwrap()
            .append_inbox(Transport::Command, None, None, None, &row("configured"))
            .unwrap();

        // And an INVOCATION's own source, under its scoped key, with a
        // row of its own. This is the log the monitor must not read.
        let invocation = crate::cli::command_source::storage_key(
            configured.clone(),
            crate::cli::event_source::SourceOrigin::Cli,
            "inv-9",
        );
        assert_ne!(invocation, configured);
        EventLog::open(&events, &invocation)
            .unwrap()
            .append_inbox(Transport::Command, None, None, None, &row("invocation"))
            .unwrap();

        let (out, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: claude.clone(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        serve_to(repo.path(), &mut clients, &state, None, &mut Vec::new());

        let Note::Items { items, .. } =
            serde_json::from_str::<Note>(heard.recv().await.unwrap().trim_end()).unwrap()
        else {
            panic!("the configured row is owed")
        };
        let contents: Vec<String> = items
            .iter()
            .filter_map(|i| match i {
                WaitItem::CommandEvent { content, .. } => Some(content.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            contents,
            vec!["configured".to_string()],
            "the monitor serves the author's CONFIGURED rows and cannot \
             see an invocation's: it resolves only config keys, so a \
             scoped log is not a log it reads — {contents:?}"
        );
    }

    /// Two authors in one repo: each one's rows reach only that author.
    ///
    /// Both have a config source, so the monitor hosts both, and the
    /// question is whether a row logged for one can reach the other —
    /// the failure that would make hosting worse than not hosting.
    /// CONFIG routing only; the mixed config/CLI case is its own test.
    #[tokio::test]
    async fn two_authors_sources_do_not_cross() {
        use crate::cli::github::log::{EventLog, Transport};

        let repo = repo_with_roster();
        with_source(repo.path(), "codex", "deploys");
        with_source(repo.path(), "claude", "builds");

        let row = |who: &str, content: &str| {
            let author = AgentLabel::parse(who).unwrap();
            let events = crate::agent_store::agents_root(repo.path())
                .join(who)
                .join("events");
            let key = match &hostable(repo.path(), &author).unwrap()[0].0 {
                clank_core::agent_config::WaitEventSource::Command(c) => c.source_key(),
                other => panic!("a command source, got {other:?}"),
            };
            EventLog::open(&events, &key)
                .unwrap()
                .append_inbox(
                    Transport::Command,
                    None,
                    None,
                    None,
                    &WaitItem::CommandEvent {
                        presentation: None,
                        source_cwd: None,
                        name: "s".into(),
                        id: content.into(),
                        event: "s".into(),
                        content: content.into(),
                        at: None,
                        failure: false,
                        requires_reply: false,
                        reply_route: None,
                        selector: None,
                    },
                )
                .unwrap();
        };
        row("codex", "theirs");
        row("claude", "mine");

        let (a, mut codex_heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let (b, mut claude_heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        // Both are plain shared clients. Invocation-source isolation is
        // covered by `an_invocations_event_reaches_it_and_not_the_monitor`.
        let agent = |who: &str, out| Client {
            writer: None,
            who: Served::Agent {
                author: AgentLabel::parse(who).unwrap(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        };
        let mut clients = vec![agent("codex", a), agent("claude", b)];
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        serve_to(repo.path(), &mut clients, &state, None, &mut Vec::new());

        let content = |line: String| -> Vec<String> {
            let Note::Items { items, .. } = serde_json::from_str(line.trim_end()).unwrap() else {
                panic!("owed its own row")
            };
            items
                .iter()
                .filter_map(|i| match i {
                    WaitItem::CommandEvent { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(
            content(codex_heard.recv().await.unwrap()),
            vec!["theirs".to_string()],
            "codex is owed its own row and nothing of claude's"
        );
        assert_eq!(
            content(claude_heard.recv().await.unwrap()),
            vec!["mine".to_string()],
            "and claude the same, the other way"
        );
        assert!(
            clients.is_empty(),
            "both were woken, and a woken connection is spent"
        );
    }

    /// A newcomer is presented to ALONE.
    ///
    /// A client's queue is `CLIENT_QUEUE` deep, so presenting to
    /// everybody on each arrival lets a burst of joins retire
    /// established readers that were keeping up perfectly well — the
    /// same amplification the source path avoids, through the door the
    /// source path does not use (codex on 0555c8f).
    #[tokio::test]
    async fn arrivals_do_not_put_frames_in_established_clients_queues() {
        let repo = repo_with_roster();
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let (out, mut established) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let mut clients = vec![Client {
            writer: None,
            who: Served::Agent {
                author: AgentLabel::parse("claude").unwrap(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        }];

        // More arrivals than a queue is deep, none of them drained.
        // Their receivers are HELD: a dropped one closes the channel,
        // which would retire the newcomer for a reason this test is
        // not about.
        let mut newcomers = Vec::new();
        for _ in 0..(CLIENT_QUEUE + 2) {
            let (out, rx) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
            newcomers.push(rx);
            admit_into(
                repo.path(),
                &mut clients,
                Client {
                    writer: None,
                    who: Served::Agent {
                        author: AgentLabel::parse("codex").unwrap(),
                    },
                    out,
                    holder: None,
                    shown: Default::default(),
                    conn: 0,
                    report: None,
                    farewell: None,
                },
                Some(&state),
                &mut Vec::new(),
                &Default::default(),
            );
        }

        assert!(
            established.try_recv().is_err(),
            "an established client is sent nothing because others joined"
        );
        assert!(
            clients.iter().any(|c| c.who.name() == "claude"),
            "and it is still connected: {:?}",
            clients.iter().map(|c| c.who.name()).collect::<Vec<_>>()
        );
    }

    /// A source signal presents to that author's clients and no others.
    ///
    /// Scope matters beyond tidiness: a client's queue is only
    /// `CLIENT_QUEUE` deep, so presenting to everybody whenever one
    /// author's log moved would put frames in queues that had no
    /// reason to receive them, and a burst would retire readers that
    /// were keeping up.
    #[tokio::test]
    async fn a_source_signal_presents_only_to_its_own_author() {
        let repo = repo_with_roster();
        let (mine, mut heard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let (theirs, mut unheard) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let (watching, mut observer) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let agent = |author: &str, out| Client {
            writer: None,
            who: Served::Agent {
                author: AgentLabel::parse(author).unwrap(),
            },
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
        };
        let mut clients = vec![
            agent("codex", mine),
            agent("claude", theirs),
            Client {
                writer: None,
                who: Served::Observer,
                out: watching,
                holder: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: None,
            },
        ];

        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        serve_to(
            repo.path(),
            &mut clients,
            &state,
            Some(&AgentLabel::parse("codex").unwrap()),
            &mut Vec::new(),
        );

        assert!(
            heard.try_recv().is_ok(),
            "the author whose log moved is presented to"
        );
        assert!(
            unheard.try_recv().is_err(),
            "another author's client must hear nothing"
        );
        assert!(
            observer.try_recv().is_err(),
            "and an observer is not owed anybody's items"
        );
        assert_eq!(clients.len(), 3, "nobody was retired for this");

        // Unscoped, everybody is presented to — which is what a fold
        // tick does and why the scope is a parameter rather than the
        // rule.
        serve_to(repo.path(), &mut clients, &state, None, &mut Vec::new());
        assert!(unheard.try_recv().is_ok());
        assert!(observer.try_recv().is_ok());
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
            writer: None,
            who: Served::Observer,
            out,
            holder: None,
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: None,
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

        serve(repo.path(), &mut clients, &state);
        assert_eq!(clients.len(), 1, "an observer stays registered");
        let line = queued.recv().await.expect("it was sent something");
        assert_eq!(
            serde_json::from_str::<Note>(line.trim_end()).unwrap(),
            Note::Beat,
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
            match watchdog_look(seen, *now, missed, limit, Retiring::No) {
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
                turn_end: None,
            },
        };
        let back: Hello = serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
        assert_eq!(back, hello);

        for note in [
            Note::Beat,
            Note::Items {
                source_cwd: None,
                items: vec![WaitItem::PromoteFromQueue {
                    name: "next".into(),
                    priority: 500,
                }],
            },
        ] {
            let back: Note = serde_json::from_str(&serde_json::to_string(&note).unwrap()).unwrap();
            assert_eq!(back, note);
        }

        for welcome in [
            Welcome::Ok { proto: PROTO },
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

#[cfg(test)]
mod lease_tests {
    use super::*;
    use std::cell::Cell;

    fn me() -> u32 {
        unsafe { libc::getuid() }
    }

    /// A namespace of its own and the lease path in it.
    fn namespace() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("ns");
        let path = dir.join("repo.lock");
        (home, dir, path)
    }

    fn take(dir: &std::path::Path, path: &std::path::Path) -> Result<MonitorLease, LeaseError> {
        acquire_lease_with(dir, path.to_path_buf(), me(), |_| {})
    }

    /// Put a new file at `path`, as an outside cleanup and recreation
    /// would.
    fn replace(path: &std::path::Path) {
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, b"").unwrap();
        std::fs::rename(&tmp, path).unwrap();
    }

    fn id_at(path: &std::path::Path) -> FileId {
        FileId::of(&std::fs::metadata(path).unwrap())
    }

    #[test]
    fn a_lease_is_owned_only_when_the_file_locked_is_the_file_at_the_path() {
        let (_home, dir, path) = namespace();
        let lease = take(&dir, &path).expect("unchanged: verified");
        assert_eq!(lease.held(), id_at(&path));
        assert_eq!(lease.check(), Found::Held);
        assert!(matches!(take(&dir, &path), Err(LeaseError::Busy(_))));
        drop(lease);

        let always = acquire_lease_with(&dir, path.clone(), me(), replace);
        assert!(
            matches!(&always, Err(LeaseError::Unverifiable(why)) if why.contains("kept changing")),
            "replaced between open and lock, every time: {always:?}"
        );

        let opened = Cell::new(0);
        let once = acquire_lease_with(&dir, path.clone(), me(), |p| {
            opened.set(opened.get() + 1);
            if opened.get() == 1 {
                replace(p);
            }
        })
        .expect("replaced once, then verified on the retry");
        assert_eq!(opened.get(), 2);
        assert_eq!(once.held(), id_at(&path));
    }

    fn counting() -> (std::rc::Rc<Cell<u32>>, impl Fn() -> Result<(), String>) {
        let count = std::rc::Rc::new(Cell::new(0));
        let c = count.clone();
        (count, move || {
            c.set(c.get() + 1);
            Ok(())
        })
    }

    #[test]
    fn an_unchanged_lease_only_looks() {
        let (_home, dir, path) = namespace();
        let mut tenure = Tenure::new(take(&dir, &path).unwrap());
        let (published, publish) = counting();
        assert_eq!(tenure.settle(&publish), Settled::Owner);
        assert!(tenure.may_act(&publish));
        assert_eq!(published.get(), 0);
    }

    /// Re-taken, verified, and only then republished; afterwards it is
    /// the lease again, which a second monitor cannot take.
    #[test]
    fn a_removed_lease_is_retaken_before_it_is_republished() {
        let (_home, dir, path) = namespace();
        let lease = take(&dir, &path).unwrap();
        let old = lease.held();
        let mut tenure = Tenure::new(lease);
        std::fs::remove_file(&path).unwrap();

        let published = Cell::new(0);
        let publish = || {
            assert!(
                matches!(take(&dir, &path), Err(LeaseError::Busy(_))),
                "published before the lease was verifiably held"
            );
            published.set(published.get() + 1);
            Ok(())
        };
        let Settled::Retaken(detection) = tenure.settle(&publish) else {
            panic!("not re-taken");
        };
        assert_eq!(published.get(), 1);
        assert_eq!(detection.found, Found::Absent);
        let said = detection.to_string();
        for part in [
            path.display().to_string(),
            old.to_string(),
            "absent".into(),
            id_at(&path).to_string(),
        ] {
            assert!(said.contains(&part), "{part:?} missing from {said:?}");
        }
        assert!(
            said.contains(&time::OffsetDateTime::now_utc().year().to_string()),
            "no time in {said:?}"
        );
        assert!(matches!(take(&dir, &path), Err(LeaseError::Busy(_))));
        assert_eq!(tenure.settle(&publish), Settled::Owner);
        assert_eq!(published.get(), 1);
    }

    #[test]
    fn a_lease_another_holds_is_retired_from_and_never_republished() {
        let (_home, dir, path) = namespace();
        let lease = take(&dir, &path).unwrap();
        let old = lease.held();
        let mut tenure = Tenure::new(lease);
        std::fs::remove_file(&path).unwrap();
        let _theirs = take(&dir, &path).unwrap();
        let (published, publish) = counting();

        let Settled::Retire(detection) = tenure.settle(&publish) else {
            panic!("kept acting as owner beside another");
        };
        assert_eq!(detection.found, Found::Other(id_at(&path)));
        let said = detection.to_string();
        assert!(said.contains(&old.to_string()) && said.contains(&id_at(&path).to_string()));
        assert!(said.contains("retiring"), "{said}");
        assert!(!tenure.may_act(&publish), "fenced from then on");
        assert!(matches!(tenure.settle(&publish), Settled::Retire(_)));
        assert_eq!(published.get(), 0);
    }

    fn woken(watch: &LeaseWatch) -> bool {
        use futures_util::FutureExt;
        watch.wake.notified().now_or_never().is_some()
    }

    /// The watchdog's look changes nothing: it wakes the loop, which
    /// looks for itself and acts, once, however many looks saw it.
    #[test]
    fn the_watchdog_only_wakes_and_the_loop_acts_once() {
        let (_home, dir, path) = namespace();
        let mut tenure = Tenure::new(take(&dir, &path).unwrap());
        let watch = tenure.watch.clone();
        assert!(!woken(&watch));
        std::fs::remove_file(&path).unwrap();
        watch.observe();
        watch.observe();
        assert!(!path.exists(), "the watchdog re-took the lease");
        assert!(woken(&watch));
        assert!(!woken(&watch), "two looks, one wake");
        let (published, publish) = counting();

        assert!(tenure.may_act(&publish), "the wake's look re-takes it");
        assert_eq!(published.get(), 1);
        assert_eq!(tenure.settle(&publish), Settled::Owner);
        watch.observe();
        assert!(!woken(&watch), "the watchdog sees the file re-taken");
        assert_eq!(published.get(), 1, "one action");
    }

    /// A wake is looked at before it is acted on, whether or not the
    /// watchdog has seen anything: a replacement since the last look
    /// fences the first owner action after it.
    #[test]
    fn a_wake_after_an_unseen_replacement_is_fenced() {
        let (_home, dir, path) = namespace();
        let mut tenure = Tenure::new(take(&dir, &path).unwrap());
        let (published, publish) = counting();
        assert_eq!(tenure.settle(&publish), Settled::Owner);
        std::fs::remove_file(&path).unwrap();
        let _successor = take(&dir, &path).unwrap();
        assert!(!tenure.may_act(&publish));
        assert!(tenure.retiring());
        assert_eq!(published.get(), 0);
    }

    /// A retirement under way is judged by its deadline: a fold already
    /// old when it began does not force it out, an overrun does, and a
    /// stall outside one still does.
    #[test]
    fn a_retirement_is_judged_by_its_deadline_not_the_folds_age() {
        assert_eq!(
            watchdog_look(7, 7, 3, 4, Retiring::Within),
            Watch::Keep { tick: 7, missed: 0 }
        );
        assert_eq!(
            watchdog_look(7, 8, 0, 4, Retiring::Overdue),
            Watch::StandDown
        );
        assert_eq!(watchdog_look(7, 7, 3, 4, Retiring::No), Watch::StandDown);

        let (_home, dir, path) = namespace();
        let watch = LeaseWatch::new(&take(&dir, &path).unwrap());
        let margin = Duration::from_secs(20);
        assert_eq!(watch.retiring(unix_ms(), margin), Retiring::No);
        watch.begin_retiring(Duration::from_secs(13));
        let now = unix_ms();
        assert_eq!(watch.retiring(now, margin), Retiring::Within);
        assert_eq!(watch.retiring(now + 32_000, margin), Retiring::Within);
        assert_eq!(watch.retiring(now + 34_000, margin), Retiring::Overdue);
    }

    /// One timeline from the cutoff: a greeting that will not end and a
    /// producer that ignores TERM together still finish inside the
    /// budget and its settle allowance, rather than each stage starting
    /// its own grace when the last one ran out.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_retirement_keeps_one_whole_deadline() {
        use crate::cli::source_group::{self, tests::sh};
        let owners = source_group::tests::owners();
        let dir = tempfile::tempdir().unwrap();
        let mut command = sh("trap '' TERM; echo $$ > ready; exec sleep 300");
        command.current_dir(dir.path());
        let lease = std::sync::Arc::new(
            crate::cli::github::log::IngestLease::try_acquire(dir.path(), "src")
                .unwrap()
                .unwrap(),
        );
        let (group, mut child) = source_group::launch(
            owners,
            &source_group::Arming::standard(),
            dir.path(),
            "src",
            lease,
            crate::cli::command_source::DEFAULT_STOP_GRACE,
            &mut command,
        )
        .await
        .unwrap();
        let host = tokio::spawn(async move {
            let _ = child.wait().await;
            let _ = tokio::task::spawn_blocking(move || group.retire()).await;
        });
        let stubborn = source_group::tests::pid_in(&dir.path().join("ready")).await;
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let (ready, started) = std::sync::mpsc::channel();
        let greeting = tokio::task::spawn_blocking(move || {
            ready.send(()).unwrap();
            let _ = hold.recv();
        });
        tokio::task::spawn_blocking(move || started.recv())
            .await
            .unwrap()
            .unwrap();
        let mut tasks = OwnedTasks::default();
        tasks.greeting.push(greeting);
        let (budget, settle) = (Duration::from_millis(325), Duration::from_millis(20));

        let started = std::time::Instant::now();
        let decision = stand_down(
            dir.path(),
            Vec::new(),
            Vec::new(),
            Default::default(),
            tasks,
            &mut tokio::sync::mpsc::unbounded_channel().1,
            &mut tokio::sync::mpsc::unbounded_channel().1,
            None,
            &mut Vec::new(),
            Some(Drain {
                owners,
                grace: Duration::from_millis(250),
            }),
            budget,
            settle,
        )
        .await;
        let took = started.elapsed();
        let killed = crate::signal::probe_process(stubborn) == crate::signal::Probe::Gone;
        release.send(()).unwrap();
        host.await.unwrap();
        assert!(killed, "the producer outlived the deadline");
        assert!(matches!(decision, Shutdown::RequiresProcessExit(_)));
        assert!(
            took < budget + settle + Duration::from_millis(100),
            "the retirement overran its one deadline: {took:?}"
        );
    }

    /// A producer that ignores TERM, under a whole deadline shorter than
    /// its grace.
    async fn stubborn(
        owners: &'static crate::cli::source_group::Owners,
        dir: &std::path::Path,
        script: &str,
    ) -> tokio::task::JoinHandle<()> {
        use crate::cli::source_group::{self, tests::sh};
        let mut command = sh(script);
        command.current_dir(dir);
        let lease = std::sync::Arc::new(
            crate::cli::github::log::IngestLease::try_acquire(dir, "src")
                .unwrap()
                .unwrap(),
        );
        let (group, mut child) = source_group::launch(
            owners,
            &source_group::Arming::standard(),
            dir,
            "src",
            lease,
            crate::cli::command_source::DEFAULT_STOP_GRACE,
            &mut command,
        )
        .await
        .unwrap();
        let host = tokio::spawn(async move {
            let _ = child.wait().await;
            let _ = tokio::task::spawn_blocking(move || group.retire()).await;
        });
        source_group::tests::pid_in(&dir.join("ready")).await;
        host
    }

    async fn draining_stand_down(
        owners: &'static crate::cli::source_group::Owners,
        grace: Duration,
        budget: Duration,
        settle: Duration,
    ) -> (Shutdown, Duration) {
        let started = std::time::Instant::now();
        let decision = stand_down(
            std::path::Path::new("/w/proj"),
            Vec::new(),
            Vec::new(),
            Default::default(),
            OwnedTasks::default(),
            &mut tokio::sync::mpsc::unbounded_channel().1,
            &mut tokio::sync::mpsc::unbounded_channel().1,
            None,
            &mut Vec::new(),
            Some(Drain { owners, grace }),
            budget,
            settle,
        )
        .await;
        (decision, started.elapsed())
    }

    /// A drain whose groups are not yet retired at the whole deadline is
    /// never an orderly release, and it does not wait past the deadline.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_drain_unfinished_at_the_deadline_forbids_release() {
        let owners = crate::cli::source_group::tests::owners();
        let dir = tempfile::tempdir().unwrap();
        let host = stubborn(
            owners,
            dir.path(),
            "trap '' TERM; echo $$ > ready; exec sleep 300",
        )
        .await;
        let (budget, settle) = (Duration::from_millis(50), Duration::from_millis(10));
        let (decision, took) =
            draining_stand_down(owners, Duration::from_millis(500), budget, settle).await;
        host.await.unwrap();
        assert!(matches!(decision, Shutdown::RequiresProcessExit(_)));
        assert!(
            took < budget + settle + Duration::from_millis(100),
            "{took:?}"
        );
    }

    /// A group whose end its retirement could not confirm forbids an
    /// orderly release.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unconfirmed_retirement_forbids_release() {
        let owners = crate::cli::source_group::tests::owners_unconfirmed_in_a_drain();
        let dir = tempfile::tempdir().unwrap();
        let host = stubborn(
            owners,
            dir.path(),
            "trap 'exit 0' TERM; echo $$ > ready; while :; do sleep 0.1; done",
        )
        .await;
        let (decision, _) = draining_stand_down(
            owners,
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_millis(100),
        )
        .await;
        host.await.unwrap();
        assert!(matches!(decision, Shutdown::RequiresProcessExit(_)));
    }

    /// A host still reading its producer's output at the deadline forbids
    /// an orderly release, and is not waited for past it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reader_outstanding_at_the_deadline_forbids_release() {
        let owners = crate::cli::source_group::tests::owners();
        let _reading = owners.reading(std::path::PathBuf::from("/src.producer"));
        let (budget, settle) = (Duration::from_millis(100), Duration::from_millis(10));
        let (decision, took) = draining_stand_down(owners, Duration::ZERO, budget, settle).await;
        assert!(matches!(decision, Shutdown::RequiresProcessExit(_)));
        assert!(
            took < budget + settle + Duration::from_millis(100),
            "{took:?}"
        );
    }

    /// Settling a drain needs nothing from the blocking pool, so a full
    /// one cannot hold a retirement past its deadline.
    #[test]
    fn a_full_blocking_pool_does_not_hold_a_drain_past_its_deadline() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let owners = crate::cli::source_group::tests::owners();
            let (release, hold) = std::sync::mpsc::channel::<()>();
            let (ready, started) = std::sync::mpsc::channel();
            let occupant = tokio::task::spawn_blocking(move || {
                ready.send(()).unwrap();
                let _ = hold.recv();
            });
            started.recv_timeout(Duration::from_secs(1)).unwrap();
            let retirement = tokio::spawn(draining_stand_down(
                owners,
                Duration::ZERO,
                Duration::from_millis(50),
                Duration::from_millis(10),
            ));
            tokio::time::sleep(Duration::from_millis(200)).await;
            let in_time = retirement.is_finished();
            release.send(()).unwrap();
            occupant.await.unwrap();
            let _ = retirement.await.unwrap();
            assert!(in_time, "the retirement waited on the blocking pool");
        });
    }

    /// The stand-down a lease loss leads to drains, through the real
    /// source host: a root that exits on TERM leaves a descendant
    /// finishing accepted work for three seconds, and what it prints when
    /// done is logged before the host is cancelled.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_lease_retirement_drains_its_producers() {
        let owners = crate::cli::source_group::tests::owners();
        let root = tempfile::tempdir().unwrap();
        let events = root.path().join("events");
        std::fs::create_dir_all(&events).unwrap();
        let ready = root.path().join("ready");
        let worker = root.path().join("worker.sh");
        std::fs::write(
            &worker,
            format!(
                "trap '' TERM\ntouch '{}'\nsleep 3\nprintf '%s\\n' '{{\"id\":\"late\",\"content\":\"finished\"}}'\n",
                ready.display()
            ),
        )
        .unwrap();
        let src = clank_core::agent_config::CommandSource {
            interrupt: false,
            interrupt_message: None,
            name: "drained".into(),
            command: [
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "sh '{}' & trap 'exit 0' TERM; while :; do sleep 0.1; done",
                    worker.display()
                ),
            ]
            .to_vec(),
            reply_command: None,
            receipt_command: None,
            glyph: None,
            stop_grace: None,
        };
        let key = src.source_key();
        let author = AgentLabel::parse("alpha").unwrap();
        let (into, mut from_sources) = tokio::sync::mpsc::unbounded_channel();
        let handle = tokio::spawn(crate::cli::wait::run_command_source_with(
            src,
            crate::cli::event_source::EmissionSink::for_author(into, author.clone()),
            Some(events.clone()),
            key.clone(),
            crate::cli::command_source::reap_previous_producer,
            owners,
            crate::cli::wait::StopRequest::never(),
        ));
        for _ in 0..500 {
            if ready.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists());
        let hosted = std::collections::BTreeMap::from([(
            author,
            Hosted {
                spec: Vec::new(),
                tasks: std::collections::BTreeMap::from([(
                    key.clone(),
                    HostedTask {
                        instance: 1,
                        handle,
                        stop: None,
                    },
                )]),
                timers: None,
            },
        )]);

        let (_joined, mut arrivals) = tokio::sync::mpsc::unbounded_channel::<Greeted>();
        let decision = stand_down(
            std::path::Path::new("/w/proj"),
            Vec::new(),
            Vec::new(),
            hosted,
            OwnedTasks::default(),
            &mut arrivals,
            &mut from_sources,
            None,
            &mut Vec::new(),
            Some(Drain {
                owners,
                grace: Duration::from_secs(6),
            }),
            Duration::from_secs(9),
            Duration::from_millis(500),
        )
        .await;
        assert!(matches!(decision, Shutdown::ReadyToRelease(_)));
        let rows = crate::cli::github::log::EventLog::open(&events, &key)
            .unwrap()
            .read_rows()
            .unwrap()
            .unwrap()
            .rows;
        assert!(
            rows.iter()
                .any(|row| format!("{:?}", row.item).contains("finished")),
            "the work's output was lost to the cancellation"
        );
    }
}

#[cfg(test)]
mod build_tests {
    use super::*;
    use crate::build_stamp::{Install, Installation, Rendezvous, Source, Stamp, running};

    fn other() -> Stamp {
        let mut stamp = running().0;
        stamp[0] ^= 1;
        Stamp(stamp)
    }

    fn install(path: &std::path::Path, stamp: Stamp) {
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, crate::build_stamp::executable_with(stamp)).unwrap();
        std::fs::rename(tmp, path).unwrap();
    }

    fn noted() -> (
        std::sync::Arc<std::sync::Mutex<Vec<Install>>>,
        Box<dyn Fn(&Install) + Send>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let into = seen.clone();
        (
            seen,
            Box::new(move |i: &Install| into.lock().unwrap().push(i.clone())),
        )
    }

    fn counting() -> (
        std::rc::Rc<std::cell::Cell<u32>>,
        impl Fn() -> Result<(), String>,
    ) {
        let count = std::rc::Rc::new(std::cell::Cell::new(0));
        let c = count.clone();
        (count, move || {
            c.set(c.get() + 1);
            Ok(())
        })
    }

    fn repo() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A replaced build retires without repairing a lost lease or
    /// republishing, and stays retired; an unavailable one keeps owning.
    #[test]
    fn a_replaced_build_retires_and_never_repairs() {
        let repo = repo();
        let bin = tempfile::tempdir().unwrap();
        let path = bin.path().join("clank");
        install(&path, running());
        let (seen, note) = noted();
        let lease = acquire_lease(repo.path()).unwrap();
        let lease_path = lease.path().to_path_buf();
        let mut tenure = Tenure::new(lease).with_build(Built::new(
            Installation::at(path.clone()),
            running(),
            note,
        ));
        let (published, publish) = counting();
        assert!(tenure.may_act(&publish));

        std::fs::remove_file(&path).unwrap();
        assert!(
            tenure.may_act(&publish),
            "an unavailable installation keeps ownership"
        );

        install(&path, other());
        std::fs::remove_file(&lease_path).unwrap();
        assert_eq!(
            tenure.settle(&publish),
            Settled::Stale {
                running: running(),
                installed: other()
            }
        );
        assert!(
            !lease_path.exists(),
            "the lease was repaired by a stale build"
        );
        assert!(!tenure.may_act(&publish));
        assert_eq!(published.get(), 0, "a stale build advertised");
        let seen = seen.lock().unwrap();
        assert!(
            matches!(seen.as_slice(), [Install::Current, Install::Unavailable(_), Install::Replaced(s)] if *s == other())
        );
    }

    /// The old monitor's last word lands under its own process, never on
    /// the routing descriptor: a successor that took the repo after the
    /// old one's last owner look keeps it.
    #[test]
    fn a_late_observation_cannot_take_routing_back() {
        let repo = repo();
        let bin = tempfile::tempdir().unwrap();
        let path = bin.path().join("clank");
        install(&path, running());
        let old_pid = std::process::id() as i32;
        let note: Box<dyn Fn(&Install) + Send> = {
            let repo = repo.path().to_path_buf();
            Box::new(move |install: &Install| {
                note_observation(
                    &repo,
                    &Observation {
                        pid: old_pid,
                        birth: crate::proc_identity::token_for(old_pid),
                        running: running().to_string(),
                        installed_at: "/bin/clank".into(),
                        source: Source::Invocation,
                        state: install.into(),
                    },
                )
                .unwrap()
            })
        };
        let lease = acquire_lease(repo.path()).unwrap();
        let lease_path = lease.path().to_path_buf();
        let mut tenure = Tenure::new(lease).with_build(Built::new(
            Installation::at(path.clone()),
            running(),
            note,
        ));
        let (_, owner_publish) = counting();
        assert!(
            tenure.may_act(&owner_publish),
            "the old monitor's last owner look"
        );

        std::fs::remove_file(&lease_path).unwrap();
        let _successor = acquire_lease(repo.path()).unwrap();
        let successor = Descriptor {
            pid: 999_999,
            ..describe(
                repo.path(),
                999_999,
                std::path::Path::new("/tmp/successor.sock"),
            )
        };
        publish(repo.path(), &successor).unwrap();
        install(&path, other());
        assert!(
            !tenure.may_act(&counting().1),
            "a stale build acted as owner"
        );

        assert_eq!(
            read_published(repo.path()),
            Some(successor),
            "routing was taken back"
        );
        let observed: Observation =
            serde_json::from_slice(&std::fs::read(observation_path(repo.path(), old_pid)).unwrap())
                .unwrap();
        assert_eq!(
            observed.state,
            Observed::Replaced {
                installed: other().to_string()
            }
        );
        let _ = std::fs::remove_file(observation_path(repo.path(), old_pid));
    }

    /// Running only for a live pid whose recorded birth, where it has
    /// one, still holds; and a published descriptor never reads as
    /// nothing published.
    #[test]
    fn liveness_is_whether_the_published_process_lives() {
        let repo = repo();
        let me = std::process::id() as i32;
        // Above every platform's pid limit, so no process holds it.
        let dead = i32::MAX;
        let socket = std::path::Path::new("/tmp/x.sock");
        let now = || liveness(repo.path());
        assert_eq!(now(), Liveness::None);
        assert_eq!(now().line(), "none");

        publish(repo.path(), &describe(repo.path(), me, socket)).unwrap();
        assert_eq!(
            now(),
            Liveness::Running {
                pid: me,
                build: None,
                observed: None
            }
        );

        publish(repo.path(), &describe(repo.path(), dead, socket)).unwrap();
        assert_eq!(now(), Liveness::Gone { pid: dead });
        assert_eq!(now().line(), format!("gone (pid {dead} exited)"));

        // `me` stands in for a reused pid: alive, but not the process
        // that wrote the record under that number.
        publish(repo.path(), &describe(repo.path(), me, socket)).unwrap();
        note_observation(
            repo.path(),
            &Observation {
                pid: me,
                birth: Some(crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 0 }),
                running: running().to_string(),
                installed_at: "/bin/clank".into(),
                source: Source::Invocation,
                state: Observed::Current,
            },
        )
        .unwrap();
        assert_eq!(
            now(),
            Liveness::Gone { pid: me },
            "a stranger is not Running"
        );
        let _ = std::fs::remove_file(observation_path(repo.path(), me));

        std::fs::write(
            runtime_path(repo.path(), "json"),
            format!(r#"{{"proto":3,"pid":{me},"repo":"/w","socket":"/tmp/old.sock"}}"#),
        )
        .unwrap();
        assert_eq!(
            now(),
            Liveness::Running {
                pid: me,
                build: None,
                observed: None
            },
            "a legacy descriptor's pid is asked too"
        );
        assert_eq!(now().line(), format!("pid {me}, build unknown (legacy)"));
        std::fs::write(
            runtime_path(repo.path(), "json"),
            format!(r#"{{"proto":3,"pid":{dead}}}"#),
        )
        .unwrap();
        assert_eq!(now(), Liveness::Gone { pid: dead });

        std::fs::write(runtime_path(repo.path(), "json"), r#"{"proto":3}"#).unwrap();
        assert!(
            matches!(now(), Liveness::Unknown(_)),
            "published without a pid is neither absent nor running: {:?}",
            now()
        );
        assert!(now().line().starts_with("unknown: "), "{}", now().line());
        let _ = std::fs::remove_file(runtime_path(repo.path(), "json"));
    }

    /// Status joins the routed monitor to its own observation record,
    /// and believes the record only of the living process it names.
    #[test]
    fn status_shows_the_routed_monitors_build() {
        let repo = repo();
        let me = std::process::id() as i32;
        let build = DescribedBuild {
            running: running().to_string(),
            installed_at: "/bin/clank".into(),
            source: Source::Invocation,
        };
        publish(
            repo.path(),
            &Descriptor {
                build: Some(build.clone()),
                ..describe(repo.path(), me, std::path::Path::new("/tmp/x.sock"))
            },
        )
        .unwrap();
        let observe = |state: Observed, birth| {
            note_observation(
                repo.path(),
                &Observation {
                    pid: me,
                    birth,
                    running: build.running.clone(),
                    installed_at: build.installed_at.clone(),
                    source: build.source,
                    state,
                },
            )
            .unwrap()
        };
        let line = || liveness(repo.path()).line();
        assert!(line().ends_with("not yet observed"), "{}", line());
        observe(Observed::Current, crate::proc_identity::token_for(me));
        assert!(line().ends_with(": current"), "{}", line());
        observe(
            Observed::Replaced {
                installed: other().to_string(),
            },
            crate::proc_identity::token_for(me),
        );
        assert!(line().contains("retiring"), "{}", line());
        observe(
            Observed::Unavailable { why: "gone".into() },
            crate::proc_identity::token_for(me),
        );
        assert!(
            line().contains("install path unavailable: gone"),
            "{}",
            line()
        );
        observe(Observed::Current, None);
        assert!(
            line().ends_with("not yet observed"),
            "a record with no birth is not believed"
        );

        publish(
            repo.path(),
            &describe(repo.path(), me, std::path::Path::new("/tmp/x.sock")),
        )
        .unwrap();
        assert!(line().contains("unknown (legacy)"), "{}", line());
        let _ = std::fs::remove_file(observation_path(repo.path(), me));
        assert_eq!(
            describe_build(
                1,
                Some(&DescribedBuild {
                    source: Source::Fallback,
                    ..build
                }),
                None
            ),
            format!(
                "pid 1, build {} at /bin/clank (fallback path): not yet observed",
                running()
            )
        );
    }

    /// The successor is started from the same path it will compare
    /// against, and told so.
    #[test]
    fn a_monitor_is_started_from_its_rendezvous() {
        let command = monitor_command(
            &Rendezvous {
                path: "/opt/bin/clank".into(),
                source: Source::Invocation,
            },
            std::path::Path::new("/w/repo"),
        );
        assert_eq!(command.get_program(), "/opt/bin/clank");
        let args: Vec<_> = command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "monitor",
                "start",
                "--repo",
                "/w/repo",
                "--installed-at",
                "/opt/bin/clank",
                "--installed-from",
                "invocation"
            ]
        );
    }

    /// A proto-12 client is still refused by this build, so it works
    /// alone rather than being served by a monitor it cannot read.
    #[test]
    fn an_older_client_is_declined() {
        let repo = repo();
        let hello = format!(
            "{{\"proto\":12,\"repo\":\"{}\",\"who\":{{\"as\":\"observer\"}}}}",
            repo.path().display()
        );
        assert!(matches!(
            greeting_of(&hello, repo.path()),
            Greeting::Decline(Welcome::Decline { proto: PROTO, .. })
        ));
    }
}

#[cfg(test)]
mod superseding_tests {
    use super::*;

    const ROSTER: &str = r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join(".clank")).unwrap();
        std::fs::write(d.path().join(".clank/config.json"), ROSTER).unwrap();
        d
    }

    fn claude() -> AgentLabel {
        AgentLabel::parse("claude").unwrap()
    }

    fn bind(repo: &std::path::Path, session: &str) {
        crate::agent_store::bind_session_to_agent(
            repo,
            &claude(),
            clank_core::Tool::Claude,
            &clank_core::SessionId::parse(session).unwrap(),
        )
        .unwrap();
    }

    fn holder(session: &str) -> Option<Holder> {
        Some(Holder {
            tool: clank_core::Tool::Claude,
            session: session.into(),
        })
    }

    /// A parked agent client with a queue the test reads, and the slot
    /// its farewell lands in.
    fn parked(
        who: Served,
        holder: Option<Holder>,
    ) -> (
        Client,
        tokio::sync::mpsc::Receiver<String>,
        tokio::sync::oneshot::Receiver<String>,
    ) {
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let (farewell, last) = tokio::sync::oneshot::channel();
        (
            Client {
                who,
                holder,
                out,
                writer: None,
                shown: Default::default(),
                conn: 0,
                report: None,
                farewell: Some(farewell),
            },
            queued,
            last,
        )
    }

    fn agent() -> Served {
        Served::Agent { author: claude() }
    }

    fn admit(repo: &std::path::Path, clients: &mut Vec<Client>, client: Client) {
        let _ = admit_into(
            repo,
            clients,
            client,
            None,
            &mut Vec::new(),
            &Default::default(),
        );
    }

    /// The wait admitted last is the session's: the older one is told it
    /// is superseded and dropped, so only the newer is ever served.
    #[tokio::test]
    async fn a_newer_wait_of_the_same_session_supersedes_the_older() {
        let repo = repo();
        bind(repo.path(), "session-a1");
        let mut clients = Vec::new();
        let (older, mut older_queue, older_farewell) = parked(agent(), holder("session-a1"));
        admit(repo.path(), &mut clients, older);
        let (newer, _queue, _farewell) = parked(agent(), holder("session-a1"));
        admit(repo.path(), &mut clients, newer);

        assert_eq!(clients.len(), 1);
        let line = older_farewell.await.expect("the older wait was told");
        assert_eq!(
            serde_json::from_str::<Note>(line.trim_end()).unwrap(),
            Note::Superseded
        );
        assert!(older_queue.recv().await.is_none(), "its queue is closed");
    }

    /// Only the same agent session's wait is replaced: observers,
    /// helpers, holderless clients, other sessions and other authors are
    /// not.
    #[test]
    fn only_the_same_agent_session_is_the_same_wait() {
        let new = parked(agent(), holder("s1")).0;
        let same = |who, holder| same_wait(&parked(who, holder).0, &new);
        assert!(same(agent(), holder("s1")));
        assert!(!same(agent(), holder("s2")));
        assert!(!same(agent(), None));
        assert!(!same(Served::Observer, holder("s1")));
        assert!(!same(Served::Helper { author: claude() }, holder("s1")));
        assert!(!same(
            Served::Agent {
                author: AgentLabel::parse("codex").unwrap()
            },
            holder("s1")
        ));
        let holderless = parked(agent(), None).0;
        assert!(!same_wait(&parked(agent(), None).0, &holderless));
    }

    /// A newcomer the admission rules refuse displaces nothing.
    #[tokio::test]
    async fn a_refused_newcomer_displaces_nothing() {
        let repo = repo();
        bind(repo.path(), "session-a1");
        let mut clients = Vec::new();
        let (older, _queue, mut older_farewell) = parked(agent(), holder("session-a1"));
        admit(repo.path(), &mut clients, older);
        bind(repo.path(), "session-b2");
        let (stale, _q, _f) = parked(agent(), holder("session-a1"));
        admit(repo.path(), &mut clients, stale);
        assert_eq!(clients.len(), 1);
        assert!(
            older_farewell.try_recv().is_err(),
            "the older was superseded by a refusal"
        );
    }

    /// The farewell is written after whatever was queued, by the same
    /// writer, so a full queue cannot drop it.
    #[tokio::test]
    async fn the_farewell_survives_a_full_queue() {
        use tokio::io::AsyncBufReadExt;
        let (ours, theirs) = tokio::net::UnixStream::pair().unwrap();
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let (farewell, last) = tokio::sync::oneshot::channel();
        let beat = format!("{}\n", serde_json::to_string(&Note::Beat).unwrap());
        for _ in 0..CLIENT_QUEUE {
            out.try_send(beat.clone()).unwrap();
        }
        assert!(out.try_send(beat.clone()).is_err(), "the queue is full");
        let writer = tokio::spawn(write_to_within(
            ours,
            queued,
            Some(last),
            "claude".into(),
            Duration::from_secs(2),
        ));
        let client = Client {
            who: agent(),
            holder: holder("s1"),
            out,
            writer: Some(writer),
            shown: Default::default(),
            conn: 0,
            report: None,
            farewell: Some(farewell),
        };
        let writer = supersede(client).unwrap();
        let mut lines = tokio::io::BufReader::new(theirs).lines();
        let mut got = Vec::new();
        while let Some(line) = lines.next_line().await.unwrap() {
            got.push(serde_json::from_str::<Note>(&line).unwrap());
        }
        let mut want = vec![Note::Beat; CLIENT_QUEUE];
        want.push(Note::Superseded);
        assert_eq!(got, want);
        assert_eq!(writer.await.unwrap(), Wrote::Drained);
    }

    /// A delivery already dispatched is not retracted: the served wait
    /// has left the parked set, so a later admission of its session
    /// supersedes nothing and is owed work by the normal rules.
    #[tokio::test]
    async fn a_dispatched_delivery_stands() {
        let repo = repo();
        bind(repo.path(), "session-a1");
        std::fs::create_dir_all(repo.path().join(".clank/queue")).unwrap();
        std::fs::write(
            repo.path().join(".clank/queue/500-next.md"),
            "# next\n\nsomething to promote\n",
        )
        .unwrap();
        let state = crate::repo_state::RepoState::empty(repo.path().to_path_buf());
        let mut clients = Vec::new();
        let (first, mut first_queue, mut first_farewell) = parked(agent(), holder("session-a1"));
        let _ = admit_into(
            repo.path(),
            &mut clients,
            first,
            Some(&state),
            &mut Vec::new(),
            &Default::default(),
        );
        let served: Note =
            serde_json::from_str(first_queue.recv().await.unwrap().trim_end()).unwrap();
        assert!(matches!(served, Note::Items { .. }), "{served:?}");

        let (second, mut second_queue, _f) = parked(agent(), holder("session-a1"));
        let _ = admit_into(
            repo.path(),
            &mut clients,
            second,
            Some(&state),
            &mut Vec::new(),
            &Default::default(),
        );
        assert!(
            first_farewell.try_recv().is_err(),
            "a dispatched wait was superseded after the fact"
        );
        let owed: Note =
            serde_json::from_str(second_queue.recv().await.unwrap().trim_end()).unwrap();
        assert!(matches!(owed, Note::Items { .. }), "{owed:?}");
    }
}

#[cfg(test)]
mod user_command_tests {
    use super::*;
    use crate::cli::user_command::envelope::testkey::{Signer, payload};
    use crate::cli::user_command::store::{PaneSeen, State, Store};

    const ROSTER: &str = r#"{"agents":{"claude":{"tool":"claude","role":"master"}}}"#;

    fn claude() -> AgentLabel {
        AgentLabel::parse("claude").unwrap()
    }

    fn sid() -> clank_core::SessionId {
        clank_core::SessionId::parse("session-one").unwrap()
    }

    /// An agent client holding a Stop of `session`, and the queue its
    /// verdicts land in.
    fn holding(session: &str) -> (Client, tokio::sync::mpsc::Receiver<String>) {
        let (out, queued) = tokio::sync::mpsc::channel(CLIENT_QUEUE);
        let client = Client {
            who: Served::Agent { author: claude() },
            holder: None,
            out,
            writer: None,
            shown: Default::default(),
            conn: 7,
            report: Some(Reporting {
                report: crate::cli::turn_wire::Report::TurnEnd {
                    stop: clank_core::turn_policy::TurnEndReport {
                        tool: clank_core::Tool::Claude,
                        session: clank_core::SessionId::parse(session).unwrap(),
                        place: Default::default(),
                        tasks: clank_core::turn_policy::TaskSnapshot::Unavailable,
                    },
                    deadline: u64::MAX,
                    questions: Vec::new(),
                },
                stepped: false,
                reader: None,
            }),
            farewell: None,
        };
        (client, queued)
    }

    struct World {
        repo: std::path::PathBuf,
        home: tempfile::TempDir,
        _dir: tempfile::TempDir,
        site: Signer,
    }

    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(repo.join(".clank")).unwrap();
        std::fs::write(repo.join(".clank/config.json"), ROSTER).unwrap();
        crate::agent_store::bind_session_to_agent(
            &repo,
            &claude(),
            clank_core::Tool::Claude,
            &sid(),
        )
        .unwrap();
        crate::agent_store::set_auto_mode(&repo, &claude(), clank_core::AutoMode::On).unwrap();
        let home = tempfile::tempdir().unwrap();
        let site = Signer::generate();
        crate::cli::user_command::trust_key(home.path(), "site", &site.key().encoded(), false)
            .unwrap();
        World {
            repo,
            home,
            _dir: dir,
            site,
        }
    }

    impl World {
        fn store(&self) -> Store {
            Store::at(self.home.path().join(".clank/user-commands"))
        }

        /// File a request signed by `signer` now, its pane already seen, so
        /// no pass goes looking for one.
        fn file(&self, signer: &Signer) {
            let now = crate::cli::recovery::unix_now();
            let mut p = payload(&self.repo);
            p["session"] = "session-one".into();
            p["issued_at"] = now.into();
            p["expires_at"] = (now + 300).into();
            let tag = repo_tag(&self.repo);
            let id = crate::cli::user_command::envelope::Id::parse("r1").unwrap();
            let held = self.store().lock(&tag, &claude(), &id).unwrap();
            let mut record = crate::cli::user_command::store::Record::admitted(
                signer.sign(&p),
                crate::cli::user_command::trust::Pin {
                    name: crate::cli::user_command::trust::KeyName::parse("site").unwrap(),
                    fingerprint: self.site.key().fingerprint(),
                },
                now,
            );
            record.pane = Some(PaneSeen {
                session: "zs".into(),
                pane: "terminal_1".into(),
                pid: 1,
                birth: crate::proc_identity::ProcToken::MacStart { sec: 1, usec: 0 },
            });
            held.save(&record).unwrap();
        }
    }

    /// Through the loop's own round: a parked Stop stays parked with no
    /// request and with a forged one, and a verified request ends it with
    /// the user-command verdict; the agent is then left settling, and the
    /// round spawns no attempt while it does.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_verified_request_ends_a_park_and_a_forged_one_does_not() {
        let w = world();
        let state = crate::repo_state::RepoState::empty(w.repo.clone());
        let (mut uc, _wakes) = crate::cli::user_command::live::UserCommands::new(
            &w.repo,
            w.home.path().to_path_buf(),
            w.store(),
        );
        let (client, mut queue) = holding("session-one");
        let mut clients = vec![client];
        let mut turns = crate::cli::turn_custody::Turns::new(w.repo.clone());
        let mut recovery =
            crate::cli::recovery::Driver::new(w.repo.clone(), w.home.path().to_path_buf());
        let routes = recovery.check_in_routes();
        let (answers, _a) = tokio::sync::mpsc::unbounded_channel();
        let (asks, _b) = tokio::sync::mpsc::unbounded_channel();
        let (changes, _c) = tokio::sync::mpsc::unbounded_channel();
        let records = crate::cli::recovery::RecordWatch::spawn(changes);
        let mut owned = OwnedTasks::default();
        let mut round = |clients: &mut Vec<Client>,
                         uc: &mut crate::cli::user_command::live::UserCommands,
                         turns: &mut crate::cli::turn_custody::Turns,
                         owned: &mut OwnedTasks| {
            user_command_round(
                &w.repo,
                uc,
                clients,
                Some(&state),
                &[],
                &mut Default::default(),
                &Default::default(),
                turns,
                &mut recovery,
                &routes,
                &answers,
                &asks,
                &records,
                owned,
            )
        };
        let _ = serve_reports(
            &w.repo,
            &mut clients,
            Some(&state),
            None,
            &[],
            &mut Default::default(),
            &Default::default(),
            &mut turns,
            &routes,
        );
        assert!(queue.try_recv().unwrap().contains("park"), "parked first");
        round(&mut clients, &mut uc, &mut turns, &mut owned);
        assert_eq!(clients.len(), 1, "no request, still parked");

        w.file(&Signer::generate());
        round(&mut clients, &mut uc, &mut turns, &mut owned);
        assert_eq!(clients.len(), 1, "a forged request releases nothing");
        let tag = repo_tag(&w.repo);
        let id = crate::cli::user_command::envelope::Id::parse("r1").unwrap();
        assert_eq!(
            w.store().read(&tag, &claude(), &id).unwrap().unwrap().state,
            State::Refused
        );

        std::fs::remove_file(w.store().repo_dir(&tag).join("claude/r1.json")).unwrap();
        w.file(&w.site);
        round(&mut clients, &mut uc, &mut turns, &mut owned);
        assert!(clients.is_empty(), "the park was answered and retired");
        let verdict = queue.try_recv().unwrap();
        assert!(verdict.contains("user_command"), "{verdict}");
        assert!(
            uc.settling(crate::cli::recovery::unix_now())
                .contains(&claude()),
            "the released agent settles before it is looked at as idle"
        );
        round(&mut clients, &mut uc, &mut turns, &mut owned);
        assert!(owned.departing.is_empty(), "no attempt while it settles");
        let record = w.store().read(&tag, &claude(), &id).unwrap().unwrap();
        assert_eq!(record.state, State::WaitingForIdle);
        assert_eq!(
            record.reason.as_deref(),
            Some("its released turn is ending")
        );
    }

    /// The loop's look: a Stop of the bound session held here is a park; one
    /// of another session is not; a settling release and an agent recovery
    /// does not follow yet both wait.
    #[test]
    fn the_loops_look_at_an_agent() {
        use crate::cli::user_command::dispatch::Look;
        let w = world();
        let turns = crate::cli::turn_custody::Turns::new(w.repo.clone());
        let mut recovery =
            crate::cli::recovery::Driver::new(w.repo.clone(), w.home.path().to_path_buf());
        let none = Default::default();
        let look = |clients: &[Client], recovery: &mut _, settling| {
            user_command_look(
                &w.repo,
                clients,
                &turns,
                recovery,
                settling,
                &claude(),
                &sid(),
            )
        };
        let (mine, _q) = holding("session-one");
        assert_eq!(look(&[mine], &mut recovery, &none), Look::Parked);
        let (other, _q) = holding("session-two");
        assert!(matches!(
            look(&[other], &mut recovery, &none),
            Look::Wait(_)
        ));
        let settling = [claude()].into_iter().collect();
        assert_eq!(
            look(&[], &mut recovery, &settling),
            Look::Wait("its released turn is ending".into())
        );
        assert!(matches!(look(&[], &mut recovery, &none), Look::Wait(_)));
    }
}
