//! Integration tests for `clank agent list`.

use std::path::Path;
use std::process::Command;

fn clank_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clank")
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@test"]);
    git(path, &["config", "user.name", "test"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    dir
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

/// Write the repo-scope `agents` declaration via the typed
/// struct so schema changes type-check.
fn write_repo_agents(repo: &Path, agents: &[clank::cli::config::DefaultAgent]) {
    let file = clank::cli::config::RepoAgentsFile {
        agents: Some(agents.to_vec()),
    };
    let json = serde_json::to_string_pretty(&file).unwrap();
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(repo.join(".clank/config.json"), json).unwrap();
}

fn write_bound_skeleton(
    repo: &Path,
    label: &str,
    auto_mode: clank_core::vocab::AutoMode,
    tool: clank_core::vocab::Tool,
    session_id: &str,
) {
    let cfg = clank_core::agent_config::AgentConfig {
        auto_mode,
        role: clank_core::vocab::Role::Reviewer, // unused; declaration owns
        wfw_timeout: None,
        session: Some(clank_core::agent_config::Session {
            id: clank_core::ids::SessionId::parse(session_id).unwrap(),
            tool,
            updated_at: "2026-06-04T12:00:00Z".to_string(),
        }),
        launch: None,
    };
    clank::agent_store::save_agent_config(
        repo,
        &clank_core::ids::AgentLabel::parse(label).unwrap(),
        &cfg,
    )
    .unwrap();
}

fn agent_decl(label: &str, role: clank_core::vocab::Role) -> clank::cli::config::DefaultAgent {
    clank::cli::config::DefaultAgent {
        label: clank_core::ids::AgentLabel::parse(label).unwrap(),
        role,
        tool: None,
        launch: None,
    }
}

fn run_list(repo: &Path, json: bool) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("agent").arg("list").arg("--repo").arg(repo);
    // Isolate HOME to the temp repo so the developer's
    // user-scope `~/.clank/config.json#default_agents` doesn't
    // leak into the test. Ruthless caught the gap on 841cdf2:
    // `agent_list_empty_repo_succeeds` failed on a dev machine
    // with global default_agents configured because the test
    // saw them via the merged declaration.
    cmd.env("HOME", repo);
    if json {
        cmd.arg("--json");
    }
    cmd.output().expect("spawn clank agent list")
}

#[test]
fn agent_list_empty_repo_succeeds() {
    let dir = init_repo();
    let out = run_list(dir.path(), false);
    assert!(
        out.status.success(),
        "agent list failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        stdout.contains("no agents") || stdout.trim().is_empty(),
        "expected empty-list message; got `{stdout}`"
    );
}

#[test]
fn agent_list_shows_bound_and_unbound() {
    let dir = init_repo();
    let repo = dir.path();
    // Declaration is the source of truth post
    // agent-add-cli-and-repo-scope; skeletons hold session state.
    write_repo_agents(
        repo,
        &[
            agent_decl("claude", clank_core::vocab::Role::Master),
            agent_decl("codex", clank_core::vocab::Role::Reviewer),
            agent_decl("ruthless", clank_core::vocab::Role::Reviewer),
        ],
    );
    // Bound master (session state in skeleton).
    write_bound_skeleton(
        repo,
        "claude",
        clank_core::vocab::AutoMode::Off,
        clank_core::vocab::Tool::Claude,
        "11111111-1111-1111-1111-111111111111",
    );
    // Bound reviewer.
    write_bound_skeleton(
        repo,
        "codex",
        clank_core::vocab::AutoMode::On,
        clank_core::vocab::Tool::Codex,
        "22222222-2222-2222-2222-222222222222",
    );
    // Unbound seeded reviewer (no skeleton — appears unbound).

    let out = run_list(repo, false);
    assert!(out.status.success(), "agent list failed");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    // Header
    assert!(
        stdout.contains("LABEL") && stdout.contains("BOUND"),
        "missing header; got `{stdout}`"
    );
    // All three labels appear.
    for label in &["claude", "codex", "ruthless"] {
        assert!(
            stdout.contains(label),
            "expected `{label}` in output; got `{stdout}`"
        );
    }
    // Unbound marker shows up.
    assert!(
        stdout.contains(" NO "),
        "expected NO marker for unbound reviewer; got `{stdout}`"
    );
    // Bound marker too.
    assert!(
        stdout.contains(" yes "),
        "expected yes marker for bound entries; got `{stdout}`"
    );
}

#[test]
fn agent_list_json_schema() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        &[
            agent_decl("claude", clank_core::vocab::Role::Master),
            agent_decl("ruthless", clank_core::vocab::Role::Reviewer),
        ],
    );
    write_bound_skeleton(
        repo,
        "claude",
        clank_core::vocab::AutoMode::Off,
        clank_core::vocab::Tool::Claude,
        "11111111-1111-1111-1111-111111111111",
    );

    let out = run_list(repo, true);
    assert!(out.status.success(), "agent list --json failed");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("output should be valid JSON");
    let arr = parsed.as_array().expect("output should be an array");
    assert_eq!(arr.len(), 2);
    // Master first (sort order).
    assert_eq!(arr[0]["label"], "claude");
    assert_eq!(arr[0]["role"], "master");
    assert_eq!(arr[0]["bound"], true);
    assert_eq!(arr[0]["tool"], "claude");
    assert!(arr[0]["session_id"].is_string());
    // Then reviewer.
    assert_eq!(arr[1]["label"], "ruthless");
    assert_eq!(arr[1]["role"], "reviewer");
    assert_eq!(arr[1]["bound"], false);
    assert!(arr[1]["tool"].is_null());
    assert!(arr[1]["session_id"].is_null());
}

#[test]
fn agent_list_omits_orphan_skeleton() {
    // Codex review of eef4c49: list reads the declaration, NOT
    // the skeleton dirs. A skeleton without a declaration entry
    // is an orphan (doctor surfaces it separately); it must not
    // appear in `clank agent list`.
    let dir = init_repo();
    let repo = dir.path();
    // Declaration registers only alice.
    write_repo_agents(
        repo,
        &[agent_decl("alice", clank_core::vocab::Role::Reviewer)],
    );
    // Orphan skeleton for `removed` — present on disk but not in
    // declaration.
    write_bound_skeleton(
        repo,
        "removed",
        clank_core::vocab::AutoMode::Off,
        clank_core::vocab::Tool::Claude,
        "33333333-3333-3333-3333-333333333333",
    );

    let out = run_list(repo, true);
    assert!(out.status.success(), "agent list failed");
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let arr = parsed.as_array().unwrap();
    let labels: Vec<&str> = arr.iter().map(|r| r["label"].as_str().unwrap()).collect();
    assert_eq!(
        labels,
        vec!["alice"],
        "orphan must not appear; got {labels:?}"
    );
}

#[test]
fn agent_list_shows_declared_agents_even_without_skeleton() {
    // Codex review of eef4c49: declared agents without skeletons
    // appear as unbound. The OLD skeleton-scanning implementation
    // dropped them entirely.
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        &[agent_decl(
            "declared-only",
            clank_core::vocab::Role::Reviewer,
        )],
    );
    // No skeleton for `declared-only`.

    let out = run_list(repo, true);
    assert!(out.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["label"], "declared-only");
    assert_eq!(arr[0]["bound"], false);
}

#[test]
fn agent_list_fails_on_malformed_agent_config() {
    // After agent-add-cli-and-repo-scope, `clank agent list` reads
    // the merged declaration. A malformed `.clank/config.json`
    // (the declaration file) must fail closed — silently dropping
    // would hide misconfigured agents from the user's "what's
    // registered" view.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/config.json", "{ not json");

    let out = run_list(repo, false);
    assert!(
        !out.status.success(),
        "agent list must fail on malformed declaration; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
