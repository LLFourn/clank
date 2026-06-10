//! Shared integration-test helpers for the team-based
//! registration model (`teams-based-agent-registration`).
//!
//! Setup goes through [`TestEnv`] + [`register_team`]: build the
//! team via the REAL library cores, in a HOME distinct from the
//! repo — the dogfood path (`dogfood-init-setup-in-tests`). The
//! old repo-only raw-JSON helpers (`write_team_config` /
//! `add_reviewer`) were deleted once every test moved over.

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

/// Register a team THE REAL WAY — through the same library cores
/// the CLI handlers call, so the test setup and production share
/// one code path (the dogfood payoff of
/// `dogfood-init-setup-in-tests`). It writes BOTH scopes
/// (user-scope agents/team + the repo's team field), so `home`
/// MUST be a separate dir from `repo` — a test that set
/// `HOME=repo` would collide the two on one `.clank/config.json`.
///
/// Sequence mirrors what a user would run:
/// `clank agent add --global` (declare each agent) →
/// `clank team create/set-master/add` → `clank init --team`.
pub fn register_team(
    home: &Path,
    repo: &Path,
    master: &str,
    commit_reviewers: &[&str],
    gate_reviewers: &[&str],
) {
    use clank::cli::teams_config::{AgentDescription, ReviewKind};
    use clank_core::ids::AgentLabel;
    use clank_core::vocab::Tool;

    let desc = || AgentDescription {
        tool: Tool::Claude,
        launch: None,
        initial_prompt: None,
    };
    let lbl = |s: &str| AgentLabel::parse(s).unwrap();

    // Declare every agent in user-scope `agents`.
    for a in std::iter::once(master)
        .chain(commit_reviewers.iter().copied())
        .chain(gate_reviewers.iter().copied())
    {
        clank::cli::agent::declare_global_agent(home, &lbl(a), desc()).unwrap();
    }
    // Build the `default` team via the team cores.
    clank::cli::team::create_team(home, "default").unwrap();
    clank::cli::team::set_master(home, "default", master).unwrap();
    for r in commit_reviewers {
        clank::cli::team::add_member(home, "default", r, ReviewKind::Commit).unwrap();
    }
    for r in gate_reviewers {
        clank::cli::team::add_member(home, "default", r, ReviewKind::Gate).unwrap();
    }
    // Point the repo at the team.
    clank::cli::init::register_repo_team(home, repo, "default").unwrap();
}
