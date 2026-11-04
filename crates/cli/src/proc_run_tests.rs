use super::*;
use crate::signal::{Probe, probe_group, probe_process};

/// Set in a re-run of this test binary that hosts one case.
const CHILD: &str = "CLANK_PROC_RUN_CHILD";

/// Whether this process is a re-run hosting a case.
pub(crate) fn is_child() -> bool {
    std::env::var_os(CHILD).is_some()
}

/// Run the test `exact` in a fresh process of this test binary, with
/// `env` set there, and return what it printed after `RESULT:`.
///
/// Cases that change the fd table, the fd limit, the environment, or
/// exec run here, never in the test runner.
pub(crate) fn in_child(exact: &str, env: &[(&str, &str)]) -> String {
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", exact, "--nocapture", "--test-threads", "1"])
        .env(CHILD, "1")
        .envs(env.iter().copied())
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| l.split_once("RESULT:").map(|(_, r)| r))
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!(
                "case {exact} reported nothing ({}):\n{text}\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
        })
}

fn report(result: impl std::fmt::Display) {
    println!("RESULT:{result}");
}

/// The fds a fixture child holds, as it lists them itself.
fn fds_of_child(spec: Spec) -> std::collections::BTreeSet<i32> {
    let out = run_blocking(
        spec.args(["/dev/fd/"])
            .stdout(Out::Capture { cap: 1 << 20 }),
        Duration::from_secs(20),
    )
    .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// A descriptor at `at`, inheritable: `F_DUPFD` never sets CLOEXEC.
fn inheritable_at(at: i32) -> i32 {
    let file = std::fs::File::open("/dev/null").unwrap();
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, at) };
    assert_eq!(fd, at, "fd {at} was free");
    fd
}

#[test]
fn fd_case_host() {
    if !is_child() {
        return;
    }
    let case = std::env::var("CLANK_PROC_RUN_CASE").unwrap();
    let fds = match case.as_str() {
        "sentinel" => {
            inheritable_at(77);
            fds_of_child(Spec::new("/bin/ls"))
        }
        "keep" => {
            let fd = inheritable_at(78);
            fds_of_child(Spec::new("/bin/ls").keep_fd(fd))
        }
        "above-limit" => {
            inheritable_at(256);
            let limit = libc::rlimit {
                rlim_cur: 64,
                rlim_max: libc::RLIM_INFINITY,
            };
            let mut old = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut old) };
            let limit = libc::rlimit {
                rlim_max: old.rlim_max,
                ..limit
            };
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
            fds_of_child(Spec::new("/bin/ls"))
        }
        other => panic!("no case {other}"),
    };
    report(
        fds.iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(","),
    );
}

fn listed(case: &str) -> std::collections::BTreeSet<i32> {
    in_child(
        "proc_run::tests::fd_case_host",
        &[("CLANK_PROC_RUN_CASE", case)],
    )
    .split(',')
    .filter_map(|s| s.parse().ok())
    .collect()
}

#[test]
fn a_child_inherits_no_descriptor_it_was_not_given() {
    let fds = listed("sentinel");
    assert!(!fds.contains(&77), "the sentinel leaked: {fds:?}");
    for std in [0, 1, 2] {
        assert!(fds.contains(&std), "fd {std} missing: {fds:?}");
    }
}

#[test]
fn a_kept_descriptor_is_inherited() {
    let fds = listed("keep");
    assert!(fds.contains(&78), "keep_fds was not honoured: {fds:?}");
}

/// The marking covers what is open, not a range up to the limit: an fd
/// opened before the soft limit was lowered sits above it.
#[test]
fn a_descriptor_above_the_soft_limit_is_not_inherited() {
    let fds = listed("above-limit");
    assert!(!fds.contains(&256), "fd 256 survived: {fds:?}");
    assert!(fds.contains(&2), "{fds:?}");
}

#[test]
fn a_missing_program_reports_its_spawn_error() {
    let err = run_blocking(
        Spec::new("/nonexistent/clank-proc-run-fixture"),
        Duration::from_secs(5),
    )
    .unwrap_err();
    match err {
        RunError::Spawn(e) => assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}"),
        other => panic!("{other:?}"),
    }
}

#[cfg(not(target_os = "macos"))]
#[test]
fn an_injected_marking_error_fails_the_spawn() {
    let mut spec = Spec::new("/usr/bin/true");
    spec.inject.mark_errno = Some(libc::EIO);
    match run_blocking(spec, Duration::from_secs(5)).unwrap_err() {
        RunError::Spawn(e) => assert_eq!(e.raw_os_error(), Some(libc::EIO), "{e}"),
        other => panic!("{other:?}"),
    }
}

/// A descriptor given as stdin is the child's stdin.
#[test]
fn a_given_descriptor_is_the_childs_stdin() {
    use std::io::Write;
    let (reader, mut writer) = std::io::pipe().unwrap();
    writer.write_all(b"handed over").unwrap();
    drop(writer);
    let out = run_blocking(
        Spec::new("/bin/cat")
            .stdin(Stdin::Fd(reader.into()))
            .stdout(Out::Capture { cap: 4096 }),
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(out.stdout, b"handed over");
}

/// Concurrent spawns briefly hold a copy of a lease's descriptor, on
/// every spawn path; a take through the lease helper must outwait that
/// copy and never read a released lease as held.
#[test]
fn a_released_lease_is_never_busy_to_the_lease_helper() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lease");
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spinners: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    run_blocking(Spec::new("/usr/bin/true"), Duration::from_secs(10)).unwrap();
                }
            })
        })
        .collect();
    let mut busy = 0;
    let mut slowest = Duration::ZERO;
    for _ in 0..3000 {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        let began = Instant::now();
        if !clank_user_config::lease::take(&file).unwrap() {
            busy += 1;
        }
        slowest = slowest.max(began.elapsed());
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for s in spinners {
        s.join().unwrap();
    }
    println!("slowest take of a released lease: {slowest:?}");
    assert_eq!(busy, 0, "a released lease read as held {busy} time(s)");
}

/// Every signal `proc_run` delivers passes here first. A delivery to a
/// pid this process has already reaped is refused and recorded: the
/// number may belong to a stranger by then, so the test build never
/// sends it, and a test can see that it was attempted.
pub(crate) mod signals {
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Log {
        reaped: HashMap<i32, bool>,
        refused: Vec<i32>,
    }

    static LOG: Mutex<Option<Log>> = Mutex::new(None);

    fn with<T>(f: impl FnOnce(&mut Log) -> T) -> T {
        f(LOG
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Log::default))
    }

    pub(crate) fn spawned(pid: i32) {
        with(|l| l.reaped.insert(pid, false));
    }

    pub(crate) fn reaped(pid: i32) {
        with(|l| l.reaped.insert(pid, true));
    }

    pub(in crate::proc_run) fn admit(target: super::Target) -> bool {
        let id = match target {
            super::Target::Group(pgid) => pgid.get(),
            super::Target::Process(pid) => pid.get(),
        };
        with(|l| {
            let stale = l.reaped.get(&id) == Some(&true);
            if stale {
                l.refused.push(id);
            }
            !stale
        })
    }

    pub(crate) fn refused(pid: i32) -> bool {
        with(|l| l.refused.contains(&pid))
    }
}

/// A finished child's number is never signalled once it is reaped,
/// whichever way it finished: a run, a wait then drop, an async run.
#[test]
fn nothing_signals_a_reaped_leader() {
    let leader_of = |out: &[u8]| -> i32 { String::from_utf8_lossy(out).trim().parse().unwrap() };
    let out = run_blocking(
        sh("echo $$")
            .stdout(Out::Capture { cap: 4096 })
            .group(Group::New),
        Duration::from_secs(10),
    )
    .unwrap();
    let blocking = leader_of(&out.stdout);
    let out = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(run(
            sh("echo $$")
                .stdout(Out::Capture { cap: 4096 })
                .group(Group::New),
            Duration::from_secs(10),
        ))
        .unwrap();
    let async_run = leader_of(&out.stdout);
    let mut waited = start(Spec::new("/usr/bin/true").group(Group::New)).unwrap();
    let dropped = waited.pid() as i32;
    waited.wait().unwrap();
    drop(waited);
    for (how, pid) in [
        ("blocking run", blocking),
        ("async run", async_run),
        ("wait then drop", dropped),
    ] {
        assert!(
            !signals::refused(pid),
            "{how}: {pid} was signalled after it was reaped"
        );
    }
}

// ── completion ────────────────────────────────────────────────────

fn sh(script: &str) -> Spec {
    Spec::new("/bin/sh").args(["-c", script])
}

fn read_pid(path: &std::path::Path) -> i32 {
    for _ in 0..500 {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no pid at {}", path.display());
}

/// Whether `pid` is gone, waiting a bounded while for whoever reaps it.
fn gone(pid: i32) -> bool {
    for _ in 0..300 {
        if probe_process(pid) == Probe::Gone {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn group_gone(pgid: i32) -> bool {
    let pgid = Pgid::new(pgid).unwrap();
    for _ in 0..300 {
        if probe_group(pgid) == Probe::Gone {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

/// `waitpid` has nothing left to say about `pid`. Asked only once the
/// pid is gone, so this never reaps on anyone's behalf.
fn echild(pid: i32) -> bool {
    let mut status = 0;
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    rc == -1 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD)
}

/// A group the test owns and nothing else may touch.
struct Bystander(std::process::Child);

impl Bystander {
    fn new() -> Self {
        use std::os::unix::process::CommandExt;
        Self(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap(),
        )
    }
    fn untouched(&self) -> bool {
        probe_group(Pgid::new(self.0.id() as i32).unwrap()) == Probe::Ours
            && probe_process(self.0.id() as i32) == Probe::Ours
    }
}

impl Drop for Bystander {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The bug itself: a descendant keeps the inherited stdout open after
/// the child exits. Waiting for EOF would wait for the descendant.
#[test]
fn a_descendant_holding_the_pipe_is_incomplete_without_waiting_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let started = Instant::now();
    let err = run_blocking(
        sh(&format!(
            "sleep 30 & echo $! > '{}'; echo $$",
            pidfile.display()
        ))
        .stdout(Out::Capture { cap: 1 << 20 })
        .group(Group::New),
        Duration::from_secs(10),
    )
    .unwrap_err();
    assert!(
        matches!(err, RunError::Incomplete { .. }),
        "{err:?} after {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        gone(read_pid(&pidfile)),
        "the group kill took the descendant"
    );
}

#[tokio::test]
async fn the_async_run_is_incomplete_without_waiting_for_it_too() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let started = Instant::now();
    let err = run(
        sh(&format!("sleep 30 & echo $! > '{}'", pidfile.display()))
            .stdout(Out::Capture { cap: 1 << 20 })
            .group(Group::New),
        Duration::from_secs(10),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, RunError::Incomplete { .. }), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(gone(read_pid(&pidfile)));
}

/// A writer that lets go a second past the patience still leaves the
/// output unknown, and the run does not wait for it to.
#[test]
fn a_writer_that_lets_go_after_the_patience_is_still_incomplete() {
    let holds = HELD_PATIENCE + Duration::from_secs(1);
    for blocking in [true, false] {
        let spec = sh(&format!("(sleep {}) & exit 0", holds.as_secs_f64()))
            .stdout(Out::Capture { cap: 1 << 20 })
            .group(Group::New);
        let started = Instant::now();
        let result = if blocking {
            run_blocking(spec, Duration::from_secs(10))
        } else {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run(spec, Duration::from_secs(10)))
        };
        assert!(
            matches!(result, Err(RunError::Incomplete { .. })),
            "blocking={blocking}: {result:?}"
        );
        assert!(
            started.elapsed() >= HELD_PATIENCE && started.elapsed() < holds,
            "blocking={blocking}: decided after {:?}",
            started.elapsed()
        );
    }
}

/// A writer that lets go well inside the patience, as a concurrent
/// spawn's copy does, leaves the output whole, with what it wrote.
#[test]
fn a_writer_that_lets_go_within_the_patience_is_whole() {
    for blocking in [true, false] {
        let spec = sh("(sleep 0.05; echo late) &")
            .stdout(Out::Capture { cap: 1 << 20 })
            .group(Group::New);
        let out = if blocking {
            run_blocking(spec, Duration::from_secs(10))
        } else {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run(spec, Duration::from_secs(10)))
        }
        .unwrap_or_else(|e| panic!("blocking={blocking}: {e:?}"));
        assert_eq!(out.stdout, b"late\n", "blocking={blocking}");
    }
}

/// The failure on Linux CI: spawns on other threads hold copies of a
/// run's pipe from their fork to their exec, and a run that judged its
/// output at the instant of exit read those copies as holders. The
/// spinners fork, as Linux's spawn does, so macOS reproduces it too: a
/// `pre_exec` hook takes std off its `posix_spawn` path.
#[test]
fn concurrent_forks_never_make_a_run_incomplete() {
    use std::os::unix::process::CommandExt;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spinners: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let mut fork = std::process::Command::new("/usr/bin/true");
                    // SAFETY: the hook does nothing.
                    unsafe { fork.pre_exec(|| Ok(())) };
                    fork.status().unwrap();
                }
            })
        })
        .collect();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let mut incomplete = 0;
    for i in 0..1000 {
        let spec = Spec::new("/bin/echo")
            .arg("hi")
            .stdout(Out::Capture { cap: 4096 });
        let result = if i % 2 == 0 {
            run_blocking(spec, Duration::from_secs(10))
        } else {
            runtime.block_on(run(spec, Duration::from_secs(10)))
        };
        match result {
            Ok(out) => assert_eq!(out.stdout, b"hi\n"),
            Err(RunError::Incomplete { .. }) => incomplete += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for s in spinners {
        s.join().unwrap();
    }
    assert_eq!(
        incomplete, 0,
        "{incomplete} run(s) read a passing copy as a holder"
    );
}

/// One deadline for the whole run: an exit and its output found after
/// the budget has passed are a timeout, not a completion.
#[test]
fn a_completion_found_after_the_budget_is_a_timeout() {
    for blocking in [true, false] {
        let mut spec = sh("echo done")
            .stdout(Out::Capture { cap: 4096 })
            .group(Group::New);
        spec.inject.reader_delay = Some(Duration::from_millis(150));
        let budget = Duration::from_millis(20);
        let result = if blocking {
            run_blocking(spec, budget)
        } else {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run(spec, budget))
        };
        assert!(
            matches!(result, Err(RunError::Timeout { .. })),
            "blocking={blocking}: {result:?}"
        );
    }
}

/// A stream that never runs dry times out on the budget, and the other
/// stream is still read meanwhile: a writer of a megabyte to stdout
/// finishes only if something drains it.
#[test]
fn a_sustained_stream_times_out_and_starves_nothing() {
    for blocking in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let drained = dir.path().join("drained");
        let spec = sh(&format!(
            "yes >&2 & head -c 1048576 /dev/zero; touch '{}'; wait",
            drained.display()
        ))
        .stdout(Out::Capture { cap: 4 << 20 })
        .stderr(Out::Ring { bytes: 4096 })
        .group(Group::New);
        let budget = Duration::from_secs(2);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = if blocking {
                run_blocking(spec, budget)
            } else {
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(run(spec, budget))
            };
            let _ = tx.send(result);
        });
        let result = rx
            .recv_timeout(budget + Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("blocking={blocking}: the run outlived its budget"));
        assert!(
            matches!(result, Err(RunError::Timeout { .. })),
            "blocking={blocking}: {result:?}"
        );
        assert!(drained.exists(), "blocking={blocking}: stdout was starved");
    }
}

/// A program is looked up as execvp in the child would: relative and
/// empty `PATH` entries, and a relative program, from the CHILD's
/// directory, with a match it may not execute passed over. std's own
/// spawn, in a test only, is the control.
#[test]
fn a_program_is_found_from_the_childs_directory() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let proj = root.path().join("proj");
    let script = |rel: &str, says: &str, mode: u32| {
        let at = proj.join(rel);
        std::fs::create_dir_all(at.parent().unwrap()).unwrap();
        std::fs::write(&at, format!("#!/bin/sh\necho {says}\n")).unwrap();
        std::fs::set_permissions(&at, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    script("bin/tool", "bin-tool", 0o755);
    script("tool2", "cwd-tool", 0o755);
    script("noexec/tool", "denied", 0o644);
    assert_ne!(
        std::env::current_dir().unwrap(),
        proj,
        "the parent is elsewhere"
    );
    let absolute = proj.join("bin/tool");
    for (path, program, says) in [
        ("bin", "tool", "bin-tool"),
        ("/nonexistent:", "tool2", "cwd-tool"),
        ("noexec:bin", "tool", "bin-tool"),
        ("/nonexistent", "bin/tool", "bin-tool"),
        ("/nonexistent", absolute.to_str().unwrap(), "bin-tool"),
    ] {
        let out = run_blocking(
            Spec::new(program)
                .env("PATH", path)
                .cwd(&proj)
                .stdout(Out::Capture { cap: 4096 }),
            Duration::from_secs(10),
        )
        .unwrap_or_else(|e| panic!("PATH={path} {program}: {e:?}"));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            says,
            "PATH={path} {program}"
        );
        let control = std::process::Command::new(program)
            .env("PATH", path)
            .current_dir(&proj)
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&control.stdout).trim(),
            says,
            "std: PATH={path} {program}"
        );
    }
    match run_blocking(
        Spec::new("absent").env("PATH", "bin").cwd(&proj),
        Duration::from_secs(10),
    ) {
        Err(RunError::Spawn(e)) => assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn removed_cwd_host() {
    if !is_child() {
        return;
    }
    let gone = tempfile::tempdir().unwrap();
    let there = tempfile::tempdir().unwrap();
    std::env::set_current_dir(gone.path()).unwrap();
    std::fs::remove_dir(gone.path()).unwrap();
    let ours = run_blocking(
        Spec::new("/bin/pwd")
            .cwd(there.path())
            .stdout(Out::Capture { cap: 4096 }),
        Duration::from_secs(10),
    );
    let native = std::process::Command::new("/bin/pwd")
        .current_dir(there.path())
        .output();
    report(format!(
        "ours={} native={}",
        ours.is_ok_and(|o| o.status.success()),
        native.is_ok_and(|o| o.status.success())
    ));
}

/// The parent's directory is asked for only when the child's match is
/// relative to it, so a removed one stops nothing launched by absolute
/// paths.
#[test]
fn a_removed_parent_directory_stops_no_absolute_launch() {
    assert_eq!(
        in_child("proc_run::tests::removed_cwd_host", &[]),
        "ours=true native=true"
    );
}

#[test]
fn a_late_reader_still_gets_the_whole_output() {
    for blocking in [true, false] {
        let mut spec = sh("echo hello; echo there >&2")
            .stdout(Out::Capture { cap: 1 << 20 })
            .stderr(Out::Ring { bytes: 4096 })
            .group(Group::New);
        spec.inject.reader_delay = Some(Duration::from_millis(150));
        let out = if blocking {
            run_blocking(spec, Duration::from_secs(10))
        } else {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run(spec, Duration::from_secs(10)))
        }
        .unwrap();
        assert_eq!(out.stdout, b"hello\n", "blocking={blocking}");
        assert_eq!(out.stderr, b"there\n", "blocking={blocking}");
    }
}

#[test]
fn eight_mebibytes_under_a_sixteen_mebibyte_cap_is_whole() {
    for blocking in [true, false] {
        let spec = Spec::new("/bin/dd")
            .args(["if=/dev/zero", "bs=1048576", "count=8"])
            .stdout(Out::Capture { cap: 16 << 20 })
            .stderr(Out::Capture { cap: 1 << 20 })
            .group(Group::New);
        let out = if blocking {
            run_blocking(spec, Duration::from_secs(30))
        } else {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run(spec, Duration::from_secs(30)))
        }
        .unwrap();
        assert_eq!(out.stdout.len(), 8 << 20, "blocking={blocking}");
    }
}

#[test]
fn output_over_its_cap_is_an_overflow_never_a_truncated_success() {
    for blocking in [true, false] {
        let spec = Spec::new("/bin/dd")
            .args(["if=/dev/zero", "bs=1048576", "count=2"])
            .stdout(Out::Capture { cap: 1 << 20 })
            .group(Group::New);
        let result = if blocking {
            run_blocking(spec, Duration::from_secs(30))
        } else {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run(spec, Duration::from_secs(30)))
        };
        match result {
            Err(RunError::Overflow {
                stream: Stream::Stdout,
                cap,
                kept,
            }) => {
                assert_eq!(cap, 1 << 20);
                assert_eq!(kept.len(), cap, "the first cap bytes are kept");
            }
            other => panic!("blocking={blocking}: {other:?}"),
        }
    }
}

#[test]
fn a_slow_writer_inside_its_budget_completes() {
    let out = run_blocking(
        sh("for i in 1 2 3 4 5; do echo $i; sleep 0.1; done").stdout(Out::Capture { cap: 4096 }),
        Duration::from_secs(10),
    )
    .unwrap();
    assert_eq!(out.stdout, b"1\n2\n3\n4\n5\n");
}

// ── cleanup ───────────────────────────────────────────────────────

/// A child leading its group with a descendant, both recorded.
fn tree(dir: &std::path::Path, tail: &str) -> (Spec, std::path::PathBuf, std::path::PathBuf) {
    let leader = dir.join("leader");
    let descendant = dir.join("descendant");
    let spec = sh(&format!(
        "echo $$ > '{}'; sleep 30 >/dev/null 2>&1 & echo $! > '{}'; {tail}",
        leader.display(),
        descendant.display()
    ))
    .stdout(Out::Capture { cap: 4096 })
    .group(Group::New);
    (spec, leader, descendant)
}

fn assert_cleaned(leader: &std::path::Path, descendant: &std::path::Path, bystander: &Bystander) {
    let (leader, descendant) = (read_pid(leader), read_pid(descendant));
    assert!(gone(leader), "the child was reaped");
    assert!(echild(leader), "waitpid has nothing left for {leader}");
    assert!(gone(descendant), "the descendant was killed");
    assert!(group_gone(leader), "the group is empty");
    assert!(bystander.untouched(), "an unrelated group was signalled");
}

#[test]
fn a_timeout_kills_the_group_and_reaps() {
    let bystander = Bystander::new();
    let dir = tempfile::tempdir().unwrap();
    let (spec, leader, descendant) = tree(dir.path(), "sleep 30");
    let err = run_blocking(spec, Duration::from_millis(500)).unwrap_err();
    assert!(matches!(err, RunError::Timeout { .. }), "{err:?}");
    assert_cleaned(&leader, &descendant, &bystander);
}

#[tokio::test]
async fn an_async_timeout_kills_the_group_and_reaps() {
    let bystander = Bystander::new();
    let dir = tempfile::tempdir().unwrap();
    let (spec, leader, descendant) = tree(dir.path(), "sleep 30");
    let err = run(spec, Duration::from_millis(500)).await.unwrap_err();
    assert!(matches!(err, RunError::Timeout { .. }), "{err:?}");
    assert_cleaned(&leader, &descendant, &bystander);
}

#[tokio::test]
async fn a_cancelled_run_kills_the_group_and_reaps() {
    let bystander = Bystander::new();
    let dir = tempfile::tempdir().unwrap();
    let (spec, leader, descendant) = tree(dir.path(), "sleep 30");
    let running = run(spec, Duration::from_secs(30));
    let leader_file = leader.clone();
    let cancelled = tokio::time::timeout(Duration::from_secs(5), async move {
        tokio::pin!(running);
        tokio::select! {
            _ = &mut running => panic!("it ran to the end"),
            _ = async {
                while !leader_file.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            } => {}
        }
    })
    .await;
    assert!(cancelled.is_ok());
    assert_cleaned(&leader, &descendant, &bystander);
}

#[test]
fn a_spawn_error_touches_nothing() {
    let bystander = Bystander::new();
    let err = run_blocking(
        Spec::new("/nonexistent/clank-proc-run-fixture").group(Group::New),
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(err.not_started(), "{err:?}");
    assert!(bystander.untouched());
}

#[test]
fn a_normal_exit_still_ends_a_live_descendant() {
    let bystander = Bystander::new();
    let dir = tempfile::tempdir().unwrap();
    let (spec, leader, descendant) = tree(dir.path(), "echo done");
    let out = run_blocking(spec, Duration::from_secs(10)).unwrap();
    assert_eq!(out.stdout, b"done\n");
    assert_cleaned(&leader, &descendant, &bystander);
}

/// A fake duplex peer answering over stdin/stdout, then a protocol
/// error on the caller's side: dropping it ends the group.
#[tokio::test]
async fn a_duplex_peer_answers_and_a_protocol_error_cleans_up() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let bystander = Bystander::new();
    let dir = tempfile::tempdir().unwrap();
    let descendant = dir.path().join("descendant");
    let mut peer = start(
        sh(&format!(
            "sleep 30 >/dev/null 2>&1 & echo $! > '{}'; while read line; do echo \"got $line\"; done",
            descendant.display()
        ))
        .stdin(Stdin::Piped)
        .stdout(Out::Piped)
        .group(Group::New),
    )
    .unwrap();
    let leader = peer.pid() as i32;
    let mut input = peer.take_stdin_async().unwrap();
    let mut output = tokio::io::BufReader::new(peer.take_stdout_async().unwrap()).lines();
    input.write_all(b"ping\n").await.unwrap();
    assert_eq!(
        output.next_line().await.unwrap().as_deref(),
        Some("got ping")
    );
    input.write_all(b"not the protocol\n").await.unwrap();
    let answer = output.next_line().await.unwrap();
    assert_ne!(answer.as_deref(), Some("pong"), "a protocol error");
    drop(peer);
    assert!(gone(leader));
    assert!(echild(leader));
    assert!(gone(read_pid(&descendant)));
    assert!(group_gone(leader));
    assert!(bystander.untouched());
}

// ── owned children ────────────────────────────────────────────────

/// B exits while A runs on: B is reaped without waiting for A.
#[test]
fn the_reaper_reaps_a_short_child_behind_a_long_one() {
    let long = start(Spec::new("/bin/sleep").arg("30").group(Group::New)).unwrap();
    let a = long.pid() as i32;
    long.detach();
    let short = start(Spec::new("/usr/bin/true")).unwrap();
    let b = short.pid() as i32;
    short.detach();
    assert!(gone(b), "B is reaped while A still runs");
    assert!(echild(b));
    assert_eq!(probe_process(a), Probe::Ours, "A still runs");
    crate::signal::signal_group(Pgid::new(a).unwrap(), libc::SIGKILL);
    assert!(gone(a), "A is reaped once it exits");
}

#[test]
fn a_detached_child_outlives_its_handle_and_leaves_no_zombie() {
    let child = start(sh("sleep 0.3").group(Group::New)).unwrap();
    let pid = child.pid() as i32;
    child.detach();
    assert_eq!(probe_process(pid), Probe::Ours, "detach does not kill");
    assert!(gone(pid), "and the reaper collects it");
    assert!(echild(pid));
}

/// A child joins the group it is given, and dropping it leaves that
/// group alone: its owner signals and collects it.
#[test]
fn a_joined_child_is_in_the_group_and_its_drop_signals_nothing() {
    let leader = start(Spec::new("/bin/sleep").arg("30").group(Group::New)).unwrap();
    let pgid = leader.pgid().unwrap();
    let joined = start(Spec::new("/bin/sleep").arg("30").group(Group::Join(pgid))).unwrap();
    let member = joined.pid() as i32;
    assert_eq!(unsafe { libc::getpgid(member) }, pgid.get());
    drop(joined);
    std::thread::sleep(Duration::from_millis(200));
    // Asked of the waits this test owns: a killed child is still a
    // zombie that a liveness probe would read as running.
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(member, &mut status, libc::WNOHANG) },
        0,
        "the member was signalled"
    );
    let mut leader = leader;
    assert!(
        leader.try_wait().unwrap().is_none(),
        "the leader was signalled"
    );
    drop(leader);
    let member_pid = Pid::new(member).unwrap();
    let mut collected = false;
    for _ in 0..300 {
        crate::signal::collect_exited_child(member_pid);
        if probe_process(member) == Probe::Gone {
            collected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(collected, "the group's owner ends and collects it");
}

/// Shutting a runtime down drops its tasks' children, and those are
/// reaped.
#[test]
fn a_runtime_shutdown_leaves_no_zombie() {
    let dir = tempfile::tempdir().unwrap();
    let (spec, leader, descendant) = tree(dir.path(), "sleep 30");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.spawn(run(spec, Duration::from_secs(30)));
    let pid = read_pid(&leader);
    drop(runtime);
    assert!(gone(pid));
    assert!(echild(pid));
    assert!(gone(read_pid(&descendant)));
}

// ── exec ──────────────────────────────────────────────────────────

#[test]
fn exec_case_host() {
    if !is_child() || std::env::var_os("CLANK_PROC_RUN_EXEC").is_none() {
        return;
    }
    let err = exec(
        Spec::new("/nonexistent/clank-proc-run-fixture")
            .stdin(Stdin::Inherit)
            .stdout(Out::Inherit)
            .stderr(Out::Inherit),
    );
    report(format!("returned:{:?}", err.kind()));
}

#[test]
fn a_failed_exec_returns_its_error() {
    let said = in_child(
        "proc_run::tests::exec_case_host",
        &[("CLANK_PROC_RUN_EXEC", "1")],
    );
    assert_eq!(said, format!("returned:{:?}", io::ErrorKind::NotFound));
}

// ── cost ──────────────────────────────────────────────────────────

/// Spawn cost before (std's posix_spawn path) and after (fork with the
/// descriptor hook). Run by hand: `--ignored --nocapture spawn_cost`.
#[test]
#[ignore]
fn spawn_cost() {
    const N: u32 = 200;
    let started = Instant::now();
    for _ in 0..N {
        std::process::Command::new("/usr/bin/true")
            .stdout(std::process::Stdio::piped())
            .output()
            .unwrap();
    }
    let before = started.elapsed() / N;
    let started = Instant::now();
    for _ in 0..N {
        run_blocking(
            Spec::new("/usr/bin/true").stdout(Out::Capture { cap: 4096 }),
            Duration::from_secs(5),
        )
        .unwrap();
    }
    let after = started.elapsed() / N;
    println!("spawn cost: std {before:?}, proc_run {after:?} per /usr/bin/true");
}
