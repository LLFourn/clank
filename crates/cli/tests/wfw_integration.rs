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
    // Pre-register alice as a reviewer — the most-used label in this
    // test file. Tests that use other reviewer labels register them
    // individually.
    register_reviewer(path, "alice");
    dir
}

fn register_reviewer(repo: &Path, label: &str) {
    // Source of truth for gate input is the merged declaration
    // post agent-add-cli-and-repo-scope. Append <label> to
    // <repo>/.clank/config.json's `agents` field, preserving any
    // other top-level keys (review, hooks) the test setup may
    // have written. A single typed RepoConfigFile that serdes
    // all fields would be cleaner — tracked as a follow-up plan.
    merge_repo_config(repo, |f| {
        let entry = clank::cli::config::DefaultAgent {
            label: clank_core::ids::AgentLabel::parse(label).unwrap(),
            role: clank_core::vocab::Role::Reviewer,
            tool: None,
            launch: None,
            initial_prompt: None,
        };
        f.agents.get_or_insert_with(Vec::new).push(entry);
    });
}

/// Typed read-modify-write helper for `<repo>/.clank/config.json`
/// per `typed-config-dogfood`. Round-trips through `RepoConfigFile`
/// — every field touched here is a known typed field, and unknown
/// top-level keys land in `extra` to survive the round trip.
fn merge_repo_config<F: FnOnce(&mut clank::cli::config::RepoConfigFile)>(repo: &Path, modify: F) {
    let path = repo.join(".clank/config.json");
    let mut file: clank::cli::config::RepoConfigFile = match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => clank::cli::config::RepoConfigFile::default(),
    };
    modify(&mut file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(&file).unwrap()).unwrap();
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
/// `~/.clank/config.json` doesn't leak into assertions and
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
        // Uses merge so register_reviewer's prior `agents` field
        // survives.
        merge_repo_config(repo.path(), |f| {
            f.review = Some(clank::cli::config::ReviewSection {
                adhoc_feedback: Some(false),
                ..Default::default()
            });
        });
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
/// "reviewer blocks with no plans" still pass. Uses merge so any
/// prior agent registrations survive.
fn disable_adhoc_review(repo: &Path) {
    merge_repo_config(repo, |f| {
        f.review = Some(clank::cli::config::ReviewSection {
            adhoc_feedback: Some(false),
            ..Default::default()
        });
    });
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
fn wfw_already_approved_reviewer_does_not_wake_while_peer_pending() {
    // Two registered reviewers; alice has APPROVED but bob hasn't.
    // The all-reviewers gate's load-bearing UX guarantee: alice
    // should NOT be woken again — only bob (the missing reviewer)
    // has work. alice's wfw must time out.
    let dir = init_repo();
    let repo = dir.path();
    register_reviewer(repo, "bob");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    // alice already approved; bob hasn't reviewed. alice should
    // get no review item.
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
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "3s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    // Timeout-without-work exits 0 with empty work output; the
    // important assertion is that NO review item was emitted.
    assert!(
        !stdout.contains("review"),
        "alice already approved; should not get a review wake. stdout=`{stdout}` stderr=`{stderr}`"
    );
}

#[test]
fn wfw_missing_reviewer_does_wake_while_peer_already_approved() {
    // Symmetric counterpart: two registered reviewers; alice has
    // APPROVED but bob hasn't. bob calls wfw — they should get
    // their review item promptly.
    let dir = init_repo();
    let repo = dir.path();
    register_reviewer(repo, "bob");
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
            "bob",
            "--role",
            "reviewers",
            "--timeout",
            "3s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wfw exit={:?} stdout=`{stdout}` stderr=`{stderr}`",
        output.status
    );
    assert!(
        stdout.contains("review") && stdout.contains(&intro_sha[..7]),
        "bob is the missing reviewer; should get a review item for the intro sha {intro_sha}; got stdout=`{stdout}`"
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
    // This test uses codex as the reviewer; register codex so the
    // all-reviewers gate treats its verdict as load-bearing.
    register_reviewer(repo, "codex");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    // Initial: no feedback at all. Gate=Unreviewed, waiting_on=ReviewerApprovalsMissing.
    // Master role has no work; reviewers do.
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
fn wfw_master_approve_only_routes_to_continue() {
    // APPROVE alone (no FINISHED) routes master to Continue,
    // regardless of whether the approved commit touched code.
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
        stdout.contains("next=Continue") && stdout.contains("reason=gate_approved"),
        "expected Continue routing for approve-only; got stdout=`{stdout}`"
    );
    assert!(
        !stdout.contains("next=Finalize"),
        "Finalize must NOT appear without a FINISHED vote; got stdout=`{stdout}`"
    );
}

#[test]
fn wfw_master_finished_verdict_routes_to_finalize() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro_sha = head_sha(repo);

    // A single FINISHED vote on the intro commit unlocks finalize,
    // even for a plan-only chain (the research path).
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{intro_sha}.md"),
        "FINISHED\n\nplan is done\n",
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
        "expected Finalize routing for FINISHED verdict; got stdout=`{stdout}`"
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
    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[foo] impl");
    let impl_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{impl_sha}.md"),
        "FINISHED\n\nlgtm\n",
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
    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[foo] impl");
    let impl_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{impl_sha}.md"),
        "FINISHED\n\nlgtm\n",
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
    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[foo] impl");
    let impl_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{impl_sha}.md"),
        "FINISHED\n",
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
    // `b` needs an approved code-touching commit before it can
    // be finalized.
    write(repo, "src/b.rs", "// impl b\n");
    commit(repo, "[b] impl");
    let b_impl = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{b_impl}.md"),
        "FINISHED\n",
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
    git(repo, &["commit", "--quiet", "-m", "[foo] finish"]);

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
    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[foo] impl");
    let impl_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{impl_sha}.md"),
        "FINISHED\n",
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
fn wfw_master_no_plans_parks_until_timeout() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "1s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");

    assert_eq!(
        output.status.code(),
        Some(2),
        "idle master should park and timeout (exit 2)"
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
fn wfw_skips_queue_promote_on_duplicate_names() {
    // Regression: two queue files with the same logical name
    // must not silently surface as a promote item. wfw should
    // bail loudly (to stderr) and emit no items.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "x");
    commit(repo, "init");
    write(repo, ".clank/queue/400-foo.md", "# foo\n");
    write(repo, ".clank/queue/410-foo.md", "# foo v2\n");

    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "1s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");
    // No queue item promoted → master times out (exit 2).
    assert_eq!(
        output.status.code(),
        Some(2),
        "expected timeout exit; got stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ambiguous") && stderr.contains("foo"),
        "expected ambiguity warning on stderr; got: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("promote_from_queue"),
        "wfw must NOT emit a promote item under name ambiguity; got: {stdout}"
    );
}

#[test]
fn wfw_fresh_repo_does_not_surface_adhoc_review_by_default() {
    // A repo with no `.clank/` config and a plain commit must not
    // surface AdHocReview work to a reviewer. The default for
    // review.adhoc_feedback is `false`, so wfw should time out
    // cleanly (exit 2) instead of pulling the commit in for review.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "1s",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");

    assert_eq!(
        output.status.code(),
        Some(2),
        "expected timeout exit 2 (no adhoc work by default); got {:?} stdout=`{}` stderr=`{}`",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("adhoc_review") && !stdout.contains("AdHocReview"),
        "no adhoc work should be emitted; got: {stdout}"
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
    merge_repo_config(repo, |f| {
        f.hooks = Some(clank::cli::config::HooksSection {
            reviewer_work: Some(Some(hook_cmd.clone())),
            ..Default::default()
        });
    });

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

    merge_repo_config(repo, |f| {
        f.hooks = Some(clank::cli::config::HooksSection {
            reviewer_work: Some(Some("exit 1".to_string())),
            ..Default::default()
        });
    });

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
fn wfw_master_empty_parks_with_hooks_configured() {
    let env = TestEnv::new();
    let repo = env.repo();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    merge_repo_config(repo, |f| {
        f.hooks = Some(clank::cli::config::HooksSection {
            reviewer_work: Some(Some("true".to_string())),
            ..Default::default()
        });
    });

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
            "1s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");

    assert_eq!(
        output.status.code(),
        Some(2),
        "idle master should park and timeout; got {:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn wfw_idle_hook_fires_but_master_parks() {
    let env = TestEnv::new();
    let repo = env.repo();
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    let marker = repo.join("idle-hook-ran.txt");
    let hook_cmd = format!("touch {}", marker.display());
    let cfg = clank::cli::config::RepoConfigFile {
        review: Some(clank::cli::config::ReviewSection {
            adhoc_feedback: Some(false),
            ..Default::default()
        }),
        hooks: Some(clank::cli::config::HooksSection {
            idle: Some(Some(hook_cmd.clone())),
            ..Default::default()
        }),
        ..Default::default()
    };
    write(
        repo,
        ".clank/config.json",
        &serde_json::to_string_pretty(&cfg).unwrap(),
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
            "2s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn");

    assert_eq!(
        output.status.code(),
        Some(2),
        "idle master should park after idle hook; got {:?}",
        output.status
    );
    assert!(
        marker.exists(),
        "idle hook should have fired and created marker file"
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

    let user_config_dir = env.home().join(".clank");
    std::fs::create_dir_all(&user_config_dir).unwrap();
    let user_cfg = clank::cli::config::UserConfigFile {
        hooks: Some(clank::cli::config::HooksSection {
            reviewer_work: Some(Some(format!("touch {}", user_marker.display()))),
            ..Default::default()
        }),
        ..Default::default()
    };
    std::fs::write(
        user_config_dir.join("config.json"),
        serde_json::to_string_pretty(&user_cfg).unwrap(),
    )
    .unwrap();

    let repo_marker_path = repo_marker.display().to_string();
    merge_repo_config(repo, |f| {
        f.hooks = Some(clank::cli::config::HooksSection {
            reviewer_work: Some(Some(format!("touch {}", repo_marker_path))),
            ..Default::default()
        });
    });

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

#[test]
fn wfw_master_parked_wakes_on_queue_item() {
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "30s",
            "--json",
        ],
    );

    std::fs::create_dir_all(repo.join(".clank/queue")).unwrap();
    std::fs::write(
        repo.join(".clank/queue/100-new-feature.md"),
        "# new feature\n",
    )
    .unwrap();

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    assert!(
        exit.success(),
        "should wake on queue item; exit={exit:?} stdout=`{stdout}`"
    );
    assert!(
        stdout.contains("promote_from_queue") && stdout.contains("new-feature"),
        "should emit PromoteFromQueue; got: {stdout}"
    );
}

#[test]
fn wfw_master_with_active_plan_ignores_queue() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    std::fs::create_dir_all(repo.join(".clank/queue")).unwrap();
    std::fs::write(repo.join(".clank/queue/100-queued.md"), "# queued\n").unwrap();

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "3s",
            "--json",
        ],
    );

    let exit = wait_for_exit(&mut child, Duration::from_secs(10));
    let stdout = read_stdout_to_end(&mut child);
    assert_eq!(
        exit.code(),
        Some(2),
        "should timeout, not promote; stdout=`{stdout}`"
    );
    assert!(
        !stdout.contains("promote"),
        "should not emit queue items while plan active; got: {stdout}"
    );
}

#[test]
fn wfw_repo_block_suppresses_available_work() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    write(
        repo,
        ".clank/agents/claude/blocks/need-discussion.md",
        "should we even do this?",
    );

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "claude",
            "--role",
            "reviewers",
            "--timeout",
            "3s",
            "--json",
        ],
    );

    let exit = wait_for_exit(&mut child, Duration::from_secs(10));
    let stdout = read_stdout_to_end(&mut child);
    assert_eq!(
        exit.code(),
        Some(2),
        "repo block should suppress reviewer work and park; stdout=`{stdout}`"
    );
    assert!(
        !stdout.contains("review"),
        "should not emit review work while repo-blocked; got: {stdout}"
    );
}

#[test]
fn wfw_repo_block_suppresses_queue_promotion() {
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    std::fs::create_dir_all(repo.join(".clank/queue")).unwrap();
    std::fs::write(repo.join(".clank/queue/100-feature.md"), "# feature\n").unwrap();

    write(
        repo,
        ".clank/agents/lloyd/blocks/halt.md",
        "stop everything",
    );

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "lloyd",
            "--role",
            "master",
            "--timeout",
            "3s",
            "--json",
        ],
    );

    let exit = wait_for_exit(&mut child, Duration::from_secs(10));
    let stdout = read_stdout_to_end(&mut child);
    assert_eq!(
        exit.code(),
        Some(2),
        "repo block should suppress queue promotion; stdout=`{stdout}`"
    );
    assert!(
        !stdout.contains("promote"),
        "should not emit promote while repo-blocked; got: {stdout}"
    );
}

// ─── wfw-surfaces-work-around-blocked-plans ──────────────────────
//
// When a master's only active plan is blocked at the agent level
// (a plan-scope block), wfw used to silently park because the
// queue-fallback gate checked `fold.plans.is_empty()` rather than
// "no actionable plans for this agent." Below tests cover the fix:
// master sees the next queue item AND the pending Blocked entry.

#[test]
fn wfw_master_blocked_plan_surfaces_next_queue_item() {
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    // One active plan.
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    // Plan-scope block on `foo` for the calling master.
    write(
        repo,
        ".clank/agents/lloyd/blocks/foo/need-decision.md",
        "is this the right direction?",
    );

    // A queued item.
    std::fs::create_dir_all(repo.join(".clank/queue")).unwrap();
    std::fs::write(
        repo.join(".clank/queue/100-next-thing.md"),
        "# next thing\n",
    )
    .unwrap();

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
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "wfw should exit success when surfacing PromoteFromQueue; \
         exit={:?} stdout=`{stdout}` stderr=`{}`",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("promote_from_queue") && stdout.contains("next-thing"),
        "stdout should include PromoteFromQueue for the queue item; got: {stdout}"
    );
    assert!(
        stdout.contains("blocked") && stdout.contains("need-decision"),
        "stdout should ALSO surface the pending Blocked entry; got: {stdout}"
    );
}

#[test]
fn wfw_master_blocked_plan_empty_queue_times_out() {
    // Empty queue + only-plan-blocked → master parks until
    // timeout. (No PromoteFromQueue to emit.) The idle hook
    // would fire in this branch if configured, but a stop-hook
    // continuation only happens on success exits.
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(
        repo,
        ".clank/agents/lloyd/blocks/foo/halt.md",
        "stop everything",
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
            "1s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(
        output.status.code(),
        Some(2),
        "empty queue + all plans blocked → park until timeout; \
         exit={:?} stdout=`{stdout}`",
        output.status
    );
    assert!(
        !stdout.contains("promote_from_queue"),
        "no queue item to promote; got: {stdout}"
    );
}

#[test]
fn wfw_master_one_blocked_one_actionable_returns_only_actionable() {
    // Two active plans, one blocked, one with master work pending.
    // Master should see ONLY the actionable plan's work item plus
    // the pending Blocked entry — NOT PromoteFromQueue (an
    // actionable plan still exists for this agent).
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    // Plan A (will be blocked).
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    // Plan B (REQUEST_CHANGES from alice → master work pending).
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");
    let bar_sha = head_sha(repo);
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{}.md", &bar_sha[..7]),
        "REQUEST_CHANGES need tightening\n",
    );

    // Plan-scope block on foo (NOT on bar).
    write(
        repo,
        ".clank/agents/lloyd/blocks/foo/halt.md",
        "stop everything",
    );

    // Queue item that should NOT be surfaced — bar is actionable.
    std::fs::create_dir_all(repo.join(".clank/queue")).unwrap();
    std::fs::write(repo.join(".clank/queue/100-other.md"), "# other\n").unwrap();

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
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "wfw should exit success on actionable plan; stdout=`{stdout}`"
    );
    // bar's master work surfaces.
    assert!(
        stdout.contains("bar"),
        "actionable plan `bar` must be surfaced; got: {stdout}"
    );
    // Queue NOT surfaced (an actionable plan exists).
    assert!(
        !stdout.contains("promote_from_queue"),
        "queue must NOT surface while an actionable plan exists; got: {stdout}"
    );
    // Blocked entry MUST also surface so master sees both.
    // Codex caught the omission on 0a3c039 — actionable-path
    // used to emit only the actionable items.
    assert!(
        stdout.contains("blocked") && stdout.contains("halt"),
        "Blocked entry for foo must co-surface with bar's work; got: {stdout}"
    );
}

#[test]
fn wfw_reviewer_blocked_plan_does_not_surface_promote() {
    // Regression guard for the master-only role gate: this fix is
    // scoped to master. Reviewer code paths must NOT start emitting
    // PromoteFromQueue when all reviewable plans are suppressed.
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    // alice is the reviewer (auto-registered by init_repo).
    write(
        repo,
        ".clank/agents/alice/blocks/foo/need-decision.md",
        "is this the right direction?",
    );

    std::fs::create_dir_all(repo.join(".clank/queue")).unwrap();
    std::fs::write(
        repo.join(".clank/queue/100-next-thing.md"),
        "# next thing\n",
    )
    .unwrap();

    let output = clank_cmd(repo)
        .args([
            "wfw",
            "--no-poll",
            "--author",
            "alice",
            "--role",
            "reviewers",
            "--timeout",
            "1s",
            "--json",
        ])
        .arg("--repo")
        .arg(repo)
        .output()
        .expect("spawn clank wfw");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        !stdout.contains("promote_from_queue"),
        "reviewer must NEVER see PromoteFromQueue; got: {stdout}"
    );
}

#[test]
fn wfw_parks_on_pending_block() {
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    write(
        repo,
        ".clank/agents/claude/blocks/test-question.md",
        "is this ok?",
    );

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "claude",
            "--role",
            "master",
            "--timeout",
            "3s",
            "--json",
        ],
    );

    let exit = wait_for_exit(&mut child, Duration::from_secs(10));
    let stdout = read_stdout_to_end(&mut child);
    assert_eq!(
        exit.code(),
        Some(2),
        "should timeout (park on block), not return block item; stdout=`{stdout}`"
    );
}

#[test]
fn wfw_wakes_on_unblock() {
    let dir = init_repo();
    let repo = dir.path();
    disable_adhoc_review(repo);
    write(repo, "README.md", "# repo\n");
    commit(repo, "init");

    write(
        repo,
        ".clank/agents/claude/blocks/test-question.md",
        "is this ok?",
    );

    let mut child = spawn_wfw(
        repo,
        &[
            "--author",
            "claude",
            "--role",
            "master",
            "--timeout",
            "30s",
            "--json",
        ],
    );

    write(
        repo,
        ".clank/agents/claude/unblocks/test-question.md",
        "yes it is fine",
    );

    let exit = wait_for_exit(&mut child, Duration::from_secs(20));
    let stdout = read_stdout_to_end(&mut child);
    assert!(
        exit.success(),
        "should wake on unblock; exit={exit:?} stdout=`{stdout}`"
    );
    assert!(
        stdout.contains("unblocked") && stdout.contains("yes it is fine"),
        "should emit Unblocked with answer; got: {stdout}"
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
