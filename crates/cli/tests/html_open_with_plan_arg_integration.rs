//! Integration tests for `clank html open <plan>` shortcut
//! (Phase 3 of `html-per-plan-landing-pages`).
//!
//! All tests use `--print-path` so no real browser launches.
//! The flag mirrors the `--print` convention from
//! `clank agent start`, `clank diff`, `clank open zellij` —
//! lets CI assert on the resolved target without browser
//! mocking.

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
    write(path, "README.md", "seed\n");
    git(path, &["add", "-A"]);
    git(path, &["commit", "--quiet", "-m", "seed"]);
    dir
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

fn intro_plan(repo: &Path, stem: &str) {
    write(
        repo,
        &format!(".clank/plans/{stem}.md"),
        &format!("# {stem}\n"),
    );
    git(repo, &["add", "-A"]);
    git(
        repo,
        &["commit", "--quiet", "-m", &format!("[{stem}] intro")],
    );
}

/// `--repo` is on `clank html` (top-level), not on `clank html
/// open` (subcommand). Order matters: `clank html --repo X open
/// foo --print-path`. The test runner inserts `--repo X` between
/// `html` and the subcommand args.
fn run(repo: &Path, args: &[&str]) -> std::process::Output {
    // args[0] is expected to be the top-level subcommand
    // (e.g. "html"). Insert `--repo <repo>` AFTER it.
    let mut cmd = Command::new(clank_bin());
    cmd.arg(args[0])
        .arg("--repo")
        .arg(repo)
        .args(&args[1..])
        .env("HOME", repo);
    cmd.output().expect("spawn clank")
}

/// `--print-path` writes the wrote-line + the resolved target
/// each on their own line. The resolved target is the LAST line.
fn target_line(stdout: &str) -> String {
    stdout.lines().last().unwrap_or("").trim().to_string()
}

// ── Phase 3 tests ─────────────────────────────────────────────

#[test]
fn html_open_with_active_plan_arg_prints_per_plan_path() {
    let dir = init_repo();
    let repo = dir.path();
    intro_plan(repo, "foo");

    let out = run(repo, &["html", "open", "foo", "--print-path"]);
    assert!(
        out.status.success(),
        "html open foo --print-path failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let target = target_line(&stdout);
    assert!(
        target.ends_with("plan/foo.html"),
        "target must end with `plan/foo.html`; got: `{target}`\nfull stdout:\n{stdout}"
    );
    // The page must actually exist on disk.
    assert!(
        Path::new(&target).exists(),
        "rendered file must exist at {target}"
    );
}

#[test]
fn html_open_with_finished_plan_arg_prints_per_plan_path() {
    let dir = init_repo();
    let repo = dir.path();
    intro_plan(repo, "bar");
    // Finalize bar: move .clank/plans/bar.md → .clank/finished/bar.md.
    std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
    std::fs::rename(
        repo.join(".clank/plans/bar.md"),
        repo.join(".clank/finished/bar.md"),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[bar] finish"]);

    let out = run(repo, &["html", "open", "bar", "--print-path"]);
    assert!(
        out.status.success(),
        "html open bar --print-path failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let target = target_line(&stdout);
    assert!(
        target.ends_with("plan/bar.html"),
        "finished plans live in the SAME `plan/` output dir as active; got: `{target}`"
    );
    assert!(Path::new(&target).exists(), "rendered file must exist");
}

#[test]
fn html_open_with_unknown_plan_errors() {
    let dir = init_repo();
    let repo = dir.path();
    intro_plan(repo, "foo");

    let out = run(repo, &["html", "open", "nonexistent", "--print-path"]);
    assert!(
        !out.status.success(),
        "unknown plan must error; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Diagnostic from `plan_resolve::resolve_plan`: names the
    // missing plan + lists candidates. Phrasing-agnostic so a
    // future wording tweak doesn't break the test — the failure
    // class ("plan not found") is what we defend.
    assert!(
        stderr.contains("nonexistent") && stderr.contains("not found in repo"),
        "stderr must surface the missing plan name + failure class; got: {stderr}"
    );
    // Available candidates should be hinted.
    assert!(
        stderr.contains("foo"),
        "stderr should list candidate plans (foo); got: {stderr}"
    );
}

#[test]
fn html_open_with_no_plan_arg_falls_through_to_index() {
    // Backward-compat regression: `clank html open` (no positional)
    // must continue to target `index.html`.
    let dir = init_repo();
    let repo = dir.path();
    intro_plan(repo, "foo");

    let out = run(repo, &["html", "open", "--print-path"]);
    assert!(out.status.success());
    let target = target_line(&String::from_utf8_lossy(&out.stdout));
    assert!(
        target.ends_with("index.html"),
        "no-plan-arg must fall through to index.html; got: `{target}`"
    );
    assert!(Path::new(&target).exists());
}
