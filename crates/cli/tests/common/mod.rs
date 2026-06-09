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
/// team set up via [`register_team`] persists in user-scope
/// across every spawned `clank` invocation. Each test owns one;
/// `run()` helpers point `HOME` at `env.home()`.
///
/// `TestEnv` deliberately does NOT provide a `run()`/`cmd()` —
/// each test file's invocation needs differ (env scrubbing,
/// piped stdin, extra flags). It owns only the home+repo+team
/// scaffolding; the file keeps its own `run(&TestEnv, ...)`.
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

    /// A base `clank` command with `HOME` PRE-POINTED at this
    /// env's separate home dir. Always start hand-built commands
    /// from here so the team registered into `home()` is visible
    /// and `HOME` can't be forgotten — the footgun codex caught
    /// on 9b497a5, where a command that omitted `HOME` could fail
    /// for the wrong reason. Callers add `--repo`, args, and any
    /// `env_remove` scrubbing themselves (those vary per command).
    pub fn clank(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_clank"));
        cmd.env("HOME", self.home());
        cmd
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
