use super::*;
use crate::cli::monitor::fixtures::*;
use std::cell::Cell;

fn me() -> u32 {
    rustix::process::getuid().as_raw()
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
    assert!(
        tenure.standing_down(std::time::Instant::now()),
        "a lease another holds is given up at once"
    );
    assert_eq!(published.get(), 0);
}

/// A retirement under way is judged by its deadline: a fold already
/// old when it began does not force it out, an overrun does, and a
/// stall outside one still does.
#[test]
fn a_retirement_is_judged_by_its_deadline_not_the_folds_age() {
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
    let command = sh("trap '' TERM; echo $$ > ready; exec sleep 300").cwd(dir.path());
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
        command,
    )
    .await
    .unwrap();
    let host = tokio::spawn(async move {
        let _ = child.wait_async().await;
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
    let tasks = OwnedTasks::default();
    tasks.greeting.push(greeting);
    let (budget, settle) = (Duration::from_millis(325), Duration::from_millis(20));

    let started = std::time::Instant::now();
    let decision = stand_down(
        dir.path(),
        &mut Clients::new(),
        &mut Sources::hosting(Default::default()),
        tasks,
        None,
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
    let command = sh(script).cwd(dir);
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
        command,
    )
    .await
    .unwrap();
    let host = tokio::spawn(async move {
        let _ = child.wait_async().await;
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
        &mut Clients::new(),
        &mut Sources::hosting(Default::default()),
        OwnedTasks::default(),
        None,
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
    let mut sources = Sources::hosting(Default::default());
    let into = sources.emitter();
    let handle = tokio::spawn(crate::cli::wait::run_command_source_with(
        src,
        crate::cli::event_source::EmissionSink::for_owner(into, author.clone().into()),
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
        author.into(),
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

    sources.host(hosted);
    let decision = stand_down(
        std::path::Path::new("/w/proj"),
        &mut Clients::new(),
        &mut sources,
        OwnedTasks::default(),
        None,
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
