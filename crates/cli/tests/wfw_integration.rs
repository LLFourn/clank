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

/// Test environment with separate home and repo dirs so user-level
/// `~/.clank/hooks.json` doesn't leak into assertions and
/// user→repo config shadowing can be tested.
struct TestEnv {
    home: tempfile::TempDir,
    repo: tempfile::TempDir,
}

impl TestEnv {
    fn new() -> Self {
        let repo = init_repo();
        // Disable ad-hoc review by default in tests so only plan
        // work is visible (matches pre-adhoc-review behavior).
        let cfg_path = repo.path().join(".clank/config.json");
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            r#"{"review":{"adhoc_feedback":false}}"#,
        )
        .unwrap();
        Self {
            home: tempfile::tempdir().unwrap(),
            repo,
        }
    }

    fn repo(&self) -> &Path {
        self.repo.path()
    }

    fn home(&self) -> &Path {
        self.home.path()
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::new(clank_bin());
        cmd.env("HOME", self.home());
        cmd
    }
}

/// Build a `Command` for `clank` with HOME isolated to a temp dir.
/// For tests that don't use TestEnv, this uses the repo as a
/// fallback home (no ~/.clank/ will exist there).
fn clank_cmd(repo: &Path) -> Command {
    let mut cmd = Command::new(clank_bin());
    cmd.env("HOME", repo);
    cmd
}

/// Disable ad-hoc review in a test repo so old tests that expect
/// "reviewer blocks with no plans" still pass.
fn disable_adhoc_review(repo: &Path) {
    write(
        repo,
        ".clank/config.json",
        r#"{"review":{"adhoc_feedback":false}}"#,
    );
}

/// Spawn `clank wfw …` with stdout piped. Returns a handle the test
/// can read stdout from + reap. Pre-warming sleep so the inotify
/// watcher is attached before the test mutates files.
///
/// **Explicitly passes `--no-poll`** so the native-watcher path is
/// exercised regardless of whether the test suite is running under
/// `CODEX_SANDBOX=seatbelt`. Any test that wants polling mode must
/// build its own `clank_cmd(repo)` chain with `--poll`.
fn spawn_wfw(repo: &Path, args: &[&str]) -> std::process::Child {
    let child = clank_cmd(repo)
        .arg("wfw")
        .arg("--no-poll")
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
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
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
        stdout.contains("/agents/alice/feedback/"),
        "expected feedback path under alice's agent dir; got stdout=`{stdout}`"
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
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
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
fn wfw_reviewer_wakes_on_commit_with_index_already_staged() {
    // Isolation test for the daemon-style two-root watch shape.
    // The other "wakes on commit" tests stage the change AFTER
    // parking wfw, so `git add` itself fires the `.git/index`
    // event and the subsequent `git commit -m` rides in on the
    // 200ms debounce. That passes even if the commit-time
    // metadata write never fires.
    //
    // This test stages BEFORE parking, then runs ONLY
    // `git commit -m`. The only FS event the watcher can wake
    // on is the commit-boundary metadata update. If
    // non-recursive `.git/` doesn't catch that, this test
    // fails.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    // alice approved the intro; gate=Approved at startup.
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n\nlgtm\n",
    );

    // Stage the code change BEFORE wfw exists. This is the
    // critical ordering — the `git add` index write happens
    // before the watcher attaches, so it cannot be the wake.
    write(repo, "src/lib.rs", "// hello\n");
    git(repo, &["add", "src/lib.rs"]);

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

    // Only `git commit -m`. No further `git add`, no other FS
    // mutations. The wake must come from the commit-boundary
    // gitdir activity alone.
    git(repo, &["commit", "--quiet", "-m", "[foo] code work"]);
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
        "expected reviewer wake on commit-only sha {code_sha}; got stdout=`{stdout}`"
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
        &format!(".clank/agents/codex/feedback/{intro_sha}.md"),
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
        &format!(".clank/agents/alice/feedback/{initial_head}.md"),
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

/// Run the `clank` binary (the one this test crate built) against a
/// repo as a one-shot subcommand. Returns stdout. Panics on non-zero
/// exit so tests fail loudly when `finish`/`init` etc. break.
fn clank_run(repo: &Path, args: &[&str]) -> String {
    let output = clank_cmd(repo)
        .args(args)
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank");
    if !output.status.success() {
        panic!(
            "clank {args:?} failed (exit={:?}): stdout=`{}` stderr=`{}`",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn wfw_master_plan_only_approval_routes_to_implement() {
    // Plan-only intro is approved → master should be told to
    // implement, not finalize. Regression for the projection bug
    // where Approved+Clean always routed to MasterToFinalize.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n\nlgtm\n",
    );

    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "3s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");
    assert!(output.status.success(), "wfw exit={:?}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        stdout.contains("next=Implement")
            && stdout.contains("reason=ready_to_start_implementation"),
        "expected Implement routing; got stdout=`{stdout}`"
    );
    assert!(
        !stdout.contains("next=Finalize"),
        "Finalize must NOT appear for plan-only approval; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_master_code_only_approval_routes_to_finalize() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n\nplan lgtm\n",
    );

    // Now an impl commit, then alice approves it.
    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[foo] impl");
    let impl_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{impl_sha}.md"),
        "APPROVE\n\nimpl lgtm\n",
    );

    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "3s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        stdout.contains("next=Finalize") && stdout.contains("reason=ready_to_finalize"),
        "expected Finalize routing for code-attributed approval; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_reviewer_finish_wake_human_output() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n\nlgtm\n",
    );

    // Park as alice (no reviewer work, gate already approved).
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

    // Finalize the plan from outside.
    clank_run(repo, &["finish", "foo"]);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("finished") && stdout.contains("foo"),
        "expected finished notice for foo; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_reviewer_finish_wake_json_output() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
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
            "-j",
        ],
    );

    clank_run(repo, &["finish", "foo"]);
    let final_sha = head_sha(repo);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("wfw -j stdout must be valid JSON");
    let items = v["items"].as_array().expect("items array");
    let finished = items
        .iter()
        .find(|i| i["kind"] == "finished")
        .expect("at least one finished item");
    assert_eq!(finished["plan"], "foo");
    assert_eq!(finished["finalized_at"], final_sha);
}

#[test]
fn wfw_plan_filter_finish_wake() {
    // Same as reviewer-finish-wake but with --plan, exercising the
    // snapshot path that previously returned Ok(None) when the
    // filtered plan disappeared.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n",
    );

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--plan",
            "foo",
            "--timeout",
            "30s",
        ],
    );

    clank_run(repo, &["finish", "foo"]);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("finished"),
        "expected finished notice for filtered plan; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_mixed_work_and_finished_on_one_wake() {
    // Two active plans `a` and `b`. Alice has approved both
    // intros. Park wfw, then in quick succession (a) land a new
    // reviewable commit on `a` and (b) finalize `b`. The next
    // refold should emit BOTH a reviewer item for `a` and a
    // finished item for `b` — not just one of them.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/a.md", "# a\n");
    commit(repo, "[a] intro");
    let a_intro = head_sha(repo);
    write(repo, ".clank/plans/b.md", "# b\n");
    commit(repo, "[b] intro");
    let b_intro = head_sha(repo);

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{a_intro}.md"),
        "APPROVE\n",
    );
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{b_intro}.md"),
        "APPROVE\n",
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
            "-j",
        ],
    );

    // Land a new reviewable commit on `a` AND finalize `b`.
    write(repo, ".clank/plans/a.md", "# a v2\n");
    commit(repo, "[a] revise");
    clank_run(repo, &["finish", "b"]);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("wfw -j stdout must be valid JSON");
    let items = v["items"].as_array().expect("items array");
    let has_reviewer_for_a = items
        .iter()
        .any(|i| i["kind"] == "reviewer" && i["plan"] == "a");
    let has_finished_for_b = items
        .iter()
        .any(|i| i["kind"] == "finished" && i["plan"] == "b");
    assert!(
        has_reviewer_for_a && has_finished_for_b,
        "expected reviewer(a) AND finished(b) in same items[]; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_finish_wake_survives_early_snapshot_event() {
    // Race regression: `clank finish` moves `.clank/plans/<stem>.md`
    // to `.clank/finished/<stem>.md` BEFORE committing. The FS event
    // for that file write wakes wfw, the 200ms debounce ends BEFORE
    // the commit lands, and the refold sees nothing finished. If wfw
    // relied solely on the post-commit git-ref event for the second
    // wake, that event could fail to fire (notify drops it under
    // load, debounce ate it, etc.) and wfw would block forever. The
    // heartbeat refold is the safety net. This test forces the
    // ordering: write the finished file manually, sleep PAST the
    // debounce window, THEN run the same `git` calls clank finish does.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);
    // Approval is untracked feedback — `.clank/agents/` is
    // gitignored just like `.clank/feedback/` was. The
    // subsequent staged-finalize commit doesn't pick it up.
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n",
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

    // Stage 1: write the finished plan file (mv-finish approach).
    write(repo, ".clank/finished/foo.md", "# foo\n");

    // Stage 2: sleep past the watcher's debounce window.
    std::thread::sleep(Duration::from_millis(1200));

    // Stage 3: commit the finalize — delete from plans/, add to finished/.
    git(repo, &["rm", "--quiet", ".clank/plans/foo.md"]);
    git(repo, &["add", ".clank/finished/foo.md"]);
    git(repo, &["commit", "--quiet", "-m", "Finalize foo"]);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("finished") && stdout.contains("foo"),
        "expected finished notice after early-snapshot + late-commit race; got stdout=`{stdout}` stderr=`{stderr}`"
    );
}

#[test]
fn wfw_plan_already_finished_at_startup_emits_finished_and_exits() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n",
    );
    clank_run(repo, &["finish", "foo"]);
    let final_sha = head_sha(repo);

    // Plan is already finished; explicit --plan should emit a
    // finished item and exit 0 within the short timeout, NOT block.
    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--plan",
            "foo",
            "--timeout",
            "2s",
            "-j",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn wfw");
    assert!(
        output.status.success(),
        "wfw exit={:?} stderr=`{}`",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("wfw -j stdout must be valid JSON");
    let items = v["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1, "expected exactly one item; got {stdout}");
    assert_eq!(items[0]["kind"], "finished");
    assert_eq!(items[0]["plan"], "foo");
    assert_eq!(items[0]["finalized_at"], final_sha);
}

#[test]
fn wfw_polling_mode_wakes_on_commit_via_periodic_refold() {
    // Polling-mode proof: with --poll explicit, wfw does NOT
    // watch the gitdir natively. The wake on a git commit
    // comes from the 500ms periodic refold tick. Stage the
    // change BEFORE parking wfw so no `.clank/` event is in
    // play either — the ONLY signal available is the tick.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "APPROVE\n\nlgtm\n",
    );

    write(repo, "src/lib.rs", "// hello\n");
    git(repo, &["add", "src/lib.rs"]);

    // Build the Command directly so we can pass --poll. The
    // shared spawn_wfw helper bakes in --no-poll.
    let mut child = clank_cmd(repo)
        .args([
            "wfw",
            "--poll",
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "30s",
        ])
        .arg("--repo")
        .arg(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn clank wfw --poll");
    // Attach headroom — same as spawn_wfw.
    std::thread::sleep(Duration::from_millis(1500));

    git(repo, &["commit", "--quiet", "-m", "[foo] code work"]);
    let code_sha = head_sha(repo);

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw --poll exit={exit:?} stdout=`{stdout}` stderr=`{stderr}`"
    );
    assert!(
        stdout.contains("review") && stdout.contains(&code_sha[..7]),
        "expected reviewer wake via poll tick on sha {code_sha}; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_master_no_plans_exits_immediately_json() {
    // Master + empty plan set has nothing the watch loop can resolve
    // into work — exit 0 with an empty items envelope so the wait-mode
    // Stop hook doesn't hang the agent's turn.
    let dir = init_repo();
    let repo = dir.path();
    // Need a commit so HEAD resolves; touch only a non-clank file.
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    // --timeout 30s with exit 0 + empty items proves the fast-exit
    // path fired (the wait loop would have produced exit 2 on
    // timeout). No wall-clock assertion — it flakes on cold subprocess
    // startup and the exit shape is already a stronger signal.
    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "30s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wfw exit={:?} stdout=`{stdout}` stderr=`{stderr}`",
        output.status
    );
    assert_eq!(
        stdout.trim(),
        r#"{"items":[]}"#,
        "expected empty items envelope; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_reviewer_no_plans_still_blocks() {
    // Regression guard for the master/reviewer asymmetry: reviewers
    // MUST keep blocking on an empty plan set — a plan they need to
    // review may land any moment. If a future change widens the
    // early-exit to both roles this test catches it.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");
    disable_adhoc_review(repo);

    let start = Instant::now();
    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "2s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");
    let elapsed = start.elapsed();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected timeout exit 2; got {:?} stderr=`{}`",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed >= Duration::from_millis(1800),
        "expected reviewer to block for ~2s, exited in {elapsed:?}"
    );
}

#[test]
fn wfw_master_with_active_plan_still_blocks() {
    // Regression guard: master with an active plan that is currently
    // waiting on reviewers must still block — reviewer feedback can
    // arrive and transition the gate. The early-exit only applies to
    // an empty plan set.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let start = Instant::now();
    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "2s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");
    let elapsed = start.elapsed();

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected timeout exit 2; got {:?} stderr=`{}`",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed >= Duration::from_millis(1800),
        "expected master to block for ~2s while plan awaits review, exited in {elapsed:?}"
    );
}

#[test]
fn wfw_hook_fires_reviewer_work() {
    let env = TestEnv::new();
    let repo = env.repo();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    let marker = repo.join("hook-fired.txt");
    let hook_cmd = format!("echo $CLANK_EVENT $CLANK_PLAN > {}", marker.display());
    write(
        repo,
        ".clank/hooks.json",
        &format!(r#"{{"reviewer-work": "{hook_cmd}"}}"#),
    );

    let mut child = env
        .cmd()
        .arg("wfw")
        .arg("--no-poll")
        .args([
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "30s",
        ])
        .arg("--repo")
        .arg(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    std::thread::sleep(Duration::from_millis(1500));

    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    assert!(exit.success(), "wfw exit={exit:?} stdout=`{stdout}`");
    assert!(marker.exists(), "hook marker file should exist");
    let content = std::fs::read_to_string(&marker).unwrap();
    assert!(
        content.contains("reviewer-work") && content.contains("foo"),
        "marker should contain event + plan; got: {content}"
    );
}

#[test]
fn wfw_hook_failure_does_not_fail_wfw() {
    let env = TestEnv::new();
    let repo = env.repo();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    write(repo, ".clank/hooks.json", r#"{"reviewer-work": "exit 1"}"#);

    let mut child = env
        .cmd()
        .arg("wfw")
        .arg("--no-poll")
        .args([
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "30s",
        ])
        .arg("--repo")
        .arg(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    std::thread::sleep(Duration::from_millis(1500));

    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    let stderr = read_stderr_to_end(&mut child);
    assert!(
        exit.success(),
        "wfw should succeed despite hook failure; stderr={stderr}"
    );
    assert!(
        stdout.contains("review"),
        "should still emit work items; stdout={stdout}"
    );
    assert!(
        stderr.contains("lifecycle hook") && stderr.contains("reviewer-work"),
        "stderr should warn about the failing hook; got: {stderr}"
    );
}

#[test]
fn wfw_master_empty_exits_even_with_hooks_configured() {
    let env = TestEnv::new();
    let repo = env.repo();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    write(repo, ".clank/hooks.json", r#"{"reviewer-work": "true"}"#);

    let output = env
        .cmd()
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "30s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");

    assert!(
        output.status.success(),
        "master-empty should fast-exit even with hooks; got {:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        r#"{"items":[]}"#,
    );
}

#[test]
fn wfw_idle_hook_returns_prompt() {
    let env = TestEnv::new();
    let repo = env.repo();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    write(
        repo,
        ".clank/hooks.json",
        r#"{"idle": "echo Check stubs for ideas"}"#,
    );

    let output = env
        .cmd()
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "30s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let items = envelope["items"].as_array().expect("items array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["kind"], "idle");
    assert!(
        items[0]["prompt"].as_str().unwrap().contains("Check stubs"),
        "idle prompt should contain hook stdout; got: {}",
        items[0]["prompt"]
    );
}

#[test]
fn wfw_user_hooks_shadowed_by_repo_hooks() {
    let env = TestEnv::new();
    let repo = env.repo();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    let user_marker = env.home().join("user-hook-ran.txt");
    let repo_marker = repo.join("repo-hook-ran.txt");

    let user_hooks_dir = env.home().join(".clank");
    std::fs::create_dir_all(&user_hooks_dir).unwrap();
    std::fs::write(
        user_hooks_dir.join("hooks.json"),
        format!(r#"{{"reviewer-work": "touch {}"}}"#, user_marker.display()),
    )
    .unwrap();

    write(
        repo,
        ".clank/hooks.json",
        &format!(r#"{{"reviewer-work": "touch {}"}}"#, repo_marker.display()),
    );

    let mut child = env
        .cmd()
        .arg("wfw")
        .arg("--no-poll")
        .args([
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "30s",
        ])
        .arg("--repo")
        .arg(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    std::thread::sleep(Duration::from_millis(1500));

    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    assert!(exit.success());

    assert!(repo_marker.exists(), "repo-level hook should have run");
    assert!(
        !user_marker.exists(),
        "user-level hook should be shadowed by repo-level"
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
