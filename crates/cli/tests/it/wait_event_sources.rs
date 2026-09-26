//! Command wake sources for `clank wait` (extra-wait-events, M2):
//! spawned when the wait parks, completion is the wake, process-group
//! killed on any other exit. In-process; sh fixtures only.

use crate::common;
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
        tag: None,
        author: Some("rev".into()),
        die_with_owner: false,
        deliver_to_session: None,
        expected_generation: None,
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

/// A printed EVENT wakes a parked wait.
///
/// It used to be the command's completion
/// (a-command-is-an-event-source-like-any-other): a source is a
/// long-running producer now, so exiting is not news — a line is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_printed_event_wakes_a_parked_wait() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let ev = cmd_event(
        "probe",
        &[
            "sh",
            "-c",
            r#"sleep 0.4; printf '{"id":"1","kind":"probe","content":"the-payload"}\n'; sleep 300"#,
        ],
    );
    clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect("a line the producer printed is the wake");
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
    // A second source EMITS to supply the wake. The wait used to be
    // ended by its own timeout; with the flag gone, the teardown path
    // under test is reached the way production reaches it — by a wake
    // (remove-wait-timeout). It emits rather than completing, because
    // completing is no longer a wake.
    let waker = cmd_event(
        "waker",
        &[
            "sh",
            "-c",
            r#"sleep 1.5; printf '{"id":"1","kind":"waker","content":"go"}\n'; sleep 300"#,
        ],
    );
    clank::cli::wait::run(wait_args(repo, vec![stuck, waker]))
        .await
        .expect("the waker's event returns the wait");
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
async fn a_ci_watch_arms() {
    // The kinds are readable now, so a watch that names them arms
    // like any other. (It replaces the refusal that stood between the
    // vocabulary landing and the reader landing.) No network: the
    // wait parks and the test's timeout ends it.
    let env = idle_env();
    let repo = env.repo();
    let ev = serde_json::json!({
        "kind": "github",
        "repo": "o/r",
        "events": ["ci_failed"],
        "prs": [7],
    })
    .to_string();
    let armed = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        clank::cli::wait::run(wait_args(repo, vec![ev])),
    )
    .await;
    assert!(
        armed.is_err(),
        "arming succeeds and the wait parks; it does not refuse: {armed:?}",
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

/// A WAIT CLAIMS THE SESSION IT WAS LAUNCHED IN, NEVER THE ROSTER'S.
///
/// The claim says where this process's completion wake goes. An
/// earlier version copied the label's BOUND session out of config — a
/// roster fact, not a delivery fact — so a wait launched anywhere
/// claimed the bound session, and a stale listed task then silenced
/// that session's turns (codex 2f38deb).
///
/// Asserted as a DIFFERENCE rather than an equality, because the
/// ambient session depends on where the suite runs: under a harness
/// the test process inherits one, and from a bare terminal it has
/// none and claims nothing. Both are correct; claiming the bound
/// session is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_claims_its_launch_session_not_the_rosters() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo().to_path_buf();
    let agent_dir = clank::agent_store::agents_root(&repo).join("rev");

    // BOUND, so the roster has a session to be wrongly copied. That
    // is the whole trap: binding is what made the old code claim.
    let bound = "44444444-4444-4444-4444-444444444444";
    clank::agent_store::bind_session_to_agent(
        &repo,
        &clank_core::ids::AgentLabel::parse("rev").unwrap(),
        clank_core::vocab::Tool::Codex,
        &clank_core::ids::SessionId::parse(bound).unwrap(),
    )
    .unwrap();

    let handle = tokio::spawn(clank::cli::wait::run(wait_args(&repo, vec![])));
    let mut claim = None;
    for _ in 0..40 {
        claim = clank::wait_claim::read(&agent_dir);
        if claim.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    handle.abort();
    let _ = handle.await;

    match claim {
        // Launched inside a harness: it claims THAT session.
        Some(c) => assert_ne!(
            c.session, bound,
            "the claim copied the roster's bound session instead of its own launch",
        ),
        // No harness around it: nothing to deliver a wake to, so
        // nothing claimed.
        None => {
            assert_eq!(
                clank::wait_claim::launch_session(),
                None,
                "a wait inside a session must publish a claim",
            );
        }
    }
}

/// A command source that could never work fails the ARM,/// A command source that could never work fails the ARM,/// A command source that could never work fails the ARM, rather than
/// printing from inside its task and leaving the wait running
/// without it.
///
/// Both of these used to arm fine and then quietly return from the
/// source task, so the operator got a wait that looked healthy and
/// watched nothing (codex 072e459). A source the wait cannot run is
/// a configuration error, and the wait refuses.
#[tokio::test]
async fn a_command_source_that_cannot_run_fails_the_arm() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();

    let empty = r#"{"kind":"command","name":"deploys","command":[]}"#;
    let err = clank::cli::wait::run(wait_args(repo, vec![empty.into()]))
        .await
        .expect_err("an empty command has nothing to run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("deploys") && msg.contains("empty command"),
        "the error names the source and what is wrong: {msg}"
    );

    // A two-cell glyph would misalign every TUI row after it, and
    // refusing at render is too late — so it refuses at arm.
    let wide = r#"{"kind":"command","name":"noisy","command":["true"],"glyph":"💤"}"#;
    let err = clank::cli::wait::run(wait_args(repo, vec![wide.into()]))
        .await
        .expect_err("a two-column glyph cannot sit in a one-column cell");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("noisy") && msg.contains("glyph"),
        "the error names the source and the glyph: {msg}"
    );

    // The companion — that a VALID source is not refused — is a lib
    // test (`cli::wait::tests::arm_time_validation_refuses_only_what_cannot_run`).
    // It cannot live here: arming a good source PARKS, and an idle
    // wait has no timeout to end it (remove-wait-timeout), so the
    // only thing that would return is the backoff's failure event
    // several seconds later. A test that depends on that is timing
    // the constants, not the refusal.
}

/// A producer whose descendant holds the pipe open still wakes the
/// wait, and the descendant dies with it.
///
/// Replaces three tests of the one-shot contract
/// (a-command-is-an-event-source-like-any-other): root exit is no
/// longer a wake, so "root exit wakes despite a pipe-holding /
/// continuously-writing descendant" cannot be asserted here. The
/// hazard those protected — a held pipe stalling the source — is
/// asserted where the restart is observable, in
/// `cli::wait::tests::a_pipe_holding_descendant_does_not_stall_the_restart`.
/// What is integration-level is that the event still arrives and the
/// group is still killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pipe_holding_descendant_neither_blocks_the_wake_nor_survives_it() {
    let _serial = common::serial();
    let env = idle_env();
    let repo = env.repo();
    let pidfile = repo.join("holder.pid");
    // `yes` inherits stdout AND writes to it forever: the pipe never
    // reaches EOF and never goes quiet.
    let script = format!(
        r#"yes > /dev/null & echo $! > '{}'; sleep 300 & printf '{{"id":"1","kind":"k","content":"emitted"}}\n'; sleep 300"#,
        pidfile.display()
    );
    let ev = cmd_event("holder", &["sh", "-c", &script]);
    clank::cli::wait::run(wait_args(repo, vec![ev]))
        .await
        .expect("the event arrives despite a descendant holding the pipe");
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
    assert!(!alive, "descendant {pid} must die with the wait");
}
