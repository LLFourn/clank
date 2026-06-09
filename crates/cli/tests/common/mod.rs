//! Shared integration-test helpers for the team-based
//! registration model (`teams-based-agent-registration`).
//!
//! Registration is repo-scope only here: a `team` array of
//! fully-inline entries + a `promoted` master. This needs NO
//! user-scope config, so tests don't have to control `$HOME`.

#![allow(dead_code)]

use std::path::Path;

use serde_json::{Value, json};

/// Register a team THE REAL WAY — through the same library cores
/// the CLI handlers call, so the test setup and production share
/// one code path (the dogfood payoff of
/// `dogfood-init-setup-in-tests`). Unlike [`write_team_config`]
/// (repo-only raw JSON), this writes BOTH scopes, so `home` MUST
/// be a separate dir from `repo` (a test that sets `HOME=repo`
/// can't use this — user-scope and repo-scope would collide on
/// the same `.clank/config.json`).
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

/// Write `<repo>/.clank/config.json` with a team composed of
/// inline local entries + a `promoted` master. The master is
/// added as an inline commit entry and then promoted (the
/// resolver demotes it out of the reviewer list and installs it
/// as master). Reviewers are inline commit/gate entries.
///
/// Preserves any existing top-level keys (review, hooks, diff)
/// already in the repo config.
pub fn write_team_config(
    repo: &Path,
    master: &str,
    commit_reviewers: &[&str],
    gate_reviewers: &[&str],
) {
    let path = repo.join(".clank/config.json");
    let mut root: Value = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    };
    let obj = root.as_object_mut().expect("config root is an object");

    let mut entries: Vec<Value> = Vec::new();
    entries.push(json!({"label": master, "tool": "claude", "review": "commit"}));
    for r in commit_reviewers {
        entries.push(json!({"label": r, "tool": "claude", "review": "commit"}));
    }
    for r in gate_reviewers {
        entries.push(json!({"label": r, "tool": "claude", "review": "gate"}));
    }
    obj.insert("team".to_string(), Value::Array(entries));
    obj.insert("promoted".to_string(), json!(master));

    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(&root).unwrap()).unwrap();
}

/// Append a single commit-tier reviewer to an existing repo team
/// config (read-modify-write of the `team` array). The repo must
/// already have a `team` array (call [`write_team_config`]
/// first).
pub fn add_reviewer(repo: &Path, label: &str) {
    let path = repo.join(".clank/config.json");
    let mut root: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("repo config must exist; call write_team_config"),
    )
    .expect("repo config parses");
    let obj = root.as_object_mut().expect("config root is an object");
    let arr = obj
        .get_mut("team")
        .and_then(|t| t.as_array_mut())
        .expect("team must be an array; call write_team_config first");
    arr.push(json!({"label": label, "tool": "claude", "review": "commit"}));
    std::fs::write(&path, serde_json::to_string_pretty(&root).unwrap()).unwrap();
}
