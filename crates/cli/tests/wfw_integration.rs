//! Command-level wake-up tests for `clank wfw`.
//!
//! These spawn the real `clank` binary against a temp git repo and
//! mutate the filesystem mid-flight to assert the watcher loop wakes
//! and produces the expected work item. They cover the two trigger
//! cases the plan calls out:
//!
//! 1. Reviewer wake-up — a fresh reviewable commit lands while
//!    `wfw --role reviewers` is parked; expect a `ReviewerAction`
//!    on the new SHA.
//! 2. Master wake-up — a REQUEST_CHANGES feedback file lands while
//!    `wfw --role master` is parked; expect a `MasterAction` with
//!    `next=revise` / `reason=address_commit_changes`.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

fn commit(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", msg]);
}

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Spawn `clank wfw …` with stdout piped. Returns a handle the test
/// can read stdout from + reap. Pre-warming sleep so the inotify
/// watcher is attached before the test mutates files.
fn spawn_wfw(repo: &Path, args: &[&str]) -> std::process::Child {
    let child = Command::new(clank_bin())
        .arg("wfw")
        .args(args)
        .arg("--repo")
        .arg(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clank wfw");
    // notify takes a moment to attach. macOS FSEvents and Linux
    // inotify both need time to wire up — give a generous headroom
    // so the mutation that follows actually fires through the
    // watcher rather than landing during attach.
    std::thread::sleep(Duration::from_millis(1500));
    child
}

fn read_stdout_to_end(child: &mut std::process::Child) -> String {
    let mut buf = String::new();
    if let Some(out) = child.stdout.as_mut() {
        out.read_to_string(&mut buf).ok();
    }
    buf
}

fn read_stderr_to_end(child: &mut std::process::Child) -> String {
    let mut buf = String::new();
    if let Some(err) = child.stderr.as_mut() {
        err.read_to_string(&mut buf).ok();
    }
    buf
}

#[test]
fn wfw_reviewer_wakes_on_new_reviewable_commit() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    // alice has already approved the intro — she's a participant and
    // the gate is currently Approved on the latest reviewable commit.
    // No reviewer-eligible work for her.
    write(
        repo,
        &format!(".clank/feedback/foo/{intro_sha}/alice.md"),
        "APPROVE\n\nlgtm\n",
    );

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "30s",
        ],
    );

    // Land a new reviewable commit — alice is now missing on it.
    write(repo, ".clank/plans/foo.md", "# foo v2\n");
    commit(repo, "[foo] revise");
    let new_sha = head_sha(repo);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("review") && stdout.contains(&new_sha[..7]),
        "expected review work on new sha {new_sha}; got stdout=`{stdout}`"
    );
    assert!(
        stdout.contains("alice.md"),
        "expected feedback path for alice; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_reviewer_wakes_on_code_only_commit() {
    // Regression for the ref-only wake path: an attribution-only
    // (code-only) commit doesn't touch any `.clank/` path, so the
    // ONLY signal the watcher has is the git-ref update. Confirms
    // logs/HEAD + refs/ watching reaches a parked reviewer.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    // alice approved the intro; gate=Approved at startup.
    write(
        repo,
        &format!(".clank/feedback/foo/{intro_sha}/alice.md"),
        "APPROVE\n\nlgtm\n",
    );

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "30s",
        ],
    );

    // Code-only commit: touches `src/lib.rs`, not the plan file.
    // Classifier inherits attribution to `foo` via the active-plan
    // hint, so this is a reviewable commit, but the watcher must
    // wake on the git-ref update because nothing under `.clank/`
    // moved.
    write(repo, "src/lib.rs", "// hello\n");
    commit(repo, "[foo] code work");
    let code_sha = head_sha(repo);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("review") && stdout.contains(&code_sha[..7]),
        "expected reviewer wake on code-only sha {code_sha}; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_master_wakes_on_request_changes_feedback() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    // Initial: no feedback at all. Gate=Unreviewed, waiting_on=FirstReview.
    // Master role has no work (FirstReview only fires for reviewers).
    let mut child = spawn_wfw(
        repo,
        &["--author", "lloyd", "--role", "master", "--timeout", "30s"],
    );

    // Codex lands REQUEST_CHANGES on the intro commit — gate flips
    // to ChangesRequested, waiting_on becomes MasterToRevise.
    write(
        repo,
        &format!(".clank/feedback/foo/{intro_sha}/codex.md"),
        "REQUEST_CHANGES\n\ntake another look\n",
    );

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("master") && stdout.contains(&intro_sha[..7]),
        "expected master action on intro sha {intro_sha}; got stdout=`{stdout}`"
    );
    assert!(
        stdout.contains("next=Revise") && stdout.contains("address_commit_changes"),
        "expected revise/address_commit_changes; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_wakes_inside_linked_worktree_when_its_ref_moves() {
    let main_dir = init_repo();
    let main = main_dir.path();
    write(main, ".clank/plans/foo.md", "# foo\n");
    commit(main, "[foo] intro");

    // Linked worktree on a feature branch. wfw will run inside it
    // and resolve its own HEAD against the common refs dir. Use a
    // fresh tempdir so parallel test runs don't collide.
    let wt_holder = tempfile::tempdir().unwrap();
    let wt_root = wt_holder.path().join("linked-wt");
    git(
        main,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            wt_root.to_str().unwrap(),
        ],
    );
    let wt = wt_root.as_path();
    let initial_head = head_sha(wt);
    // Reviewer is the existing participant; she approves the intro
    // so there's no reviewer work at startup.
    write(
        wt,
        &format!(".clank/feedback/foo/{initial_head}/alice.md"),
        "APPROVE\n\nlgtm\n",
    );

    let mut child = spawn_wfw(
        wt,
        &[
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "30s",
        ],
    );

    // Advance the linked worktree's branch from the main worktree's
    // perspective via a separate `git` invocation. This updates the
    // shared `refs/heads/feature` ref — exactly the case the
    // git-common-dir watcher needs to wake on.
    write(wt, ".clank/plans/foo.md", "# foo v2\n");
    commit(wt, "[foo] revise");
    let new_head = head_sha(wt);
    assert_ne!(initial_head, new_head);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("review") && stdout.contains(&new_head[..7]),
        "expected reviewer wake on new sha {new_head}; got stdout=`{stdout}`"
    );
}

fn wait_for_exit(child: &mut std::process::Child, max: Duration) -> std::process::ExitStatus {
    let end = Instant::now() + max;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) => {
                if Instant::now() >= end {
                    let _ = child.kill();
                    panic!("wfw did not exit within {max:?}");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    }
}
