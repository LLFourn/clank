//! Shared integration-test helpers for the roster model
//! (`repo-agents-no-team`).
//!
//! Setup goes through [`TestEnv`] + [`register_team`]: build the
//! repo ROSTER via the REAL library cores (`clank agent add` /
//! `clank agent set-master`), in a HOME distinct from the repo.
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
/// `clank agent set-master <master>`.
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
