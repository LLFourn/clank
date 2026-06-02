//! Integration tests for `clank queue add` body sources +
//! content validation.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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

/// Run `clank queue ...` with --repo placed AFTER `queue` and
/// BEFORE the queue subcommand, so it binds to QueueArgs (the
/// parent) rather than the subcommand args.
fn run_queue(repo: &Path, queue_subargs: &[&str]) -> std::process::Output {
    let repo_arg = repo.to_string_lossy().to_string();
    let mut argv: Vec<&str> = vec!["queue", "--repo", &repo_arg];
    argv.extend_from_slice(queue_subargs);
    Command::new(clank_bin())
        .args(&argv)
        .env("HOME", repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank")
}

#[test]
fn queue_add_inline_message_writes_body() {
    let dir = init_repo();
    let out = run_queue(
        dir.path(),
        &["add", "foo", "--priority", "400", "-m", "real body content"],
    );
    assert!(
        out.status.success(),
        "clank queue add failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join(".clank/queue/400-foo.md")).unwrap();
    assert_eq!(body, "# foo\nreal body content");
}

#[test]
fn queue_add_from_path_writes_body() {
    let dir = init_repo();
    let stub = dir.path().join("draft.md");
    std::fs::write(&stub, "# foo\n\nfrom file\n").unwrap();
    let out = run_queue(
        dir.path(),
        &[
            "add",
            "foo",
            "--priority",
            "400",
            "--from",
            stub.to_str().unwrap(),
        ],
    );
    assert!(out.status.success());
    let body = std::fs::read_to_string(dir.path().join(".clank/queue/400-foo.md")).unwrap();
    assert_eq!(body, "# foo\n\nfrom file\n");
}

#[test]
fn queue_add_from_stubs_dir_when_present() {
    let dir = init_repo();
    write(
        dir.path(),
        ".clank/stubs/foo.md",
        "# foo\n\nfrom stubs dir\n",
    );
    let out = run_queue(dir.path(), &["add", "foo", "--priority", "400"]);
    assert!(
        out.status.success(),
        "stubs-dir fallback should work; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join(".clank/queue/400-foo.md")).unwrap();
    assert_eq!(body, "# foo\n\nfrom stubs dir\n");
}

#[test]
fn queue_add_fails_with_no_body_source() {
    let dir = init_repo();
    let out = run_queue(dir.path(), &["add", "foo", "--priority", "400"]);
    assert!(
        !out.status.success(),
        "must reject when neither -m / --from / stubs file is available"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--from") && stderr.contains("-m") && stderr.contains(".clank/stubs/"),
        "error should list all three options; got: {stderr}"
    );
    assert!(
        !dir.path().join(".clank/queue/400-foo.md").exists(),
        "no queue file should be created"
    );
}

#[test]
fn queue_add_from_stdin_dash() {
    let dir = init_repo();
    let mut child = Command::new(clank_bin())
        .args(["queue", "--repo"])
        .arg(dir.path())
        .args(["add", "foo", "--priority", "400", "--from", "-"])
        .env("HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clank");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"# foo\n\nstdin body\n")
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "stdin body should be accepted; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join(".clank/queue/400-foo.md")).unwrap();
    assert_eq!(body, "# foo\n\nstdin body\n");
}

#[test]
fn queue_add_m_and_from_mutually_exclusive() {
    let dir = init_repo();
    let out = run_queue(
        dir.path(),
        &[
            "add",
            "foo",
            "--priority",
            "400",
            "-m",
            "body",
            "--from",
            "/dev/null",
        ],
    );
    assert!(!out.status.success(), "clap should reject both flags");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot be used with") || stderr.contains("conflict"),
        "clap should explain the conflict; got: {stderr}"
    );
}

#[test]
fn queue_add_duplicate_short_circuits_before_reading_source() {
    // Regression: duplicate-name detection must fire BEFORE
    // body-source IO. Otherwise `--from -` would block on
    // stdin, or `--from missing` would fail with an
    // IO error, both worse UX than "this name already
    // exists."
    let dir = init_repo();
    write(dir.path(), ".clank/queue/400-foo.md", "# foo\nbody\n");
    let out = run_queue(
        dir.path(),
        &[
            "add",
            "foo",
            "--priority",
            "410",
            "--from",
            "/this/path/definitely/does/not/exist",
        ],
    );
    assert!(!out.status.success(), "must fail on conflict");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already exists") && stderr.contains("400-foo.md"),
        "expected conflict message, NOT a file-IO error; got: {stderr}"
    );
    assert!(
        !stderr.contains("definitely/does/not/exist") && !stderr.contains("No such file"),
        "source path must NOT be read when the name is a conflict; got: {stderr}"
    );
}

#[test]
fn queue_add_rejects_from_file_that_is_header_only() {
    let dir = init_repo();
    let stub = dir.path().join("draft.md");
    std::fs::write(&stub, "# foo\n").unwrap();
    let out = run_queue(
        dir.path(),
        &[
            "add",
            "foo",
            "--priority",
            "400",
            "--from",
            stub.to_str().unwrap(),
        ],
    );
    assert!(!out.status.success(), "header-only file should be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("empty after the header"),
        "expected post-header validation message; got: {stderr}"
    );
    assert!(!dir.path().join(".clank/queue/400-foo.md").exists());
}

#[test]
fn queue_add_rejects_empty_stubs_file() {
    let dir = init_repo();
    write(dir.path(), ".clank/stubs/foo.md", "");
    let out = run_queue(dir.path(), &["add", "foo", "--priority", "400"]);
    assert!(!out.status.success(), "empty stub should be rejected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("empty after the header"),
        "expected post-header validation message; got: {stderr}"
    );
    assert!(!dir.path().join(".clank/queue/400-foo.md").exists());
}
