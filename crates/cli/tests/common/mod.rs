//! Shared integration-test helpers for the team-based
//! registration model (`teams-based-agent-registration`).
//!
//! Registration is repo-scope only here: a `team` array of
//! fully-inline entries + a `promoted` master. This needs NO
//! user-scope config, so tests don't have to control `$HOME`.

#![allow(dead_code)]

use std::path::Path;

use serde_json::{Value, json};

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
