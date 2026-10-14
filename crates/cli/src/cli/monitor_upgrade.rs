//! `clank monitor upgrade`: move every reachable monitor older than this
//! build onto it, once.
//!
//! Monitors from this build on retire by themselves when a different
//! build is installed. Older ones cannot, and a repo where nobody is
//! active never sends one the newer client that would ask it to go. This
//! asks each of them, in the socket namespaces this invocation vets, and
//! starts a successor for each repo whose monitor agreed it is obsolete.
//!
//! A socket's repo is taken from its own process's command line, never
//! from a descriptor: a descriptor is a hint that can be stale or name a
//! reused pid, and the migration acts only on what it verified.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::monitor::{Seen, Who};

/// How many probes or starts run at once.
pub(crate) const AT_ONCE: usize = 8;
/// The whole command's deadline: discovery, every probe and every start.
pub(crate) const WITHIN: Duration = Duration::from_secs(30);

/// What became of one monitor socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// An older monitor of its repo, which answered that it is obsolete.
    Asked,
    /// An older monitor that refused for another reason: nothing follows.
    Refused(String),
    /// Already this build's protocol: left alone.
    Current,
    /// Newer than this build: left alone.
    Newer,
    /// Nothing answered there.
    Unreachable(String),
    /// It answered, but nothing was established.
    Unproven(String),
    /// Which repo it serves could not be verified, so it was not greeted.
    Skipped(String),
    /// The deadline came first.
    TimedOut,
}

/// What became of a successor for a repo whose monitor was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Successor {
    /// This command started one, and attached to it.
    Spawned,
    /// A compatible monitor was already there, and this command attached
    /// to it without starting one.
    Reused,
    /// No compatible monitor could be attached to; `spawned` says
    /// whether one was started anyway.
    Unestablished { spawned: bool, why: String },
    /// Still running at the deadline. `spawned` is what was known then:
    /// whether a monitor process had been created, or that nobody could
    /// yet say.
    InFlight { spawned: Created },
    /// The deadline came before it was begun: nothing was started.
    TimedOut,
}

/// Whether a start still running at the deadline had created a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Created {
    /// It had, and was still waiting to attach to it.
    Yes,
    /// It had not been seen to; it may yet.
    Unknown,
}

/// One monitor socket and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Swept {
    pub(crate) socket: PathBuf,
    pub(crate) repo: Option<PathBuf>,
    pub(crate) outcome: Outcome,
    /// How its old monitor was ended, when it was asked to go.
    pub(crate) retired: Option<Retirement>,
}

/// How an asked old monitor was ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retirement {
    /// It exited after TERM.
    Exited,
    /// It ignored TERM for [`TERM_GRACE`], and was killed.
    Killed,
    /// It was already gone, so nothing was sent.
    Gone,
    /// Still running at the deadline, or not confirmed gone by then.
    StillRunning,
    /// Its process could not be looked at, so nothing was sent.
    Unverified,
}

/// How long an old monitor is given to exit on TERM before KILL: longer
/// than a lease-loss drain (`monitor::DRAIN_GRACE`, 10 s) that TERM may
/// join, so a cooperative drain is not mistaken for a wedged one. A KILL
/// still cuts off whatever producer work is left; it proves nothing was
/// drained.
pub(crate) const TERM_GRACE: Duration = Duration::from_secs(12);

/// A monitor process as the upgrade verified it: the repo it serves, and
/// the birth that is the only proof a later look at its pid is still it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) repo: PathBuf,
    pub(crate) birth: crate::proc_identity::ProcToken,
}

/// What one look at an asked monitor's pid found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Still {
    /// The verified process, by its birth.
    Ours,
    /// Exited: nothing there, or another process with another birth.
    Gone,
    /// The look established nothing.
    Unknown,
}

/// Everything the command did.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Whether discovery finished before the deadline. When it did not,
    /// nothing was greeted or started.
    pub(crate) discovered: bool,
    /// Each socket namespace, and why it was not swept when it was not.
    pub(crate) namespaces: Vec<(PathBuf, Result<(), String>)>,
    pub(crate) swept: Vec<Swept>,
    pub(crate) successors: Vec<(PathBuf, Successor)>,
}

type RepoOf = Box<dyn Fn(i32) -> Result<Verified, String> + Send + Sync>;
type Probe = Box<dyn Fn(&Path, &Path, Instant) -> Outcome + Send + Sync>;
/// Start a successor for a repo before the deadline, calling the third
/// argument the moment a monitor process has been created.
type Retire = Box<dyn Fn(i32, &Verified, Instant) -> Retirement + Send + Sync>;
type Start = Box<dyn Fn(&Path, Instant, &(dyn Fn() + Sync)) -> Successor + Send + Sync>;

/// The effects the migration has on the world, so the whole operation is
/// reachable without a real monitor.
pub(crate) struct Effects {
    /// The repo the monitor process `pid` serves, verified.
    pub(crate) repo_of: RepoOf,
    /// Greet the monitor at a socket for a repo, before the deadline.
    pub(crate) probe: Probe,
    /// Start a successor for a repo, before the deadline.
    pub(crate) start: Start,
    /// End the old monitor process `pid` of a repo, before the deadline.
    pub(crate) retire: Retire,
}

type Namespaces = Vec<(PathBuf, Result<(), String>)>;

/// The monitor sockets in each namespace: only a namespace that vets as
/// this uid's, and only entries that are sockets this uid owns, named by
/// a pid. A namespace not begun by `deadline` says so; one begun is read
/// through, and `drive` stops waiting for it at the deadline.
fn discover(dirs: &[PathBuf], me: u32, deadline: Instant) -> (Namespaces, Vec<(PathBuf, i32)>) {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let mut namespaces = Vec::new();
    let mut found = Vec::new();
    for dir in dirs {
        if Instant::now() >= deadline {
            namespaces.push((dir.clone(), Err("not reached by the deadline".to_string())));
            continue;
        }
        match super::monitor::trusted_dir(dir, me) {
            super::monitor::Trust::Ours => {}
            super::monitor::Trust::Absent => {
                namespaces.push((dir.clone(), Err("absent".to_string())));
                continue;
            }
            super::monitor::Trust::Theirs(why) => {
                namespaces.push((dir.clone(), Err(why)));
                continue;
            }
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                namespaces.push((dir.clone(), Err(e.to_string())));
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(pid) = path
                .extension()
                .filter(|x| *x == "sock")
                .and(path.file_stem())
                .and_then(|stem| stem.to_str())
                .and_then(|stem| stem.parse::<i32>().ok())
            else {
                continue;
            };
            let ours = std::fs::symlink_metadata(&path)
                .is_ok_and(|md| md.file_type().is_socket() && md.uid() == me);
            if ours {
                found.push((path, pid));
            }
        }
        namespaces.push((dir.clone(), Ok(())));
    }
    found.sort();
    (namespaces, found)
}

/// One repo's successor, across the endpoints that may start it.
#[derive(Debug, Clone, Default)]
struct Claim {
    /// The endpoint starting it now.
    starting: Option<usize>,
    /// The endpoint that last started it.
    last: Option<usize>,
    /// Another endpoint's monitor went while a start was running.
    retry: bool,
    /// A successor was created or attached.
    established: bool,
}

/// How far one endpoint's operation got.
#[derive(Debug, Clone, Default)]
struct Progress {
    repo: Option<PathBuf>,
    outcome: Option<Outcome>,
    retired: Option<Retirement>,
    successor: Option<Starting>,
}

#[derive(Debug, Clone)]
enum Starting {
    Begun,
    Created,
    Ended(Successor),
}

/// The whole migration under one deadline. Each endpoint is one
/// operation, on at most [`AT_ONCE`] threads: verify its repo, greet it,
/// and, if it answered that it is obsolete and no other endpoint has
/// already claimed that repo, start its successor. No operation waits for
/// another, and none begins a step past the deadline.
///
/// At the deadline the report is taken from how far each got. Work still
/// running is not joined; whatever it does afterwards changes nothing in
/// the report and, being past the deadline, begins no further step.
pub(crate) fn drive(
    dirs: &[PathBuf],
    me: u32,
    effects: std::sync::Arc<Effects>,
    deadline: Instant,
) -> Report {
    use std::sync::{Arc, Mutex};
    let (sent, found) = std::sync::mpsc::channel();
    let dirs = dirs.to_vec();
    std::thread::spawn(move || {
        let _ = sent.send(discover(&dirs, me, deadline));
    });
    let left = deadline.saturating_duration_since(Instant::now());
    let Ok((namespaces, found)) = found.recv_timeout(left) else {
        return Report::default();
    };

    let progress: Arc<Vec<Mutex<Progress>>> =
        Arc::new(found.iter().map(|_| Mutex::default()).collect());
    let claims: Arc<Mutex<std::collections::BTreeMap<PathBuf, Claim>>> = Arc::default();
    let (ended, ends) = std::sync::mpsc::channel::<()>();
    let operate = {
        let (progress, effects, claims) = (progress.clone(), effects.clone(), claims.clone());
        move |at: usize, socket: PathBuf, pid: i32| {
            let note = |f: &dyn Fn(&mut Progress)| f(&mut progress[at].lock().unwrap());
            let verified = match (effects.repo_of)(pid) {
                Ok(verified) => verified,
                Err(why) => return note(&|p| p.outcome = Some(Outcome::Skipped(why.clone()))),
            };
            let repo = verified.repo.clone();
            note(&|p| p.repo = Some(repo.clone()));
            if Instant::now() >= deadline {
                return;
            }
            let outcome = (effects.probe)(&socket, &repo, deadline);
            note(&|p| p.outcome = Some(outcome.clone()));
            if outcome != Outcome::Asked || Instant::now() >= deadline {
                return;
            }
            // Every asked monitor is ended, a duplicate as much as the one
            // published, so none is left holding a source.
            let retired = (effects.retire)(pid, &verified, deadline);
            note(&|p| p.retired = Some(retired));
            if !matches!(
                retired,
                Retirement::Exited | Retirement::Killed | Retirement::Gone
            ) {
                return;
            }
            loop {
                {
                    let mut claims = claims.lock().unwrap();
                    let claim = claims.entry(repo.clone()).or_default();
                    if claim.established {
                        return;
                    }
                    // Another endpoint is starting it now: if that start
                    // creates nothing, it is tried again, this endpoint's
                    // monitor being gone too.
                    if claim.starting.is_some() {
                        claim.retry = true;
                        return;
                    }
                    if Instant::now() >= deadline {
                        return;
                    }
                    claim.starting = Some(at);
                    claim.last = Some(at);
                    claim.retry = false;
                }
                note(&|p| p.successor = Some(Starting::Begun));
                let created = || note(&|p| p.successor = Some(Starting::Created));
                let started = (effects.start)(&repo, deadline, &created);
                note(&|p| p.successor = Some(Starting::Ended(started.clone())));
                let mut claims = claims.lock().unwrap();
                let claim = claims.entry(repo.clone()).or_default();
                claim.starting = None;
                let uncreated = matches!(
                    started,
                    Successor::Unestablished { spawned: false, .. } | Successor::TimedOut
                );
                if !uncreated {
                    claim.established = true;
                    return;
                }
                if !claim.retry {
                    return;
                }
            }
        }
    };
    let operate = Arc::new(operate);
    let (mut next, mut running) = (0, 0);
    loop {
        while running < AT_ONCE && next < found.len() && Instant::now() < deadline {
            let (socket, pid) = found[next].clone();
            let (at, ended, operate) = (next, ended.clone(), operate.clone());
            std::thread::spawn(move || {
                operate(at, socket, pid);
                let _ = ended.send(());
            });
            next += 1;
            running += 1;
        }
        if running == 0 {
            break;
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        match ends.recv_timeout(left) {
            Ok(()) => running -= 1,
            Err(_) => break,
        }
    }

    let progress: Vec<Progress> = progress.iter().map(|p| p.lock().unwrap().clone()).collect();
    let claims = claims.lock().unwrap().clone();
    let swept: Vec<Swept> = found
        .iter()
        .zip(&progress)
        .map(|((socket, _), p)| Swept {
            socket: socket.clone(),
            repo: p.repo.clone(),
            outcome: p.outcome.clone().unwrap_or(Outcome::TimedOut),
            retired: p.retired,
        })
        .collect();
    let mut asked: Vec<PathBuf> = swept
        .iter()
        .filter(|s| s.outcome == Outcome::Asked)
        .filter_map(|s| s.repo.clone())
        .collect();
    asked.sort();
    asked.dedup();
    let successors = asked
        .into_iter()
        .map(|repo| {
            let claimant = claims.get(&repo).and_then(|c| c.starting.or(c.last));
            let successor = match claimant.map(|at| &progress[at].successor) {
                Some(Some(Starting::Ended(successor))) => successor.clone(),
                Some(Some(Starting::Created)) => Successor::InFlight {
                    spawned: Created::Yes,
                },
                Some(Some(Starting::Begun)) | Some(None) => Successor::InFlight {
                    spawned: Created::Unknown,
                },
                None if swept.iter().any(|s| {
                    s.repo.as_ref() == Some(&repo)
                        && matches!(
                            s.retired,
                            Some(Retirement::StillRunning | Retirement::Unverified)
                        )
                }) =>
                {
                    Successor::Unestablished {
                        spawned: false,
                        why: "an old monitor of it was not confirmed gone".into(),
                    }
                }
                None => Successor::TimedOut,
            };
            (repo, successor)
        })
        .collect();
    Report {
        discovered: true,
        namespaces,
        swept,
        successors,
    }
}

fn classify(seen: Seen) -> Outcome {
    match seen {
        Seen::OlderPeer { obsolete: true, .. } => Outcome::Asked,
        Seen::OlderPeer { why, .. } => Outcome::Refused(why),
        Seen::Shared(_) => Outcome::Current,
        Seen::Newer(_) => Outcome::Newer,
        Seen::Theirs(why) | Seen::Unproven(why) => Outcome::Unproven(why),
        Seen::Vacant(why) => Outcome::Unreachable(why),
    }
}

/// Greet the monitor at `socket` as an observer of `repo`.
fn probe(socket: &Path, repo: &Path, deadline: Instant) -> Outcome {
    classify(super::monitor::greet_at(
        &socket.to_string_lossy(),
        repo,
        Who::Observer,
        deadline,
        super::monitor::connect_within,
    ))
}

/// The repo a monitor process serves: a process this uid may signal,
/// identified by its birth across the read, whose command line is
/// `clank monitor start --repo <path>`.
fn repo_of_monitor(pid: i32, me: u32) -> Result<Verified, String> {
    use crate::proc_identity::inspect::{Inspection, inspect};
    let token = crate::proc_identity::token_for(pid)
        .ok_or(format!("process {pid} has no birth to check"))?;
    // Read between the birth taken above and the one `inspect` checks
    // after its reads, so it is this process's.
    match crate::proc_identity::uid_of(pid) {
        Some(uid) if uid == me => {}
        Some(uid) => return Err(format!("process {pid} runs as uid {uid}, not this user")),
        None => return Err(format!("process {pid}'s user could not be read")),
    }
    let Inspection::Verified(details) = inspect(pid, &token) else {
        return Err(format!("process {pid} could not be inspected"));
    };
    let argv = details
        .argv
        .map_err(|_| format!("process {pid}'s command line could not be read"))?;
    let repo = repo_in_argv(&argv).ok_or(format!("process {pid} is not a clank monitor"))?;
    Ok(Verified { repo, birth: token })
}

/// The repo in a `clank monitor start --repo <path>` command line, and
/// only in one: the program is clank, and the subcommand follows it.
fn repo_in_argv(argv: &[std::ffi::OsString]) -> Option<PathBuf> {
    let program = Path::new(argv.first()?).file_name()?;
    if program != "clank" || argv.get(1)? != "monitor" || argv.get(2)? != "start" {
        return None;
    }
    let at = argv.iter().position(|a| a == "--repo")?;
    argv.get(at + 1).map(PathBuf::from)
}

type Spawn = fn(&Path) -> std::io::Result<Box<dyn super::monitor::Started>>;

/// End the verified old monitor `pid`: TERM, then KILL if it has not
/// exited within [`TERM_GRACE`], each sent only while the pid still has
/// the birth it was verified with, and never past the deadline.
fn retire(pid: i32, verified: &Verified, deadline: Instant) -> Retirement {
    retire_with(
        || {
            observe_with(
                || crate::proc_identity::token_for(pid),
                || crate::signal::probe_process(pid),
                &verified.birth,
            )
        },
        |sig| {
            if let Some(pid) = crate::signal::Pid::new(pid) {
                crate::signal::signal_process(pid, sig);
            }
        },
        TERM_GRACE,
        deadline,
    )
}

/// One look at a pid against the `birth` it was verified with. Another
/// birth there means ours exited and the pid was reused; nothing there
/// means it exited; a look that establishes neither proves nothing.
fn observe_with(
    token: impl Fn() -> Option<crate::proc_identity::ProcToken>,
    probe: impl Fn() -> crate::signal::Probe,
    birth: &crate::proc_identity::ProcToken,
) -> Still {
    match token() {
        Some(now) if &now == birth => Still::Ours,
        Some(_) => Still::Gone,
        None if probe() == crate::signal::Probe::Gone => Still::Gone,
        None => Still::Unknown,
    }
}

/// [`retire`] with the look and the signal supplied, so no test sends a
/// real one. The deadline is checked after every look and immediately
/// before each signal, so no signal is sent past it.
fn retire_with(
    observe: impl Fn() -> Still,
    signal: impl Fn(libc::c_int),
    grace: Duration,
    deadline: Instant,
) -> Retirement {
    let gone_by = |by: Instant| {
        loop {
            if observe() == Still::Gone {
                return true;
            }
            let now = Instant::now();
            if now >= by {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50).min(by - now));
        }
    };
    match observe() {
        Still::Gone => return Retirement::Gone,
        Still::Unknown => return Retirement::Unverified,
        Still::Ours => {}
    }
    if Instant::now() >= deadline {
        return Retirement::StillRunning;
    }
    signal(libc::SIGTERM);
    if gone_by((Instant::now() + grace).min(deadline)) {
        return Retirement::Exited;
    }
    match observe() {
        Still::Gone => return Retirement::Exited,
        Still::Unknown => return Retirement::StillRunning,
        Still::Ours => {}
    }
    if Instant::now() >= deadline {
        return Retirement::StillRunning;
    }
    signal(libc::SIGKILL);
    if gone_by(deadline) {
        Retirement::Killed
    } else {
        Retirement::StillRunning
    }
}

/// Start `repo`'s monitor through the ordinary attach-or-spawn, inside
/// the one deadline the command holds, and say what it established.
fn start(repo: &Path, deadline: Instant, created: &(dyn Fn() + Sync)) -> Successor {
    start_with(
        repo,
        deadline,
        super::monitor::spawn_monitor,
        super::monitor::connect_within,
        |repo| super::monitor::acquire_lease(repo).is_ok(),
        created,
    )
}

/// [`start`] with process creation, connection and the lease probe
/// supplied. Only a spawn that returned a process counts as one started.
fn start_with(
    repo: &Path,
    deadline: Instant,
    spawn: Spawn,
    connect: fn(&Path, Instant) -> std::io::Result<std::os::unix::net::UnixStream>,
    took_it: impl Fn(&Path) -> bool,
    created: &(dyn Fn() + Sync),
) -> Successor {
    if Instant::now() >= deadline {
        return Successor::TimedOut;
    }
    let spawned = std::sync::atomic::AtomicBool::new(false);
    let attached = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
        .map(|runtime| {
            runtime.block_on(super::monitor::attach_or_spawn_until(
                repo,
                Who::Observer,
                deadline,
                super::monitor::ATTACH_BUDGET,
                super::monitor::SPAWN_SETTLE / super::monitor::SHARE_ATTEMPTS,
                super::monitor::SHARE_ATTEMPTS,
                |repo| {
                    let started = spawn(repo);
                    if started.is_ok() {
                        spawned.store(true, std::sync::atomic::Ordering::Release);
                        created();
                    }
                    started
                },
                connect,
                took_it,
            ))
        });
    let spawned = spawned.load(std::sync::atomic::Ordering::Acquire);
    match attached {
        Ok(super::monitor::Attached::Shared(_)) if spawned => Successor::Spawned,
        Ok(super::monitor::Attached::Shared(_)) => Successor::Reused,
        Ok(super::monitor::Attached::Alone(why)) => Successor::Unestablished { spawned, why },
        Err(why) => Successor::Unestablished {
            spawned,
            why: format!("no runtime to start it on: {why}"),
        },
    }
}

/// [`drive`], awaited without needing the runtime's blocking pool.
pub(crate) async fn upgrade_with(
    dirs: Vec<PathBuf>,
    me: u32,
    effects: Effects,
    deadline: Instant,
) -> Report {
    let (sent, report) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sent.send(drive(&dirs, me, std::sync::Arc::new(effects), deadline));
    });
    report.await.unwrap_or_default()
}

/// `clank monitor upgrade`.
pub async fn run() -> anyhow::Result<()> {
    let me = unsafe { libc::getuid() };
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let report = upgrade_with(
        super::monitor::socket_dirs(me, xdg.as_deref()),
        me,
        Effects {
            repo_of: Box::new(move |pid| repo_of_monitor(pid, me)),
            probe: Box::new(probe),
            start: Box::new(start),
            retire: Box::new(retire),
        },
        Instant::now() + WITHIN,
    )
    .await;
    if !report.discovered {
        println!("discovery did not finish by the deadline; nothing was greeted or started");
    }
    for (dir, swept) in &report.namespaces {
        match swept {
            Ok(()) => println!("swept {}", dir.display()),
            Err(why) => println!("not swept {}: {why}", dir.display()),
        }
    }
    for s in &report.swept {
        let repo = s
            .repo
            .as_ref()
            .map(|r| r.display().to_string())
            .unwrap_or_else(|| "?".into());
        match s.retired {
            Some(retired) => println!(
                "{} ({repo}): {:?}, {retired:?}",
                s.socket.display(),
                s.outcome
            ),
            None => println!("{} ({repo}): {:?}", s.socket.display(), s.outcome),
        }
    }
    for (repo, successor) in &report.successors {
        println!("{}: {successor:?}", repo.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn me() -> u32 {
        unsafe { libc::getuid() }
    }

    /// A vetted socket namespace holding listening sockets for `pids`.
    fn namespace(
        pids: impl IntoIterator<Item = i32>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        Vec<std::os::unix::net::UnixListener>,
    ) {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("socks");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let listeners = pids
            .into_iter()
            .map(|pid| {
                std::os::unix::net::UnixListener::bind(dir.join(format!("{pid}.sock"))).unwrap()
            })
            .collect();
        (home, dir, listeners)
    }

    fn birth(n: i32) -> crate::proc_identity::ProcToken {
        crate::proc_identity::ProcToken::MacStart {
            sec: n as u64,
            usec: 0,
        }
    }

    fn verified(repo: PathBuf, pid: i32) -> Verified {
        Verified {
            repo,
            birth: birth(pid),
        }
    }

    fn repo_for(pid: i32) -> PathBuf {
        PathBuf::from(format!("/w/{pid}"))
    }

    type Started = std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>;

    fn effects(
        probe: impl Fn(&Path, &Path, Instant) -> Outcome + Send + Sync + 'static,
        started: Started,
    ) -> std::sync::Arc<Effects> {
        std::sync::Arc::new(effects_of(probe, started))
    }

    fn effects_of(
        probe: impl Fn(&Path, &Path, Instant) -> Outcome + Send + Sync + 'static,
        started: Started,
    ) -> Effects {
        Effects {
            repo_of: Box::new(|pid| Ok(verified(repo_for(pid), pid))),
            probe: Box::new(probe),
            start: Box::new(move |repo, _, _| {
                started.lock().unwrap().push(repo.to_path_buf());
                Successor::Spawned
            }),
            retire: Box::new(|_, _, _| Retirement::Exited),
        }
    }

    fn pid_of(socket: &Path) -> i32 {
        socket
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// The whole operation: a successor only for a monitor that agreed it
    /// is obsolete, none for a refusal or a current, newer or unproven
    /// one, and every socket reported.
    #[test]
    fn only_an_obsolete_monitor_gets_a_successor() {
        let (_home, dir, _listeners) = namespace([1, 2, 3, 4, 5, 6]);
        let started = Started::default();
        let report = drive(
            std::slice::from_ref(&dir),
            me(),
            effects(
                |socket, _, _| match pid_of(socket) {
                    1 => Outcome::Asked,
                    2 => Outcome::Refused("this monitor serves /w/other".into()),
                    3 => Outcome::Current,
                    4 => Outcome::Newer,
                    5 => Outcome::Unproven("garbled".into()),
                    _ => Outcome::Unreachable("refused".into()),
                },
                started.clone(),
            ),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(*started.lock().unwrap(), vec![repo_for(1)]);
        assert_eq!(report.successors, vec![(repo_for(1), Successor::Spawned)]);
        assert_eq!(report.swept.len(), 6);
        assert_eq!(report.namespaces, vec![(dir, Ok(()))]);
    }

    /// Nothing is started once the deadline has passed, however the
    /// probes went.
    #[test]
    fn nothing_starts_after_the_deadline() {
        let (_home, dir, _listeners) = namespace([1]);
        let started = Started::default();
        let within = Duration::from_millis(100);
        let report = drive(
            &[dir],
            me(),
            effects(
                move |_, _, _| {
                    std::thread::sleep(within * 2);
                    Outcome::Asked
                },
                started.clone(),
            ),
            Instant::now() + within,
        );
        std::thread::sleep(within * 2);
        assert!(
            started.lock().unwrap().is_empty(),
            "the late answer started one"
        );
        assert!(report.successors.is_empty());
        assert_eq!(
            report.swept[0].outcome,
            Outcome::TimedOut,
            "an answer after the deadline counted"
        );
    }

    /// A namespace that does not vet as this uid's is reported and never
    /// read; a non-socket entry is not a monitor.
    #[test]
    fn only_vetted_namespaces_and_real_sockets_are_swept() {
        use std::os::unix::fs::PermissionsExt;
        let (_home, open, _listeners) = namespace([7]);
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let (_home2, ours, _listeners2) = namespace([]);
        std::fs::write(ours.join("8.sock"), b"not a socket").unwrap();
        let probed = std::sync::Arc::new(AtomicUsize::new(0));
        let count = probed.clone();
        let report = drive(
            &[open.clone(), ours.clone()],
            me(),
            effects(
                move |_, _, _| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Outcome::Asked
                },
                Default::default(),
            ),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(probed.load(Ordering::SeqCst), 0);
        assert!(report.swept.is_empty());
        assert!(matches!(&report.namespaces[0], (d, Err(_)) if *d == open));
        assert_eq!(report.namespaces[1], (ours, Ok(())));
    }

    /// At most [`AT_ONCE`] probes are ever running together, and the
    /// whole sweep ends by its deadline, slow discovery included.
    #[test]
    fn probes_run_at_most_eight_at_once_and_end_by_the_deadline() {
        let (_home, dir, _listeners) = namespace(100..120);
        let active = std::sync::Arc::new(AtomicUsize::new(0));
        let most = std::sync::Arc::new(AtomicUsize::new(0));
        let (a, m) = (active.clone(), most.clone());
        let within = Duration::from_millis(700);
        let started = Instant::now();
        let mut effects = effects_of(
            move |_, _, _| {
                let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
                a.fetch_sub(1, Ordering::SeqCst);
                Outcome::Current
            },
            Default::default(),
        );
        effects.repo_of = Box::new(|pid| {
            std::thread::sleep(Duration::from_millis(75));
            Ok(verified(repo_for(pid), pid))
        });
        let report = drive(&[dir], me(), std::sync::Arc::new(effects), started + within);
        assert_eq!(most.load(Ordering::SeqCst), AT_ONCE);
        assert!(
            started.elapsed() < within + Duration::from_millis(300),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(report.swept.len(), 20);
    }

    /// A lookup still running at the deadline does not hold the command:
    /// the report comes at the deadline, that socket unfinished, and its
    /// late result greets nothing.
    #[test]
    fn a_lookup_still_running_at_the_deadline_is_left_unfinished() {
        let (_home, dir, _listeners) = namespace([300]);
        let probed = std::sync::Arc::new(AtomicUsize::new(0));
        let count = probed.clone();
        let mut effects = effects_of(
            move |_, _, _| {
                count.fetch_add(1, Ordering::SeqCst);
                Outcome::Asked
            },
            Default::default(),
        );
        effects.repo_of = Box::new(|pid| {
            std::thread::sleep(Duration::from_millis(500));
            Ok(verified(repo_for(pid), pid))
        });
        let started = Instant::now();
        let within = Duration::from_millis(30);
        let report = drive(&[dir], me(), std::sync::Arc::new(effects), started + within);
        assert!(
            started.elapsed() < within + Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(report.swept.len(), 1);
        assert_eq!(report.swept[0].outcome, Outcome::TimedOut);
        assert!(report.successors.is_empty());
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(
            probed.load(Ordering::SeqCst),
            0,
            "the late lookup went on to greet"
        );
    }

    /// Discovery that has not begun by the deadline reaches no namespace
    /// and greets nothing.
    #[test]
    fn nothing_is_discovered_after_the_deadline() {
        let (_home, dir, _listeners) = namespace([301]);
        let probed = std::sync::Arc::new(AtomicUsize::new(0));
        let count = probed.clone();
        let report = drive(
            &[dir],
            me(),
            effects(
                move |_, _, _| {
                    count.fetch_add(1, Ordering::SeqCst);
                    Outcome::Asked
                },
                Default::default(),
            ),
            Instant::now() - Duration::from_millis(1),
        );
        assert!(!report.discovered || report.swept.is_empty());
        assert!(report.swept.is_empty() && report.successors.is_empty());
        assert_eq!(probed.load(Ordering::SeqCst), 0);
        let (_, swept) = discover(
            std::slice::from_ref(&namespace([302]).1),
            me(),
            Instant::now() - Duration::from_millis(1),
        );
        assert!(swept.is_empty(), "enumeration past the deadline");
    }

    /// The command needs nothing from a full blocking pool.
    #[test]
    fn a_full_blocking_pool_cannot_hold_the_command() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_home, dir, _listeners) = namespace([401]);
            let (release, hold) = std::sync::mpsc::channel::<()>();
            let (ready, started) = std::sync::mpsc::channel();
            let occupant = tokio::task::spawn_blocking(move || {
                ready.send(()).unwrap();
                let _ = hold.recv();
            });
            started.recv_timeout(Duration::from_secs(1)).unwrap();
            let task = tokio::spawn(upgrade_with(
                vec![dir],
                me(),
                effects_of(|_, _, _| Outcome::Current, Default::default()),
                Instant::now() + Duration::from_millis(30),
            ));
            tokio::time::sleep(Duration::from_millis(200)).await;
            let finished = task.is_finished();
            release.send(()).unwrap();
            occupant.await.unwrap();
            let _ = task.await.unwrap();
            assert!(finished, "the command waited on the blocking pool");
        });
    }

    /// A legacy monitor's answer, byte for byte: only the obsolete one
    /// asks it to go; a refusal for another repo does not.
    /// What a proto-12 monitor of the repo answers this build's client,
    /// byte for byte in its words.
    fn obsolete_from_12() -> String {
        format!(
            "{{\"t\":\"decline\",\"proto\":12,\"why\":\"this monitor speaks proto 12, the client speaks {}\"}}\n",
            super::super::monitor::PROTO
        )
    }

    #[test]
    fn only_the_exact_obsolete_answer_counts_as_asked() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("501.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                std::io::BufReader::new(&stream)
                    .read_line(&mut line)
                    .unwrap();
                let hello: serde_json::Value = serde_json::from_str(&line).unwrap();
                let answer = if hello["repo"] == "/w/real" {
                    obsolete_from_12()
                } else {
                    "{\"t\":\"decline\",\"proto\":12,\"why\":\"this monitor serves /w/real, the client asked for /w/stale\"}\n"
                        .to_string()
                };
                (&stream).write_all(answer.as_bytes()).unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(
            probe(&socket, Path::new("/w/real"), deadline),
            Outcome::Asked
        );
        assert!(matches!(
            probe(&socket, Path::new("/w/stale"), deadline),
            Outcome::Refused(_)
        ));
    }

    #[test]
    fn only_a_clank_monitor_command_line_names_a_repo() {
        let argv = |args: &[&str]| {
            args.iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            repo_in_argv(&argv(&[
                "/home/u/.cargo/bin/clank",
                "monitor",
                "start",
                "--repo",
                "/w/a"
            ])),
            Some(PathBuf::from("/w/a"))
        );
        for not in [
            &[
                "/usr/bin/python3",
                "script.py",
                "monitor",
                "start",
                "--repo",
                "/w/a",
            ][..],
            &["/bin/clank", "wait", "--repo", "/w/a"],
            &["/bin/clank", "monitor", "start"],
            &["/bin/clank-web", "monitor", "start", "--repo", "/w/a"],
        ] {
            assert_eq!(repo_in_argv(&argv(not)), None, "{not:?}");
        }
    }

    struct NoProcess;

    impl super::super::monitor::Started for NoProcess {
        fn reap(self: Box<Self>) {}
    }

    fn refused(_: &Path, _: Instant) -> std::io::Result<std::os::unix::net::UnixStream> {
        Err(std::io::ErrorKind::ConnectionRefused.into())
    }

    fn cannot_spawn(_: &Path) -> std::io::Result<Box<dyn super::super::monitor::Started>> {
        Err(std::io::ErrorKind::NotFound.into())
    }

    fn spawns_nothing_that_answers(
        _: &Path,
    ) -> std::io::Result<Box<dyn super::super::monitor::Started>> {
        Ok(Box::new(NoProcess))
    }

    static SPAWNS: AtomicUsize = AtomicUsize::new(0);

    fn counted(_: &Path) -> std::io::Result<Box<dyn super::super::monitor::Started>> {
        SPAWNS.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(NoProcess))
    }

    /// A monitor for `repo` answering every greeting `answer`, published
    /// where clients look.
    fn serving(repo: &Path, answer: &'static str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("m.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut line = String::new();
                let _ = std::io::BufReader::new(&stream).read_line(&mut line);
                let _ = (&stream).write_all(answer.as_bytes());
            }
        });
        super::super::monitor::publish(repo, &super::super::monitor::describe(repo, 4242, &socket))
            .unwrap();
        dir
    }

    /// The real start's facts: nothing past the deadline; a spawn that
    /// failed is not a spawn; one that started but never answered is
    /// spawned yet unestablished; a compatible monitor already serving
    /// is reused.
    #[test]
    fn the_real_start_reports_what_it_established() {
        let repo = tempfile::tempdir().unwrap();
        let soon = || Instant::now() + Duration::from_millis(400);
        assert_eq!(
            start(
                repo.path(),
                Instant::now() - Duration::from_millis(1),
                &|| {}
            ),
            Successor::TimedOut
        );
        assert!(matches!(
            start_with(repo.path(), soon(), cannot_spawn, refused, |_| true, &|| {}),
            Successor::Unestablished { spawned: false, .. }
        ));
        let created = AtomicUsize::new(0);
        assert!(matches!(
            start_with(
                repo.path(),
                soon(),
                spawns_nothing_that_answers,
                refused,
                |_| true,
                &|| {
                    created.fetch_add(1, Ordering::SeqCst);
                }
            ),
            Successor::Unestablished { spawned: true, .. }
        ));
        assert_eq!(created.load(Ordering::SeqCst), 1, "creation was not told");
        let _serving = serving(
            repo.path(),
            Box::leak(
                format!(
                    "{{\"t\":\"ok\",\"proto\":{}}}\n",
                    super::super::monitor::PROTO
                )
                .into_boxed_str(),
            ),
        );
        assert_eq!(
            start_with(
                repo.path(),
                soon(),
                counted,
                super::super::monitor::connect_within,
                |_| true,
                &|| {}
            ),
            Successor::Reused
        );
    }

    /// Time spent before the spawn, however it was spent, comes out of
    /// the one deadline: once it is gone, nothing is started.
    #[test]
    fn a_start_whose_deadline_is_spent_beforehand_spawns_nothing() {
        let repo = tempfile::tempdir().unwrap();
        let before = SPAWNS.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_millis(50);
        let out = start_with(
            repo.path(),
            deadline,
            counted,
            refused,
            |_| {
                std::thread::sleep(Duration::from_millis(200));
                true
            },
            &|| {},
        );
        assert!(
            matches!(out, Successor::Unestablished { spawned: false, .. }),
            "{out:?}"
        );
        assert_eq!(
            SPAWNS.load(Ordering::SeqCst),
            before,
            "spawned past the deadline"
        );
    }

    /// A process is this user's only by its observed uid, not by being
    /// signalable; this test's own process is neither another user's
    /// nor a monitor.
    #[test]
    fn a_process_of_another_user_is_not_verified() {
        let pid = std::process::id() as i32;
        assert_eq!(crate::proc_identity::uid_of(pid), Some(me()));
        let why = repo_of_monitor(pid, me() + 1).unwrap_err();
        assert!(why.contains("runs as uid"), "{why}");
        let why = repo_of_monitor(pid, me()).unwrap_err();
        assert!(why.contains("not a clank monitor"), "{why}");
    }

    /// One endpoint's slow lookup holds back nothing else: a healthy
    /// obsolete monitor's successor starts before the cutoff, its two
    /// endpoints start it once, and the slow one's late answer starts
    /// nothing.
    #[test]
    fn a_stalled_endpoint_does_not_hold_back_a_healthy_repo() {
        let (_home, dir, _listeners) = namespace([1, 2, 3]);
        let started = Started::default();
        let mut effects = effects_of(|_, _, _| Outcome::Asked, started.clone());
        effects.repo_of = Box::new(|pid| match pid {
            2 => {
                std::thread::sleep(Duration::from_millis(500));
                Ok(verified(repo_for(2), 2))
            }
            n => Ok(verified(repo_for(1), n)),
        });
        let report = drive(
            &[dir],
            me(),
            std::sync::Arc::new(effects),
            Instant::now() + Duration::from_millis(200),
        );
        assert_eq!(report.successors, vec![(repo_for(1), Successor::Spawned)]);
        let stalled = report
            .swept
            .iter()
            .find(|s| s.socket.ends_with("2.sock"))
            .unwrap();
        assert_eq!(stalled.outcome, Outcome::TimedOut);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(
            *started.lock().unwrap(),
            vec![repo_for(1)],
            "started twice, or the late answer started one"
        );
    }

    /// A start still running at the cutoff is not reported as one that
    /// never began: what was known of its process then is kept.
    #[test]
    fn a_running_start_reports_what_it_had_done() {
        for (create_first, want) in [(true, Created::Yes), (false, Created::Unknown)] {
            let (_home, dir, _listeners) = namespace([1]);
            let mut effects = effects_of(|_, _, _| Outcome::Asked, Default::default());
            effects.start = Box::new(move |_, _, created| {
                if create_first {
                    created();
                }
                std::thread::sleep(Duration::from_millis(500));
                Successor::Spawned
            });
            let report = drive(
                &[dir],
                me(),
                std::sync::Arc::new(effects),
                Instant::now() + Duration::from_millis(100),
            );
            assert_eq!(
                report.successors,
                vec![(repo_for(1), Successor::InFlight { spawned: want })]
            );
        }
    }

    /// A fake old monitor for `retire_with`: alive until `exits_on`
    /// arrives, recording every signal.
    struct Old {
        alive: std::sync::atomic::AtomicBool,
        sent: std::sync::Mutex<Vec<libc::c_int>>,
    }

    impl Old {
        fn new(alive: bool) -> Self {
            Old {
                alive: std::sync::atomic::AtomicBool::new(alive),
                sent: Default::default(),
            }
        }

        fn retire(
            &self,
            exits_on: Option<libc::c_int>,
            grace: Duration,
            within: Duration,
        ) -> Retirement {
            retire_with(
                || {
                    if self.alive.load(Ordering::SeqCst) {
                        Still::Ours
                    } else {
                        Still::Gone
                    }
                },
                |sig| {
                    self.sent.lock().unwrap().push(sig);
                    if Some(sig) == exits_on {
                        self.alive.store(false, Ordering::SeqCst);
                    }
                },
                grace,
                Instant::now() + within,
            )
        }

        fn sent(&self) -> Vec<libc::c_int> {
            self.sent.lock().unwrap().clone()
        }
    }

    /// TERM first; KILL only for one that outlives its grace and is
    /// still the same process; nothing at all for one already gone; and
    /// never past the deadline.
    #[test]
    fn an_old_monitor_is_termed_then_killed_on_a_timeout() {
        let grace = Duration::from_millis(100);
        let within = Duration::from_secs(2);

        let exits = Old::new(true);
        assert_eq!(
            exits.retire(Some(libc::SIGTERM), grace, within),
            Retirement::Exited
        );
        assert_eq!(exits.sent(), vec![libc::SIGTERM]);

        let stubborn = Old::new(true);
        let started = Instant::now();
        assert_eq!(
            stubborn.retire(Some(libc::SIGKILL), grace, within),
            Retirement::Killed
        );
        assert_eq!(stubborn.sent(), vec![libc::SIGTERM, libc::SIGKILL]);
        assert!(started.elapsed() >= grace, "killed before its grace");

        let gone = Old::new(false);
        assert_eq!(gone.retire(None, grace, within), Retirement::Gone);
        assert!(gone.sent().is_empty());

        let unkillable = Old::new(true);
        let started = Instant::now();
        let short = Duration::from_millis(300);
        assert_eq!(
            unkillable.retire(None, grace, short),
            Retirement::StillRunning
        );
        assert!(
            started.elapsed() < short + Duration::from_millis(100),
            "{:?}",
            started.elapsed()
        );

        let deadline_first = Old::new(true);
        assert_eq!(
            deadline_first.retire(None, Duration::from_secs(5), Duration::from_millis(100)),
            Retirement::StillRunning
        );
        assert_eq!(
            deadline_first.sent(),
            vec![libc::SIGTERM],
            "a KILL past the deadline"
        );
    }

    /// A pid reused after TERM by another monitor of the same repo has
    /// another birth, so it is not the monitor that was asked, and is not
    /// sent KILL: the look compares the birth verified at the start.
    #[test]
    fn a_pid_reused_by_another_monitor_is_not_killed() {
        let termed = std::sync::atomic::AtomicBool::new(false);
        let sent = std::sync::Mutex::new(Vec::new());
        let out = retire_with(
            || {
                observe_with(
                    || {
                        Some(if termed.load(Ordering::SeqCst) {
                            birth(2)
                        } else {
                            birth(1)
                        })
                    },
                    || crate::signal::Probe::Ours,
                    &birth(1),
                )
            },
            |sig| {
                sent.lock().unwrap().push(sig);
                termed.store(true, Ordering::SeqCst);
            },
            Duration::from_millis(50),
            Instant::now() + Duration::from_secs(1),
        );
        assert_eq!(out, Retirement::Exited);
        assert_eq!(*sent.lock().unwrap(), vec![libc::SIGTERM]);
        assert_eq!(
            observe_with(|| None, || crate::signal::Probe::Unknown(1), &birth(1)),
            Still::Unknown,
            "an unreadable process is not gone"
        );
        assert_eq!(
            observe_with(|| None, || crate::signal::Probe::Gone, &birth(1)),
            Still::Gone
        );
    }

    /// No signal past the deadline: not at entry, not after a look that
    /// itself ran past it, not before KILL; and a process that can no
    /// longer be looked at is never reported as having exited.
    #[test]
    fn no_signal_is_sent_past_the_deadline() {
        let sends = |observe: &dyn Fn() -> Still, grace, within| {
            let sent = std::sync::Mutex::new(Vec::new());
            let out = retire_with(
                observe,
                |sig| sent.lock().unwrap().push(sig),
                grace,
                Instant::now() + within,
            );
            (out, sent.into_inner().unwrap())
        };
        let ours = || Still::Ours;
        let (out, sent) = sends(&ours, Duration::from_secs(1), Duration::ZERO);
        assert_eq!(
            (out, sent),
            (Retirement::StillRunning, vec![]),
            "expired at entry"
        );

        let slow = || {
            std::thread::sleep(Duration::from_millis(40));
            Still::Ours
        };
        let (_, sent) = sends(&slow, Duration::from_secs(1), Duration::from_millis(5));
        assert!(
            sent.is_empty(),
            "TERM after a look that ran past the deadline: {sent:?}"
        );

        let looks = AtomicUsize::new(0);
        let late_before_kill = || {
            if looks.fetch_add(1, Ordering::SeqCst) >= 2 {
                std::thread::sleep(Duration::from_millis(200));
            }
            Still::Ours
        };
        let (_, sent) = sends(
            &late_before_kill,
            Duration::from_millis(30),
            Duration::from_millis(150),
        );
        assert_eq!(
            sent,
            vec![libc::SIGTERM],
            "KILL after a look that ran past the deadline"
        );

        let looked = AtomicUsize::new(0);
        let unreadable = || {
            if looked.fetch_add(1, Ordering::SeqCst) == 0 {
                Still::Ours
            } else {
                Still::Unknown
            }
        };
        let (out, _) = sends(
            &unreadable,
            Duration::from_millis(50),
            Duration::from_millis(300),
        );
        assert_ne!(
            out,
            Retirement::Exited,
            "an unreadable process counted as gone"
        );
    }

    /// Two old monitors of one repo: the first to go starts the successor
    /// while the other still holds the repo, and creates nothing; when the
    /// other goes, the start is tried again and succeeds.
    #[test]
    fn a_start_that_created_nothing_is_retried_when_a_duplicate_goes() {
        let (_home, dir, _listeners) = namespace([1, 2]);
        let second_gone = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let starts = std::sync::Arc::new(AtomicUsize::new(0));
        let mut effects = effects_of(|_, _, _| Outcome::Asked, Default::default());
        effects.repo_of = Box::new(|pid| Ok(verified(repo_for(1), pid)));
        let gone = second_gone.clone();
        effects.retire = Box::new(move |pid, _, _| {
            if pid == 2 {
                std::thread::sleep(Duration::from_millis(300));
                gone.store(true, Ordering::SeqCst);
            }
            Retirement::Exited
        });
        let (gone, count) = (second_gone.clone(), starts.clone());
        effects.start = Box::new(move |_, _, _| {
            count.fetch_add(1, Ordering::SeqCst);
            if gone.load(Ordering::SeqCst) {
                Successor::Spawned
            } else {
                std::thread::sleep(Duration::from_millis(400));
                Successor::Unestablished {
                    spawned: false,
                    why: "the other old monitor still holds the repo".into(),
                }
            }
        });
        let report = drive(
            &[dir],
            me(),
            std::sync::Arc::new(effects),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(report.successors, vec![(repo_for(1), Successor::Spawned)]);
        assert_eq!(starts.load(Ordering::SeqCst), 2);
    }

    /// Through the driver: every asked monitor is ended, duplicates
    /// included, with one successor per repo; current and newer ones are
    /// neither signalled nor restarted; one still running at the deadline
    /// gets no successor.
    #[test]
    fn the_driver_ends_every_asked_monitor_and_starts_one_successor() {
        let (_home, dir, _listeners) = namespace([1, 2, 3, 4, 5]);
        let started = Started::default();
        let retired = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut effects = effects_of(
            |socket, _, _| match pid_of(socket) {
                1 | 2 | 5 => Outcome::Asked,
                3 => Outcome::Current,
                _ => Outcome::Newer,
            },
            started.clone(),
        );
        effects.repo_of = Box::new(|pid| {
            Ok(verified(
                match pid {
                    1 | 2 => repo_for(1),
                    n => repo_for(n),
                },
                pid,
            ))
        });
        let into = retired.clone();
        effects.retire = Box::new(move |pid, _, _| {
            into.lock().unwrap().push(pid);
            if pid == 5 {
                Retirement::StillRunning
            } else {
                Retirement::Exited
            }
        });
        let report = drive(
            &[dir],
            me(),
            std::sync::Arc::new(effects),
            Instant::now() + Duration::from_secs(5),
        );
        let mut ended = retired.lock().unwrap().clone();
        ended.sort();
        assert_eq!(ended, vec![1, 2, 5], "every asked monitor, and no other");
        assert_eq!(*started.lock().unwrap(), vec![repo_for(1)], "one successor");
        assert!(
            report
                .successors
                .contains(&(repo_for(1), Successor::Spawned))
        );
        assert!(
            report
                .successors
                .iter()
                .any(|(repo, s)| *repo == repo_for(5)
                    && matches!(s, Successor::Unestablished { spawned: false, .. }))
        );
        let of = |pid: i32| {
            report
                .swept
                .iter()
                .find(|s| pid_of(&s.socket) == pid)
                .unwrap()
                .retired
        };
        assert_eq!(of(3), None);
        assert_eq!(of(4), None);
        assert_eq!(of(5), Some(Retirement::StillRunning));
    }
}
