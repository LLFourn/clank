//! Shared integration-test helpers for the roster model
//! (`repo-agents-no-team`).
//!
//! Setup goes through [`TestEnv`] + [`register_team`]: build the
//! repo ROSTER via the REAL library cores (`clank agent add` /
//! `clank agent promote`), in a HOME distinct from the repo.
//! The repo's `agents` IS the operating team — there's no separate
//! `team` field.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// A git repo with its OWN HOME, distinct from the repo dir, so a
/// team set up via [`register_team`] persists in user-scope.
/// Tests pass `env.home()` / `env.repo()` to the library cores
/// they call in-process — no test executes the clank binary
/// (clean break, lloyd 2026-06-10; behavior coverage lives in
/// each `cli::*` module's unit tests).
pub struct TestEnv {
    pub home: tempfile::TempDir,
    pub repo: tempfile::TempDir,
}

impl TestEnv {
    /// `git init` a fresh repo (main branch, test identity, no
    /// gpg) alongside a separate empty HOME.
    pub fn init() -> Self {
        let home = tempfile::tempdir().expect("home tempdir");
        let repo = tempfile::tempdir().expect("repo tempdir");
        let p = repo.path();
        git(p, &["init", "--quiet", "--initial-branch=main"]);
        git(p, &["config", "user.email", "test@test"]);
        git(p, &["config", "user.name", "test"]);
        git(p, &["config", "commit.gpgsign", "false"]);
        Self { home, repo }
    }

    pub fn home(&self) -> &Path {
        self.home.path()
    }

    pub fn repo(&self) -> &Path {
        self.repo.path()
    }

    /// Register a `default` team via the real cores (see
    /// [`register_team`]). Call once per env.
    pub fn register_team(&self, master: &str, commit_reviewers: &[&str], gate_reviewers: &[&str]) {
        register_team(
            self.home(),
            self.repo(),
            master,
            commit_reviewers,
            gate_reviewers,
        );
    }
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("spawn git");
    assert!(status.success(), "git {args:?} failed");
}

/// Build a repo ROSTER THE REAL WAY — through the same library
/// cores the CLI handlers call, so test setup and production share
/// one code path. `home` is accepted for signature stability (some
/// callers build a user-scope library too) but the roster lives in
/// the repo config; the two MUST be separate dirs.
///
/// Sequence mirrors what a user would run on a fresh repo:
/// `clank agent add <name> --tool …` for each agent, then
/// `clank agent promote <master>`.
pub fn register_team(
    home: &Path,
    repo: &Path,
    master: &str,
    commit_reviewers: &[&str],
    gate_reviewers: &[&str],
) {
    let _ = home;
    use clank::cli::teams_config::{AgentDescription, RosterRole};
    use clank_core::ids::AgentLabel;
    use clank_core::vocab::Tool;

    let desc = || AgentDescription {
        tool: Tool::Claude,
        launch: None,
        initial_prompt: None,
    };
    let lbl = |s: &str| AgentLabel::parse(s).unwrap();

    // Add the master + reviewers to the repo roster (inline
    // definitions), then designate the master.
    clank::cli::agent::add_repo_roster_agent(repo, &lbl(master), desc(), RosterRole::Commit)
        .unwrap();
    for r in commit_reviewers {
        clank::cli::agent::add_repo_roster_agent(repo, &lbl(r), desc(), RosterRole::Commit)
            .unwrap();
    }
    for r in gate_reviewers {
        clank::cli::agent::add_repo_roster_agent(repo, &lbl(r), desc(), RosterRole::Gate).unwrap();
    }
    clank::cli::agent::set_repo_master(repo, &lbl(master)).unwrap();
}

// ---------------------------------------------------------------
// Wait-test timing support (remove-wait-timeout).
//
// `clank wait` no longer has a timeout, so "it stays parked" can no
// longer be asserted by letting a clock expire. It is now asserted
// by SAMPLING: spawn the wait, look after a window, abort. The
// window is the delicate part.
//
// A watcher attach costs ~2ms from an ordinary binary but 7.5-11.3s
// under libtest on some machines (measured; the mechanism inside the
// harness was never identified). So a fixed 2s window would sample a
// wait that has not begun to park — and would pass just as happily
// for a wait wedged in setup, which is exactly the bug worth
// catching. Every window here is therefore derived from a real
// measurement taken once per test binary.
// ---------------------------------------------------------------

use std::time::{Duration, Instant};

/// This environment's watcher-attach cost, measured once per test
/// binary against a real repo.
///
/// Note for anyone reading a slow run: attach is SYNCHRONOUS, so a
/// spawned wait occupies a runtime worker for its whole duration.
/// Tests here therefore need more workers than the two a wait plus
/// the test itself would suggest — with too few, every worker sits
/// in a blocking attach and the test's own `timeout` future is never
/// polled, so its deadline cannot fire and the test hangs instead of
/// failing.
pub fn attach_cost(repo: &Path) -> Duration {
    static COST: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *COST.get_or_init(|| {
        let (tx, _rx) = std::sync::mpsc::channel::<()>();
        let t = Instant::now();
        let watcher = clank::repo_watch::RepoStateWatcher::attach(repo, true, tx)
            .expect("attach a watcher to the fixture repo");
        drop(watcher);
        t.elapsed()
    })
}

/// How long to let a wait run before concluding it is parked.
pub fn parked_window(repo: &Path) -> Duration {
    attach_cost(repo) + Duration::from_secs(2)
}

/// Deadline for a race that must RESOLVE — generous enough to cover
/// setup plus the work itself. Linear in the measurement, not a
/// multiple of it: attach has been observed anywhere from 2ms to
/// ~20s here, and multiplying the high end turns a 4-test binary
/// into minutes of waiting for deadlines nothing is expected to
/// reach.
pub fn race_deadline(repo: &Path) -> Duration {
    attach_cost(repo) + Duration::from_secs(30)
}

/// Attaching watchers concurrently is what made these binaries flaky
/// (four at once pushed every deadline past its limit). Hold this for
/// the body of any test that spawns a wait.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// Assert a wait does NOT resolve: spawn it, sample after
/// [`parked_window`], then abort. Returns nothing — a wait that
/// parked has no result to inspect.
pub async fn assert_stays_parked(repo: &Path, args: clank::cli::WaitArgs, what: &str) {
    let handle = tokio::spawn(clank::cli::wait::run(args));
    tokio::time::sleep(parked_window(repo)).await;
    let finished = handle.is_finished();
    handle.abort();
    assert!(!finished, "{what}: the wait resolved instead of parking");
}
