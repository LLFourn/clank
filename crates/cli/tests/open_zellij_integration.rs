//! Integration tests for `clank open zellij` (Phase 4 of
//! `clank-open-zellij`).
//!
//! All tests use `--print` mode so no real zellij is needed
//! in CI. The pinned `--print` semantics emit the composed
//! KDL on stdout and the would-be-spawned argv on stderr,
//! mirroring `clank diff --print`'s "argv on stdout + env on
//! stderr" convention.

use std::path::Path;
use std::process::Command;

use clank::cli::config::{DefaultAgent, RepoConfigFile};
use clank_core::ids::AgentLabel;
use clank_core::vocab::Role;

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

fn write_repo_agents(repo: &Path, agents: Vec<DefaultAgent>) {
    let file = RepoConfigFile {
        agents: Some(agents),
        ..Default::default()
    };
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(&file).unwrap(),
    )
    .unwrap();
}

fn agent(label: &str, role: Role) -> DefaultAgent {
    DefaultAgent {
        label: AgentLabel::parse(label).unwrap(),
        role,
        tool: None,
        launch: None,
    }
}

fn run_zellij(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.args(["open", "zellij", "--repo"])
        .arg(repo)
        .args(args)
        // Tests should never need a real zellij. Default to
        // NOT being inside a session unless the test sets it.
        .env_remove("ZELLIJ_SESSION_NAME")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .env("HOME", repo);
    cmd.output().expect("spawn clank open zellij")
}

// ─── Phase 1: dry-rename regression ────────────────────────────

#[test]
fn legacy_open_invocation_errors_with_subcommand_hint() {
    // `clank open <path>` (without the `dry` subcommand) must
    // fail. Clap's exact wording is whatever it is — we just
    // want the call to fail AND tell the user it's a subcommand
    // problem (so `--help` can lead them to `dry` / `zellij`).
    let dir = init_repo();
    let repo = dir.path();
    let out = Command::new(clank_bin())
        .args(["open", "/tmp/some/path"])
        .env("HOME", repo)
        .env_remove("ZELLIJ_SESSION_NAME")
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "legacy `clank open <path>` invocation must fail; got stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unrecognized subcommand") || stderr.contains("subcommand"),
        "error should surface the subcommand problem so --help leads the user to `dry`; got: {stderr}"
    );
    assert!(
        stderr.contains("--help") || stderr.contains("help"),
        "error should suggest --help for discovery; got: {stderr}"
    );
}

// ─── Phase 2: KDL composition ──────────────────────────────────

#[test]
fn open_zellij_print_emits_kdl_with_master_and_reviewers() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        vec![
            agent("alice", Role::Master),
            agent("bob", Role::Reviewer),
            agent("carol", Role::Reviewer),
        ],
    );

    let out = run_zellij(repo, &["--print"]);
    assert!(
        out.status.success(),
        "--print failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("name=\"alice (master)\""),
        "master pane missing; got:\n{stdout}"
    );
    assert!(
        stdout.contains("name=\"bob (reviewer)\""),
        "bob pane missing; got:\n{stdout}"
    );
    assert!(
        stdout.contains("name=\"carol (reviewer)\""),
        "carol pane missing; got:\n{stdout}"
    );
    assert!(stdout.contains("command \"clank\""));
    assert!(stdout.contains("args \"agent\" \"start\" \"alice\""));
    assert!(stdout.contains("args \"agent\" \"start\" \"bob\""));
    assert!(stdout.contains("args \"agent\" \"start\" \"carol\""));
}

#[test]
fn open_zellij_print_includes_tab_bar_and_status_bar_plugins() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    let out = run_zellij(repo, &["--print"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("plugin location=\"zellij:tab-bar\""),
        "tab-bar plugin missing; got:\n{stdout}"
    );
    assert!(
        stdout.contains("plugin location=\"zellij:status-bar\""),
        "status-bar plugin missing; got:\n{stdout}"
    );
}

#[test]
fn open_zellij_no_master_errors_with_suggestion() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        vec![agent("a", Role::Reviewer), agent("b", Role::Reviewer)],
    );

    let out = run_zellij(repo, &["--print"]);
    assert!(
        !out.status.success(),
        "no-master must error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("clank agent add") && stderr.contains("--role master"),
        "diagnostic should name the fix; got: {stderr}"
    );
}

#[test]
fn open_zellij_multiple_masters_errors_with_diagnostic() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        vec![
            agent("alice", Role::Master),
            agent("bob", Role::Master),
            agent("carol", Role::Reviewer),
        ],
    );

    let out = run_zellij(repo, &["--print"]);
    assert!(
        !out.status.success(),
        "multi-master must error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("alice") && stderr.contains("bob"),
        "multi-master diagnostic must list BOTH masters; got: {stderr}"
    );
    assert!(
        stderr.contains("clank agent set-role"),
        "diagnostic should suggest set-role; got: {stderr}"
    );
}

// ─── Phase 3: spawn vs --print ─────────────────────────────────

#[test]
fn open_zellij_no_zellij_session_errors_without_print() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    // No --print, no ZELLIJ_SESSION_NAME → must error.
    let out = run_zellij(repo, &[]);
    assert!(
        !out.status.success(),
        "missing zellij session must error without --print"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ZELLIJ_SESSION_NAME") && stderr.contains("--print"),
        "diagnostic should name the env var AND the --print escape; got: {stderr}"
    );
}

#[test]
fn open_zellij_no_zellij_session_succeeds_with_print() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    let out = run_zellij(repo, &["--print"]);
    assert!(
        out.status.success(),
        "--print should succeed even without ZELLIJ_SESSION_NAME; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("layout {"),
        "KDL should be on stdout; got: {stdout}"
    );
}

// ─── Order & spawn metadata ────────────────────────────────────

#[test]
fn open_zellij_reviewer_order_matches_declaration_order() {
    // Explicit non-alphabetical order so a BTreeMap-ordered
    // iteration would fail this test (per ruthless 7a58d12).
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        vec![
            agent("master", Role::Master),
            agent("bob", Role::Reviewer),
            agent("alice", Role::Reviewer),
            agent("codex", Role::Reviewer),
        ],
    );

    let out = run_zellij(repo, &["--print"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let bob_idx = stdout
        .find("name=\"bob (reviewer)\"")
        .expect("bob pane present");
    let alice_idx = stdout
        .find("name=\"alice (reviewer)\"")
        .expect("alice pane present");
    let codex_idx = stdout
        .find("name=\"codex (reviewer)\"")
        .expect("codex pane present");
    assert!(
        bob_idx < alice_idx && alice_idx < codex_idx,
        "panes must appear in declaration order (bob, alice, codex); got positions {bob_idx} / {alice_idx} / {codex_idx}"
    );
}

#[test]
fn open_zellij_tab_name_in_print_spawn_metadata() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    let out = run_zellij(repo, &["--print"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The basename of the tempdir is what becomes the tab name.
    let basename = repo.file_name().and_then(|s| s.to_str()).unwrap();
    assert!(
        stderr.contains(&format!("--name {basename}")),
        "spawn metadata must include --name <basename> ({basename}); got: {stderr}"
    );
    assert!(
        stderr.contains("--cwd"),
        "spawn metadata must include --cwd; got: {stderr}"
    );
    assert!(
        stderr.contains(&repo.display().to_string()),
        "spawn metadata must include the repo path; got: {stderr}"
    );
}
