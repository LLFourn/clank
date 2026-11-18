use std::time::Duration;

use super::published::{Observation, describe, forget_dead_observation, note_observation, publish};
use super::runtime::{runtime_dir, runtime_path_in};
use super::socket::{bind_listener, ensure_dir, socket_dirs};
use super::wire::{DescribedBuild, Descriptor};

/// Whether a bounded retirement is under way, as the watchdog sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retiring {
    No,
    Within,
    /// Past its deadline and the margin after it.
    Overdue,
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
    let me = rustix::process::getuid().as_raw();
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
/// CLIENT is what copes, exactly as it does when no socket directory is
/// usable: a waiter tries again and then says it has no monitor, and a
/// status panel watches the repo itself.
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
    acquire_lease_taking(dir, path, me, between, clank_user_config::lease::take)
}

/// [`acquire_lease`] in one attempt, for a caller polling it to its own
/// deadline: a patient take would park the runtime thread polling it.
pub(super) fn acquire_lease_now(repo: &std::path::Path) -> Result<MonitorLease, LeaseError> {
    let me = rustix::process::getuid().as_raw();
    let dir = runtime_dir(me);
    let path = runtime_path_in(&dir, repo, "lock");
    acquire_lease_taking(&dir, path, me, |_| {}, clank_user_config::lease::take_now)
}

fn acquire_lease_taking(
    dir: &std::path::Path,
    path: std::path::PathBuf,
    me: u32,
    between: impl Fn(&std::path::Path),
    take: fn(&std::fs::File) -> std::io::Result<bool>,
) -> Result<MonitorLease, LeaseError> {
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
        match take(&file) {
            Ok(true) => {}
            Ok(false) => {
                return Err(LeaseError::Busy(format!(
                    "another monitor already owns this repo: its lease at {} is held",
                    path.display()
                )));
            }
            Err(e) => {
                return Err(LeaseError::Unverifiable(format!(
                    "cannot lock the monitor lease at {}: {e}",
                    path.display()
                )));
            }
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

pub(super) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(super) fn timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "an unknown time".into())
}

/// What the loop and the watchdog share about the lease. The watchdog
/// only looks, and wakes the loop when what it saw changed; the loop
/// alone acts, after its own look.
pub(crate) struct LeaseWatch {
    pub(super) path: std::path::PathBuf,
    held: std::sync::Mutex<FileId>,
    pub(super) wake: tokio::sync::Notify,
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

    pub(super) fn held(&self) -> FileId {
        *self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The watchdog's look: read-only. Wakes the loop when the path no
    /// longer holds the file locked; it is the loop's look that decides.
    pub(super) fn observe(&self) {
        if found_at(&self.path, self.held()) != Found::Held {
            self.wake.notify_one();
        }
    }

    pub(super) fn begin_retiring(&self, within: Duration) {
        self.retiring_until.store(
            unix_ms() + within.as_millis() as u64,
            std::sync::atomic::Ordering::Release,
        );
    }

    /// How long the retirement under way has left, its margin included.
    pub(super) fn retiring_left(&self, now_ms: u64, margin: Duration) -> Option<Duration> {
        match self
            .retiring_until
            .load(std::sync::atomic::Ordering::Acquire)
        {
            0 => None,
            until => Some(Duration::from_millis(
                (until + margin.as_millis() as u64).saturating_sub(now_ms),
            )),
        }
    }

    pub(super) fn retiring(&self, now_ms: u64, margin: Duration) -> Retiring {
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

/// A monitor's view of its own build against the installation: the
/// authority every writer in this process answers to, looked at from the
/// loop.
pub(crate) struct Built {
    authority: crate::upgrade::Authority,
    installed_at: std::path::PathBuf,
    running: crate::build_stamp::Stamp,
    shown: Option<crate::build_stamp::Install>,
    /// Records a changed state where status reads it.
    note: Box<dyn Fn(&crate::build_stamp::Install) + Send>,
}

impl Built {
    /// `authority` watches the installation at `installed_at`.
    pub(crate) fn new(
        authority: crate::upgrade::Authority,
        installed_at: std::path::PathBuf,
        running: crate::build_stamp::Stamp,
        note: Box<dyn Fn(&crate::build_stamp::Install) + Send>,
    ) -> Self {
        Built {
            authority,
            installed_at,
            running,
            shown: None,
            note,
        }
    }

    /// Look at the installation, noting and logging only a change.
    fn look(&mut self) -> crate::upgrade::Standing {
        let standing = self.authority.look();
        if let Some(install) = self.authority.seen()
            && self.shown.as_ref() != Some(&install)
        {
            eprintln!(
                "monitor: installation {} at {}: {install:?} (running {})",
                self.installed_at.display(),
                timestamp(),
                self.running
            );
            (self.note)(&install);
            self.shown = Some(install);
        }
        standing
    }
}

/// How long a monitor that has seen a newer clank installed goes on
/// hearing the results of presentations it had already handed out: the
/// bound their hooks answer within.
pub(super) const QUIESCE_BUDGET: Duration =
    Duration::from_millis(clank_core::turn_policy::DELIVERY_RESULT_WITHIN);

/// The loop's hold on the repo, and the only code that re-takes the
/// lease, publishes for it or gives it up.
pub(crate) struct Tenure {
    lease: MonitorLease,
    pub(super) watch: std::sync::Arc<LeaseWatch>,
    retiring: bool,
    build: Option<Built>,
    /// Set when a stale build began retiring: until then it stays to
    /// settle what is in flight.
    pub(super) quiesce_by: Option<std::time::Instant>,
}

impl Tenure {
    pub(crate) fn new(lease: MonitorLease) -> Self {
        let watch = LeaseWatch::new(&lease);
        Tenure {
            lease,
            watch,
            retiring: false,
            build: None,
            quiesce_by: None,
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
            && let Some(installed) = build.look().stale()
        {
            self.retiring = true;
            self.quiesce_by = Some(std::time::Instant::now() + QUIESCE_BUDGET);
            return Settled::Stale {
                running: build.running,
                installed: installed.stamp,
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

    /// Whether the loop gives the repo up now. A lease lost to another
    /// owner goes at once. A stale build stays for the work it has in
    /// flight, a presentation handed out before the install was seen,
    /// whose result and receipts would be lost with this monitor, and for
    /// no longer than its hook has to answer.
    pub(crate) fn standing_down(&self, now: std::time::Instant) -> bool {
        let quiescing = self.quiesce_by.is_some_and(|by| now < by)
            && self.build.as_ref().is_some_and(|build| {
                matches!(
                    build.authority.standing(),
                    crate::upgrade::Standing::Quiescing { .. }
                )
            });
        self.retiring && !quiescing
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

/// The loop's hold on the repo: its [`Tenure`], the socket it serves on,
/// the descriptor it republishes on re-taking the lease, and the watch on
/// the installation that can end it.
pub(super) struct Hold {
    listener: tokio::net::UnixListener,
    _installs: Option<crate::upgrade::Subscription>,
    installs_lost: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    installs: std::sync::Arc<tokio::sync::Notify>,
    advertised: Advertised,
    tenure: Tenure,
}

/// What this monitor publishes as the repo's descriptor.
struct Advertised {
    repo: std::sync::Arc<std::path::PathBuf>,
    pid: i32,
    socket: std::path::PathBuf,
    described: Option<DescribedBuild>,
}

impl Advertised {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            build: self.described.clone(),
            ..describe(&self.repo, self.pid, &self.socket)
        }
    }

    fn republish(&self) -> Result<(), String> {
        publish(&self.repo, &self.descriptor()).map_err(|e| e.to_string())
    }
}

impl Hold {
    /// Take `repo`: its lease, a socket to serve on, and the descriptor
    /// published. `None` when its installation already holds another
    /// build.
    pub(super) fn take(
        repo: &std::sync::Arc<std::path::PathBuf>,
        args: &crate::cli::MonitorStartArgs,
    ) -> anyhow::Result<Option<Hold>> {
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
        let authority = crate::upgrade::process();
        if let Some(installed) = &installed {
            authority.watch(
                crate::build_stamp::Installation::at(installed.path.clone()),
                running,
            );
            if let Some(other) = authority.look().stale() {
                eprintln!(
                    "monitor: {} holds build {}, not this one ({running}); not starting",
                    installed.path.display(),
                    other.stamp
                );
                return Ok(None);
            }
        }

        // The lease first: everything after it assumes no other owner,
        // including clearing a socket a dead one left behind.
        let lease = acquire_lease(repo).map_err(|e| anyhow::anyhow!("{e}"))?;
        let me = rustix::process::getuid().as_raw();
        let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
        let pid = std::process::id() as i32;
        let (listener, socket) = bind_listener(&socket_dirs(me, xdg.as_deref()), pid, me, &lease)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        forget_dead_observation(repo);
        let described = installed.as_ref().map(|r| DescribedBuild {
            running: running.to_string(),
            version: Some(crate::build_stamp::VERSION.to_string()),
            installed_at: r.path.to_string_lossy().into_owned(),
            source: r.source,
        });
        let mut tenure = Tenure::new(lease);
        if let Some(installed) = installed.as_ref() {
            let (repo, birth) = (repo.clone(), crate::proc_identity::token_for(pid));
            let (installed_at, source) = (
                installed.path.to_string_lossy().into_owned(),
                installed.source,
            );
            tenure = tenure.with_build(Built::new(
                authority.clone(),
                installed.path.clone(),
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
        let advertised = Advertised {
            repo: repo.clone(),
            pid,
            socket,
            described,
        };
        let installs = std::sync::Arc::new(tokio::sync::Notify::new());
        let installs_lost = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        // A monitor that cannot hear an install would serve on, stale, past
        // the build that should replace it: it ends instead, saying why.
        let subscribed = authority
            .subscribe(
                {
                    let installs = installs.clone();
                    move || installs.notify_one()
                },
                {
                    let (installs, lost) = (installs.clone(), installs_lost.clone());
                    move |why| {
                        lost.lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert(why);
                        installs.notify_one();
                    }
                },
            )
            .map_err(|why| anyhow::anyhow!("cannot watch the installation: {why}"))?;
        publish(repo, &advertised.descriptor())?;
        println!(
            "monitor for {} listening on {}",
            repo.display(),
            advertised.socket.display()
        );
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        Ok(Some(Hold {
            listener,
            _installs: subscribed,
            installs_lost,
            installs,
            advertised,
            tenure,
        }))
    }

    /// The next connection to this monitor's socket.
    pub(super) async fn accept(&self) -> std::io::Result<tokio::net::UnixStream> {
        Ok(self.listener.accept().await?.0)
    }

    pub(super) fn pid(&self) -> i32 {
        self.advertised.pid
    }

    pub(super) fn watch(&self) -> &std::sync::Arc<LeaseWatch> {
        &self.tenure.watch
    }

    /// Told when the watchdog saw the lease's path change.
    pub(super) fn lease_changed(&self) -> &tokio::sync::Notify {
        &self.tenure.watch.wake
    }

    /// Told when where this build stands against its installation may
    /// have moved, or the watch on it failed.
    pub(super) fn installs(&self) -> &tokio::sync::Notify {
        &self.installs
    }

    /// An error once the watch on the installation has failed.
    pub(super) fn installed(&self) -> anyhow::Result<()> {
        if let Some(why) = self
            .installs_lost
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            anyhow::bail!("the watch on the installation failed: {why}");
        }
        Ok(())
    }

    /// [`Tenure::may_act`], republishing the descriptor on a re-take.
    pub(super) fn may_act(&mut self) -> bool {
        let advertised = &self.advertised;
        self.tenure.may_act(&|| advertised.republish())
    }

    pub(super) fn standing_down(&self) -> bool {
        self.tenure.standing_down(std::time::Instant::now())
    }

    /// Tell the watchdog a retirement is under way, to be done `within`.
    pub(super) fn begin_retiring(&self, within: Duration) {
        self.tenure.watch.begin_retiring(within);
    }

    /// When a stale build that stayed to settle what is in flight goes.
    pub(super) fn quiesced(&self) -> Option<tokio::time::Instant> {
        self.tenure.quiesce_by.map(tokio::time::Instant::from_std)
    }
}

#[cfg(test)]
#[path = "lease_tests.rs"]
mod lease_tests;

#[cfg(test)]
#[path = "build_tests.rs"]
mod build_tests;
