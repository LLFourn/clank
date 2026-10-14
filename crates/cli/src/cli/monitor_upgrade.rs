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

type RepoOf = Box<dyn Fn(i32) -> Result<PathBuf, String> + Send + Sync>;
type Probe = Box<dyn Fn(&Path, &Path, Instant) -> Outcome + Send + Sync>;
/// Start a successor for a repo before the deadline, calling the third
/// argument the moment a monitor process has been created.
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

/// How far one endpoint's operation got.
#[derive(Debug, Clone, Default)]
struct Progress {
    repo: Option<PathBuf>,
    outcome: Option<Outcome>,
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
    // Which endpoint's operation starts each repo's successor: the first
    // to hear that repo's monitor say it is obsolete.
    let claimed: Arc<Mutex<std::collections::BTreeMap<PathBuf, usize>>> = Arc::default();
    let (ended, ends) = std::sync::mpsc::channel::<()>();
    let operate = {
        let (progress, effects, claimed) = (progress.clone(), effects.clone(), claimed.clone());
        move |at: usize, socket: PathBuf, pid: i32| {
            let note = |f: &dyn Fn(&mut Progress)| f(&mut progress[at].lock().unwrap());
            let repo = match (effects.repo_of)(pid) {
                Ok(repo) => repo,
                Err(why) => return note(&|p| p.outcome = Some(Outcome::Skipped(why.clone()))),
            };
            note(&|p| p.repo = Some(repo.clone()));
            if Instant::now() >= deadline {
                return;
            }
            let outcome = (effects.probe)(&socket, &repo, deadline);
            note(&|p| p.outcome = Some(outcome.clone()));
            if outcome != Outcome::Asked || Instant::now() >= deadline {
                return;
            }
            {
                let mut claimed = claimed.lock().unwrap();
                if claimed.contains_key(&repo) {
                    return;
                }
                claimed.insert(repo.clone(), at);
            }
            note(&|p| p.successor = Some(Starting::Begun));
            let created = || note(&|p| p.successor = Some(Starting::Created));
            let started = (effects.start)(&repo, deadline, &created);
            note(&|p| p.successor = Some(Starting::Ended(started.clone())));
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
    let claimed = claimed.lock().unwrap().clone();
    let swept: Vec<Swept> = found
        .iter()
        .zip(&progress)
        .map(|((socket, _), p)| Swept {
            socket: socket.clone(),
            repo: p.repo.clone(),
            outcome: p.outcome.clone().unwrap_or(Outcome::TimedOut),
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
            let successor = match claimed.get(&repo).map(|at| &progress[*at].successor) {
                Some(Some(Starting::Ended(successor))) => successor.clone(),
                Some(Some(Starting::Created)) => Successor::InFlight {
                    spawned: Created::Yes,
                },
                Some(Some(Starting::Begun)) | Some(None) => Successor::InFlight {
                    spawned: Created::Unknown,
                },
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
fn repo_of_monitor(pid: i32, me: u32) -> Result<PathBuf, String> {
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
    repo_in_argv(&argv).ok_or(format!("process {pid} is not a clank monitor"))
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
        println!("{} ({repo}): {:?}", s.socket.display(), s.outcome);
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
            repo_of: Box::new(|pid| Ok(repo_for(pid))),
            probe: Box::new(probe),
            start: Box::new(move |repo, _, _| {
                started.lock().unwrap().push(repo.to_path_buf());
                Successor::Spawned
            }),
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
            Ok(repo_for(pid))
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
            Ok(repo_for(pid))
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
                    "{\"t\":\"decline\",\"proto\":12,\"why\":\"this monitor speaks proto 12, the client speaks 13\"}\n"
                } else {
                    "{\"t\":\"decline\",\"proto\":12,\"why\":\"this monitor serves /w/real, the client asked for /w/stale\"}\n"
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
        let _serving = serving(repo.path(), "{\"t\":\"ok\",\"proto\":13}\n");
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
                Ok(repo_for(2))
            }
            _ => Ok(repo_for(1)),
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
}
