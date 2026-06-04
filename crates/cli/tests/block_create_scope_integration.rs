//! Integration tests for `clank block create`'s required-scope
//! validation. Scope must be explicit — `--plan <stem>` OR `--all`
//! — to prevent accidentally suppressing every wfw item for the
//! agent across every plan + queue item.

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

fn run_block_create(repo: &Path, name: &str, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("block")
        .arg("create")
        .arg(name)
        .arg("--repo")
        .arg(repo)
        .arg("--author")
        .arg("alice")
        .arg("-m")
        .arg("placeholder question");
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("spawn clank block create")
}

#[test]
fn block_create_no_scope_errors() {
    let dir = init_repo();
    let out = run_block_create(dir.path(), "foo", &[]);
    assert!(
        !out.status.success(),
        "block create with no scope must fail; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--plan") && stderr.contains("--all"),
        "diagnostic must name both flag options; got: {stderr}"
    );
    // No file was written.
    assert!(
        !dir.path()
            .join(".clank/agents/alice/blocks/foo.md")
            .exists()
    );
}

#[test]
fn block_create_with_plan_writes_to_plan_subdir() {
    let dir = init_repo();
    let out = run_block_create(dir.path(), "foo", &["--plan", "my-plan"]);
    assert!(
        out.status.success(),
        "block create --plan should succeed; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = dir.path().join(".clank/agents/alice/blocks/my-plan/foo.md");
    assert!(path.exists(), "plan-scoped block should land at {path:?}");
}

#[test]
fn block_create_all_writes_to_repo_scope_with_warning() {
    let dir = init_repo();
    let out = run_block_create(dir.path(), "foo", &["--all"]);
    assert!(
        out.status.success(),
        "block create --all should succeed; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    let path = dir.path().join(".clank/agents/alice/blocks/foo.md");
    assert!(path.exists(), "repo-scope block should land at {path:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("REPO-WIDE BLOCK"),
        "stderr must include the REPO-WIDE BLOCK notice; got: {stderr}"
    );
}

#[test]
fn block_create_plan_and_all_errors() {
    let dir = init_repo();
    let out = run_block_create(dir.path(), "foo", &["--plan", "my-plan", "--all"]);
    assert!(
        !out.status.success(),
        "--plan and --all must be mutually exclusive; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Neither file path was written.
    assert!(
        !dir.path()
            .join(".clank/agents/alice/blocks/foo.md")
            .exists()
    );
    assert!(
        !dir.path()
            .join(".clank/agents/alice/blocks/my-plan/foo.md")
            .exists()
    );
}
