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
fn open_dry_rejects_unknown_repo_flag() {
    // Codex 30a0194: `clank open dry --repo <path>` was
    // accepted by clap but silently ignored by run_dry. The
    // path argument carries the repo info already (the
    // inspector derives the repo from the path itself), so
    // there is no `--repo` flag on `open dry`. Lock this in:
    // passing `--repo` must fail at the clap layer, NOT
    // succeed silently.
    let dir = init_repo();
    let repo = dir.path();
    let out = Command::new(clank_bin())
        .args(["open", "dry", "--repo", "/tmp", "/tmp/some/path"])
        .env("HOME", repo)
        .env_remove("ZELLIJ_SESSION_NAME")
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "open dry --repo must be rejected (silent no-op flags mislead editor integrations); \
         got stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--repo") || stderr.contains("unexpected") || stderr.contains("argument"),
        "error should mention the unknown flag; got: {stderr}"
    );
}

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
fn open_zellij_print_mode_emits_kdl_without_writing_file() {
    // --print is inspection-only: emit KDL on stdout + the
    // would-be-spawned argv on stderr, but DO NOT write the
    // layout file. (Codex 361b104 plan refinement: file-write
    // is a non-print side effect.)
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    let out = run_zellij(repo, &["--print"]);
    assert!(
        out.status.success(),
        "--print should succeed without zellij installed; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("layout {"),
        "KDL should be on stdout; got: {stdout}"
    );
    assert!(
        !repo.join(".clank/zellij/layout.kdl").exists(),
        "--print mode must NOT write the layout file"
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
fn open_zellij_tab_name_in_kdl_and_layout_path_in_spawn_metadata() {
    // Tab name now lives in the KDL (`tab name="<basename>"`)
    // rather than a `--name` argv flag. The spawn argv on
    // stderr is `zellij --layout <path>` only.
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    let out = run_zellij(repo, &["--print"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let basename = repo.file_name().and_then(|s| s.to_str()).unwrap();
    assert!(
        stdout.contains(&format!("tab name=\"{basename}\"")),
        "KDL should contain `tab name=\"<basename>\"`; got:\n{stdout}"
    );
    // macOS canonicalizes /tmp → /private/tmp, so we can't
    // predict the exact prefix. Verify shape only.
    assert!(
        stderr.contains("spawn: zellij --layout"),
        "spawn metadata must be `zellij --layout <path>`; got: {stderr}"
    );
    assert!(
        stderr.contains(".clank/zellij/layout.kdl"),
        "spawn line must reference the layout path; got: {stderr}"
    );
}

#[test]
fn open_zellij_writes_layout_file_under_clank_dir() {
    // Non-print: layout file is written under
    // <repo>/.clank/zellij/layout.kdl. The spawn itself will
    // fail (no zellij in CI / no session), but the file write
    // happens before the spawn so we can assert on the
    // post-state regardless.
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    let _ = run_zellij(repo, &[]); // ignore status — zellij absent in CI
    let layout_path = repo.join(".clank/zellij/layout.kdl");
    assert!(
        layout_path.exists(),
        "layout.kdl should be written at {} even if spawn fails",
        layout_path.display()
    );
    let body = std::fs::read_to_string(&layout_path).unwrap();
    assert!(
        body.contains("layout {"),
        "file should be valid KDL; got:\n{body}"
    );
    let basename = repo.file_name().and_then(|s| s.to_str()).unwrap();
    assert!(
        body.contains(&format!("tab name=\"{basename}\"")),
        "file should have the tab name; got:\n{body}"
    );
}

#[test]
fn open_zellij_adds_zellij_dir_to_gitignore_idempotently() {
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(repo, vec![agent("alice", Role::Master)]);

    // First invocation.
    let _ = run_zellij(repo, &[]);
    let gitignore = repo.join(".clank/.gitignore");
    let body1 = std::fs::read_to_string(&gitignore).unwrap();
    assert!(
        body1.lines().any(|l| l.trim() == "/zellij/"),
        "gitignore should contain `/zellij/`; got:\n{body1}"
    );

    // Second invocation — must NOT add a duplicate entry.
    let _ = run_zellij(repo, &[]);
    let body2 = std::fs::read_to_string(&gitignore).unwrap();
    assert_eq!(
        body2.matches("/zellij/").count(),
        1,
        "second invocation must not duplicate /zellij/; got:\n{body2}"
    );
}

#[test]
fn open_zellij_pane_commands_pin_repo_via_absolute_path() {
    // Codex 361b104: when `clank open zellij --repo <abs-path>`
    // is invoked from a different cwd, the spawned zellij
    // session's cwd doesn't match the repo. Every pane command
    // must pin `--repo <abs-path>` so the resolved repo is
    // unambiguous.
    let dir = init_repo();
    let repo = dir.path();
    write_repo_agents(
        repo,
        vec![agent("alice", Role::Master), agent("bob", Role::Reviewer)],
    );

    // Invoke from a DIFFERENT cwd (the tempdir's parent, or
    // any path that isn't the repo).
    let cwd = std::env::temp_dir();
    let out = Command::new(clank_bin())
        .current_dir(&cwd)
        .args(["open", "zellij", "--repo"])
        .arg(repo)
        .arg("--print")
        .env_remove("ZELLIJ_SESSION_NAME")
        .env("HOME", repo)
        .output()
        .expect("spawn clank open zellij");
    assert!(
        out.status.success(),
        "--print from outside repo should succeed; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Whatever path clank resolved for --repo is what shows up
    // in the pane args. macOS canonicalization (/tmp → /private/tmp)
    // means we can't assert on `repo.display()` directly. The
    // shape we care about: the args have `--repo` pinned to an
    // absolute path (not the cwd we invoked from).
    let cwd_str = cwd.display().to_string();
    for label in ["alice", "bob"] {
        let pat = format!("args \"agent\" \"start\" \"{label}\" \"--repo\" \"");
        let pos = stdout
            .find(&pat)
            .unwrap_or_else(|| panic!("{label}'s args must contain `--repo` pin; got:\n{stdout}"));
        let after = &stdout[pos + pat.len()..];
        let close = after.find('"').unwrap();
        let path_in_args = &after[..close];
        assert!(
            path_in_args.starts_with('/'),
            "{label}'s --repo must be an absolute path, not relative; got: {path_in_args}"
        );
        assert!(
            path_in_args != cwd_str,
            "{label}'s --repo must NOT be the invocation cwd ({cwd_str}) — that's the bug we're guarding against; got: {path_in_args}"
        );
        // Sanity: it should at least contain the tempdir's basename.
        let basename = repo.file_name().and_then(|s| s.to_str()).unwrap();
        assert!(
            path_in_args.contains(basename),
            "{label}'s --repo should reference the actual repo (basename {basename}); got: {path_in_args}"
        );
    }
}
