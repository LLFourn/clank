//! Integration tests for `clank open zellij` (Phase 4 of
//! `clank-open-zellij`).
//!
//! All tests use `--print` mode so no real zellij is needed
//! in CI. The pinned `--print` semantics emit the composed
//! KDL on stdout and the would-be-spawned argv on stderr,
//! mirroring `clank diff --print`'s "argv on stdout + env on
//! stderr" convention.

mod common;

use std::path::Path;
use std::process::Command;

fn clank_bin() -> &'static str {
    env!("CARGO_BIN_EXE_clank")
}

fn init_repo() -> common::TestEnv {
    common::TestEnv::init()
}

/// Write a repo `team` array of reviewers WITHOUT a master
/// (no `promoted`) — used to exercise the no-master error path.
fn write_repo_reviewers_no_master(repo: &Path, reviewers: &[&str]) {
    let entries: Vec<serde_json::Value> = reviewers
        .iter()
        .map(|r| serde_json::json!({"label": r, "tool": "claude", "review": "commit"}))
        .collect();
    let cfg = serde_json::json!({ "team": entries });
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(
        repo.join(".clank/config.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
}

fn run_zellij(env: &common::TestEnv, args: &[&str]) -> std::process::Output {
    let mut cmd = env.clank();
    cmd.args(["open", "zellij", "--repo"])
        .arg(env.repo())
        .args(args)
        // Tests should never need a real zellij. Default to
        // NOT being inside a session unless the test sets it.
        .env_remove("ZELLIJ_SESSION_NAME")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT");
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
    let home = tempfile::tempdir().expect("isolated test HOME");
    let out = Command::new(clank_bin())
        .args(["open", "dry", "--repo", "/tmp", "/tmp/some/path"])
        .env("HOME", home.path())
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
    let home = tempfile::tempdir().expect("isolated test HOME");
    let out = Command::new(clank_bin())
        .args(["open", "/tmp/some/path"])
        .env("HOME", home.path())
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
    let env = init_repo();
    env.register_team("alice", &["bob", "carol"], &[]);

    let out = run_zellij(&env, &["--print"]);
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
    let env = init_repo();
    env.register_team("alice", &[], &[]);

    let out = run_zellij(&env, &["--print"]);
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
    let env = init_repo();
    // A team with reviewers but no designated master
    // (`teams-based-agent-registration`): resolution fails with
    // the NoMaster error.
    write_repo_reviewers_no_master(env.repo(), &["a", "b"]);

    let out = run_zellij(&env, &["--print"]);
    assert!(
        !out.status.success(),
        "no-master must error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no master") || stderr.contains("set-master") || stderr.contains("promote"),
        "diagnostic should name the master-designation fix; got: {stderr}"
    );
}

// ─── Phase 3: spawn vs --print ─────────────────────────────────

#[test]
fn open_zellij_print_mode_emits_kdl_without_writing_file() {
    // --print is inspection-only: emit KDL on stdout + the
    // would-be-spawned argv on stderr, but DO NOT write the
    // layout file. (Codex 361b104 plan refinement: file-write
    // is a non-print side effect.)
    let env = init_repo();
    env.register_team("alice", &[], &[]);

    let out = run_zellij(&env, &["--print"]);
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
        !env.repo().join(".clank/zellij/layout.kdl").exists(),
        "--print mode must NOT write the layout file"
    );
}

// ─── Order & spawn metadata ────────────────────────────────────

#[test]
fn open_zellij_reviewer_order_matches_declaration_order() {
    // Explicit non-alphabetical order so a BTreeMap-ordered
    // iteration would fail this test (per ruthless 7a58d12).
    let env = init_repo();
    env.register_team("master", &["bob", "alice", "codex"], &[]);

    let out = run_zellij(&env, &["--print"]);
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
    let env = init_repo();
    env.register_team("alice", &[], &[]);

    let out = run_zellij(&env, &["--print"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let basename = env.repo().file_name().and_then(|s| s.to_str()).unwrap();
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
    let env = init_repo();
    env.register_team("alice", &[], &[]);

    let _ = run_zellij(&env, &[]); // ignore status — zellij absent in CI
    let layout_path = env.repo().join(".clank/zellij/layout.kdl");
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
    let basename = env.repo().file_name().and_then(|s| s.to_str()).unwrap();
    assert!(
        body.contains(&format!("tab name=\"{basename}\"")),
        "file should have the tab name; got:\n{body}"
    );
}

#[test]
fn open_zellij_adds_zellij_dir_to_gitignore_idempotently() {
    let env = init_repo();
    env.register_team("alice", &[], &[]);

    // First invocation.
    let _ = run_zellij(&env, &[]);
    let gitignore = env.repo().join(".clank/.gitignore");
    let body1 = std::fs::read_to_string(&gitignore).unwrap();
    assert!(
        body1.lines().any(|l| l.trim() == "/zellij/"),
        "gitignore should contain `/zellij/`; got:\n{body1}"
    );

    // Second invocation — must NOT add a duplicate entry.
    let _ = run_zellij(&env, &[]);
    let body2 = std::fs::read_to_string(&gitignore).unwrap();
    assert_eq!(
        body2.matches("/zellij/").count(),
        1,
        "second invocation must not duplicate /zellij/; got:\n{body2}"
    );
}

#[test]
fn open_zellij_panes_set_cwd_to_repo_so_tool_launches_in_repo() {
    // Codex 8075d43: --repo on `clank agent start` only fixes
    // clank-side config resolution. The exec'd tool (e.g.
    // `claude --resume <session>`) inherits process cwd. To
    // ensure the tool launches IN the repo, each pane's KDL
    // sets `cwd="<repo>"`. Verify from a DIFFERENT invocation
    // cwd so we know we're not accidentally testing pwd-leak.
    let env = init_repo();
    env.register_team("alice", &["bob"], &[]);

    let cwd = std::env::temp_dir();
    let out = env
        .clank()
        .current_dir(&cwd)
        .args(["open", "zellij", "--repo"])
        .arg(env.repo())
        .arg("--print")
        .env_remove("ZELLIJ_SESSION_NAME")
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Each pane has its own cwd= attribute. macOS /tmp →
    // /private/tmp canonicalization means we look for the
    // pattern shape, not exact match.
    for label in ["alice", "bob"] {
        let role = if label == "alice" {
            "master"
        } else {
            "reviewer"
        };
        let pat = format!("name=\"{label} ({role})\" cwd=\"");
        let pos = stdout
            .find(&pat)
            .unwrap_or_else(|| panic!("{label}'s pane must have cwd= attribute; got:\n{stdout}"));
        let after = &stdout[pos + pat.len()..];
        let close = after.find('"').unwrap();
        let cwd_value = &after[..close];
        assert!(
            cwd_value.starts_with('/'),
            "{label}'s cwd must be an absolute path; got: {cwd_value}"
        );
        let basename = env.repo().file_name().and_then(|s| s.to_str()).unwrap();
        assert!(
            cwd_value.contains(basename),
            "{label}'s cwd must reference the repo (basename {basename}), \
             not the invocation cwd; got: {cwd_value}"
        );
    }
}

#[test]
fn open_zellij_pane_commands_pin_repo_via_absolute_path() {
    // Codex 361b104: when `clank open zellij --repo <abs-path>`
    // is invoked from a different cwd, the spawned zellij
    // session's cwd doesn't match the repo. Every pane command
    // must pin `--repo <abs-path>` so the resolved repo is
    // unambiguous.
    let env = init_repo();
    env.register_team("alice", &["bob"], &[]);

    // Invoke from a DIFFERENT cwd (the tempdir's parent, or
    // any path that isn't the repo).
    let cwd = std::env::temp_dir();
    let out = env
        .clank()
        .current_dir(&cwd)
        .args(["open", "zellij", "--repo"])
        .arg(env.repo())
        .arg("--print")
        .env_remove("ZELLIJ_SESSION_NAME")
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
        let basename = env.repo().file_name().and_then(|s| s.to_str()).unwrap();
        assert!(
            path_in_args.contains(basename),
            "{label}'s --repo should reference the actual repo (basename {basename}); got: {path_in_args}"
        );
    }
}

// ─── document-clank-open-zellij-new-tab ─────────────────────────

#[test]
fn clank_open_zellij_help_documents_auto_detect() {
    // Plan: document-clank-open-zellij-new-tab. The doc must tell
    // users that `clank open zellij` opens a new tab when run
    // inside a zellij session and a new session otherwise — both
    // behaviors flow from `zellij --layout` natively.
    let out = Command::new(clank_bin())
        .args(["open", "zellij", "--help"])
        .output()
        .expect("spawn clank open zellij --help");
    assert!(
        out.status.success(),
        "--help should exit 0; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("new tab"),
        "long_about must mention `new tab` (inside-session behavior); got: {stdout}"
    );
    assert!(
        stdout.contains("new session"),
        "long_about must mention `new session` (outside-session behavior); got: {stdout}"
    );
}

#[test]
fn clank_open_help_does_not_claim_action_new_tab() {
    // Plan: document-clank-open-zellij-new-tab. The pre-plan
    // variant docstring at mod.rs:194-197 claimed "Spawn the tab
    // via `zellij action new-tab`" — that command path was
    // discussed in early drafts but never implemented (the real
    // spawn at open_zellij.rs:37 uses `--layout`). Defend against
    // regression to the wrong description on both surfaces.
    for (label, args) in [
        ("clank open --help", vec!["open", "--help"]),
        ("clank open zellij --help", vec!["open", "zellij", "--help"]),
    ] {
        let out = Command::new(clank_bin())
            .args(&args)
            .output()
            .unwrap_or_else(|_| panic!("spawn {label}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stdout.contains("action new-tab"),
            "{label} must NOT claim `action new-tab` (that was never the implementation); got: {stdout}"
        );
    }
}
