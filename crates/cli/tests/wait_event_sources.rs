//! Command wake sources for `clank wait` (extra-wait-events, M2):
//! spawned when the wait parks, completion is the wake, process-group
//! killed on any other exit. In-process; sh fixtures only.

mod common;
use common::TestEnv;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use clank::cli::WaitArgs;

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .unwrap()
            .success(),
        "git {args:?} failed"
    );
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn wait_args(repo: &Path, events: Vec<String>) -> WaitArgs {
    WaitArgs {
        repo: Some(repo.to_path_buf()),
        author: Some("rev".into()),
        die_with_owner: false,
        json: true,
        events,
        r#for: None,
        peek: false,
        no_cache: false,
        poll: true,
        no_poll: false,
    }
}

/// An idle-reviewer repo: no plans, so the wait parks until a source
/// (or timeout) wakes it.
fn idle_env() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("master", &["rev"], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, "seed.txt", "seed");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "scaffold"]);
    env
}

fn cmd_event(name: &str, argv: &[&str]) -> String {
    serde_json::json!({ "kind": "command", "name": name, "command": argv }).to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn command_completion_wakes_a_parked_wait() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let ev = cmd_event("probe", &["sh", "-c", "sleep 0.4; echo the-payload"]);
    clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect("the command's completion is the wake");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn returning_kills_the_whole_process_group() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("grandchild.pid");
    // The shell backgrounds a long sleep (a GRANDCHILD) and then waits
    // on it: only a group kill reaps the sleep.
    let script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
    let stuck = cmd_event("stuck", &["sh", "-c", &script]);
    // A second source that DOES complete supplies the wake. The wait
    // used to be ended by its own timeout; with the flag gone, the
    // teardown path under test is reached the way production reaches
    // it — by a wake (remove-wait-timeout).
    let waker = cmd_event("waker", &["sh", "-c", "sleep 1.5"]);
    clank::cli::wait::run(wait_args(repo, vec![stuck, waker]))
        .await
        .expect("the waker's completion returns the wait");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("grandchild pidfile written")
        .trim()
        .parse()
        .unwrap();
    // Give the group kill a beat, then the grandchild must be gone.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "grandchild {pid} must die with the wait (group kill)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chatty_output_is_bounded_and_signals_report_null_exit() {
    // Both contracts via the JSON the wait would emit — exercised at
    // the source-runner level through a real parked wait: a child that
    // spews 100 KiB then exits still wakes with a bounded tail, and a
    // self-killed child wakes with a null exit code. The emitted JSON
    // goes to stdout (not capturable in-process), so these assert the
    // OBSERVABLE contract: the wait returns Ok (woke) promptly rather
    // than hanging, for both shapes.
    let env = idle_env();
    let repo = env.repo();
    let chatty = cmd_event("chatty", &["sh", "-c", "yes xxxxxxxx | head -c 100000"]);
    clank::cli::wait::run(wait_args(repo, vec![chatty]))
        .await
        .expect("bounded ring: a chatty child still wakes the wait");

    let signaled = cmd_event("selfkill", &["sh", "-c", "kill -9 $$"]);
    clank::cli::wait::run(wait_args(repo, vec![signaled]))
        .await
        .expect("a signal-killed child still wakes the wait");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_source_wait_parks_to_its_timeout() {
    // A wait with NO event sources parks to its deadline and returns
    // parked. (The no-hot-loop fuse itself is pinned
    // deterministically by the `drain_external_fuses_a_closed_channel`
    // unit test — codex 907acd5 — since wall time can't distinguish a
    // spin from a park.)
    let env = idle_env();
    let repo = env.repo();
    common::assert_stays_parked(repo, wait_args(repo, vec![]), "idle with no sources").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_exit_wakes_even_with_a_pipe_holding_descendant() {
    // codex fc7a4ff: "command COMPLETING is the wake" means ROOT exit,
    // not pipe EOF. The root backgrounds a sleeper that INHERITS
    // stdout (holding the pipe open), then exits immediately. The wait
    // must wake PROMPTLY on root exit — not stall until the sleeper
    // ends or the timeout — and the sleeper must be group-killed.
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("holder.pid");
    // `sleep 300 &` inherits stdout; the root echoes and exits. The
    // 300s descendant IS the proof: if the code waited for pipe EOF
    // instead of root exit, the wait would blow past its 20s timeout
    // and return Err — so `run()` returning Ok within the timeout is
    // itself the "root exit is the wake" assertion (no flaky
    // wall-clock threshold under parallel load — codex fc7a4ff).
    let script = format!("sleep 300 & echo $! > '{}'; echo done", pidfile.display());
    let ev = cmd_event("root", &["sh", "-c", &script]);
    clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect("root exit wakes within the timeout, despite the pipe-holding descendant");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("descendant pidfile")
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "the pipe-holding descendant {pid} must be group-killed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn root_exit_wakes_even_with_a_continuously_writing_descendant() {
    // codex a69ddd8: a descendant that CONTINUOUSLY writes to the
    // inherited pipe keeps every post-exit read succeeding — the
    // per-read quiet timeout never trips. The absolute drain deadline
    // must still complete the wake. `yes` writes forever to the
    // inherited stdout; the root records its pid and exits.
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("yes.pid");
    let script = format!("yes & echo $! > '{}'; echo done", pidfile.display());
    let ev = cmd_event("root", &["sh", "-c", &script]);
    clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect("root exit wakes despite a continuously-writing descendant");
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("descendant pidfile")
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "the continuously-writing descendant {pid} must be group-killed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_event_during_the_refold_is_not_lost() {
    // codex 907acd5: the single return boundary drains once more after
    // the refold, so an external item that races a repo wake is never
    // aborted away. Arm a command source AND create repo work mid-wait;
    // whichever the beat sees, the other merges into the same result
    // and the wait RETURNS (never hangs).
    let env = idle_env();
    let repo = env.repo();
    let ev = cmd_event("racer", &["sh", "-c", "sleep 0.6; echo hi"]);
    let waiter = tokio::spawn(clank::cli::wait::run(wait_args(repo, vec![ev])));
    tokio::time::sleep(Duration::from_millis(400)).await;
    write(repo, ".clank/queue/500-p.md", "# p\n");
    tokio::time::timeout(common::race_deadline(repo), waiter)
        .await
        .expect("a beat with repo + external readiness must return, not hang")
        .expect("join")
        .expect("returns a combined result");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peek_starts_no_command_source() {
    // --peek returns from the initial pass, before sources spawn.
    let env = idle_env();
    let repo = env.repo();
    let marker = repo.join("source_ran.marker");
    let ev = cmd_event("touch", &["touch", marker.to_str().unwrap()]);
    let mut args = wait_args(repo, vec![ev]);
    args.peek = true;
    clank::cli::wait::run(args).await.expect("peek returns");
    // Give any (erroneously) spawned source a moment to run.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!marker.exists(), "--peek must spawn no source");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fast_source_wins_and_the_slow_source_is_group_killed() {
    // Two sources: one completes fast (the wake), one is a stuck shell
    // with a backgrounded grandchild. On return the supervisor aborts
    // AND joins the slow task; its process group — grandchild included
    // — must be dead.
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("slow.pid");
    let slow_script = format!("sleep 300 & echo $! > '{}'; wait", pidfile.display());
    let slow = cmd_event("slow", &["sh", "-c", &slow_script]);
    let fast = cmd_event("fast", &["sh", "-c", "sleep 0.5; echo done"]);
    clank::cli::wait::run(wait_args(repo, vec![slow, fast]))
        .await
        .expect("the fast source wakes the wait");

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("slow source's grandchild pidfile")
        .trim()
        .parse()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .unwrap()
        .success();
    assert!(
        !alive,
        "the slow source's grandchild {pid} must be group-killed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_github_poll_interval_fails_loud_at_arm_time() {
    // codex ef1861a: a malformed poll_interval must error at arm time,
    // not silently fall back to the default deep in the poll loop.
    let env = idle_env();
    let repo = env.repo();
    let ev = serde_json::json!({
        "kind": "github",
        "repo": "o/r",
        "events": ["issue_opened"],
        "poll_interval": "banana"
    })
    .to_string();
    let err = clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect_err("a bad poll_interval must fail loud");
    assert!(
        format!("{err:#}").contains("poll_interval"),
        "error names the bad field: {err:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_config_wait_events_is_a_hard_error() {
    // codex 1bbb61d: an unknown tagged kind in the FILE config must
    // propagate, not be silently treated as no sources.
    let env = idle_env();
    let repo = env.repo();
    let cfg_dir = repo.join(".clank/agents/rev");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.json"),
        r#"{"wait_events":[{"kind":"telepathy","repo":"x"}]}"#,
    )
    .unwrap();
    let err = clank::cli::wait::run(wait_args(repo, vec![]))
        .await
        .expect_err("unknown config kind must fail loud");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("config") || msg.contains("wait_events") || msg.contains("kind"),
        "error surfaces the config parse failure: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_event_json_is_an_arm_time_error() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let err = clank::cli::wait::run(wait_args(repo, vec!["{\"kind\":\"nope\"}".into()]))
        .await
        .expect_err("unknown kind fails loud at arm time");
    assert!(
        format!("{err:#}").contains("invalid --event"),
        "error names the offending input: {err:#}"
    );
}
