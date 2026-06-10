//! In-process integration tests for `clank shelve` / `unshelve` /
//! `shelve clean` (plan-lifecycle-verbs). Drives the command cores
//! directly — no binary spawning.

mod common;

use common::TestEnv;
use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?} failed");
}

fn git_out(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout).to_string()
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

/// Master-only repo with an in-flight plan `foo` (intro + 2 impl
/// commits).
fn repo_with_inflight_foo() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("claude", &[], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, "src/base.rs", "// base\n");
    commit(repo, "[misc] base");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/a.rs", "// a\n");
    commit(repo, "[foo] impl a");
    write(repo, "src/b.rs", "// b\n");
    commit(repo, "[foo] impl b");
    env
}

fn shelve_args(env: &TestEnv, to_queue: bool, waiting_for: Option<&str>) -> clank::cli::ShelveArgs {
    clank::cli::ShelveArgs {
        command: None,
        plan: Some("foo".into()),
        repo: Some(env.repo().to_path_buf()),
        waiting_for: waiting_for.map(str::to_string),
        to_queue,
        priority: None,
        force: true, // impl commits touch non-plan content
        dry: false,
        yes: true,
        allow_rewrite_protected: true, // tests run on `main`
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

#[test]
fn shelve_protects_then_drops() {
    let env = repo_with_inflight_foo();
    let repo = env.repo();

    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, false, None,
    )))
    .unwrap();

    // Protective ref exists.
    let refs = git_out(repo, &["show-ref"]);
    assert!(
        refs.contains("refs/clank/shelved/foo"),
        "protective ref missing: {refs}"
    );
    // Branch history is clean of foo.
    let log = git_out(repo, &["log", "--format=%s"]);
    assert!(!log.contains("[foo]"), "foo commits still on branch: {log}");
    assert!(log.contains("[misc] base"), "foreign work preserved");
    // State recorded; plan file gone from the worktree.
    assert!(repo.join(".clank/shelved/foo.json").is_file());
    assert!(!repo.join(".clank/plans/foo.md").exists());
}

#[test]
fn git_gc_does_not_lose_shelved_work() {
    // Ruthless c8f216b concern 1: the protective ref must keep the
    // shelved commits alive through a full gc.
    let env = repo_with_inflight_foo();
    let repo = env.repo();
    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, false, None,
    )))
    .unwrap();

    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(repo.join(".clank/shelved/foo.json")).unwrap(),
    )
    .unwrap();
    let shas: Vec<String> = state["shas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(shas.len(), 3, "intro + 2 impl commits recorded");

    git(repo, &["gc", "--prune=now", "--quiet"]);

    for sha in &shas {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "-e", sha])
            .status()
            .unwrap();
        assert!(status.success(), "shelved commit {sha} lost to gc");
    }
}

#[test]
fn unshelve_restores_and_resets_reviews() {
    let env = repo_with_inflight_foo();
    let repo = env.repo();
    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, false, None,
    )))
    .unwrap();

    // Other work lands while foo is shelved.
    write(repo, "src/other.rs", "// other\n");
    commit(repo, "[misc] other work");

    block_on(clank::cli::shelve::run_unshelve(clank::cli::UnshelveArgs {
        plan: "foo".into(),
        repo: Some(repo.to_path_buf()),
    }))
    .unwrap();

    // Plan file is back; commits replayed on top.
    assert!(repo.join(".clank/plans/foo.md").is_file());
    let log = git_out(repo, &["log", "--format=%s", "-4"]);
    assert!(log.contains("[foo] impl b"), "replayed commits: {log}");
    // Ref + state cleaned up.
    assert!(!repo.join(".clank/shelved/foo.json").exists());
    assert!(!git_out(repo, &["show-ref"]).contains("refs/clank/shelved/foo"));

    // Reviews reset by design: the snapshot sees foo active again.
    // (Master-only team → gate computes zero-reviewer Approved; the
    // load-bearing assertion is that foo is an ACTIVE plan again
    // with its latest reviewable = the replayed head.)
    let snap = block_on(clank::cli::status::snapshot(repo, Some(env.home()))).unwrap();
    let json = snap.to_json();
    let plans = json["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0]["plan"], "foo");
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        plans[0]["latest_reviewable_sha"].as_str().unwrap(),
        head.trim(),
        "replayed head is the new latest reviewable"
    );
}

#[test]
fn interleaved_plan_refuses() {
    let env = TestEnv::init();
    env.register_team("claude", &[], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    // A second plan's commit interleaves into foo's range.
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");
    write(repo, "src/a.rs", "// a\n");
    commit(repo, "[foo] impl a");

    let err = block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, false, None,
    )))
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("foreign commit"),
        "interleaved plan must refuse with the foreign-commit error; got: {err}"
    );
    // Nothing mutated: no ref, no state, branch intact.
    assert!(!git_out(repo, &["show-ref"]).contains("refs/clank/shelved/foo"));
    assert!(!repo.join(".clank/shelved/foo.json").exists());
}

#[test]
fn to_queue_saves_body_and_sets_aside() {
    let env = repo_with_inflight_foo();
    let repo = env.repo();
    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, true, None,
    )))
    .unwrap();

    let queued = repo.join(".clank/queue/500-foo.md");
    assert!(queued.is_file(), "body re-queued");
    assert_eq!(std::fs::read_to_string(&queued).unwrap(), "# foo\n");
    // Commits are STILL set aside restorably (strictly better than
    // the removed demote).
    assert!(repo.join(".clank/shelved/foo.json").is_file());
    assert!(git_out(repo, &["show-ref"]).contains("refs/clank/shelved/foo"));
}

#[test]
fn unshelve_refuses_when_plan_active_again() {
    let env = repo_with_inflight_foo();
    let repo = env.repo();
    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, false, None,
    )))
    .unwrap();

    // The stem gets re-promoted as a fresh plan.
    write(repo, ".clank/plans/foo.md", "# foo v2\n");
    commit(repo, "[foo] intro v2");

    let err = block_on(clank::cli::shelve::run_unshelve(clank::cli::UnshelveArgs {
        plan: "foo".into(),
        repo: Some(repo.to_path_buf()),
    }))
    .unwrap_err()
    .to_string();
    assert!(err.contains("already active"), "got: {err}");
    // Fail-closed: shelved state untouched.
    assert!(repo.join(".clank/shelved/foo.json").is_file());
}

#[test]
fn conflicted_unshelve_leaves_ref_and_state_intact() {
    let env = repo_with_inflight_foo();
    let repo = env.repo();
    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, false, None,
    )))
    .unwrap();

    // Conflicting content at a path the shelved commits also touch.
    write(repo, "src/a.rs", "// conflicting\n");
    commit(repo, "[misc] conflicting change");

    let err = block_on(clank::cli::shelve::run_unshelve(clank::cli::UnshelveArgs {
        plan: "foo".into(),
        repo: Some(repo.to_path_buf()),
    }))
    .unwrap_err()
    .to_string();
    assert!(err.contains("cherry-pick"), "got: {err}");
    // Fail-closed: protective ref + state survive the conflict.
    assert!(git_out(repo, &["show-ref"]).contains("refs/clank/shelved/foo"));
    assert!(repo.join(".clank/shelved/foo.json").is_file());
}

#[test]
fn shelve_clean_discards_ref_and_state() {
    let env = repo_with_inflight_foo();
    let repo = env.repo();
    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env, false, None,
    )))
    .unwrap();

    block_on(clank::cli::shelve::run_clean(clank::cli::ShelveCleanArgs {
        plan: "foo".into(),
        repo: Some(repo.to_path_buf()),
        yes: true,
    }))
    .unwrap();

    assert!(!repo.join(".clank/shelved/foo.json").exists());
    assert!(!git_out(repo, &["show-ref"]).contains("refs/clank/shelved/foo"));
}

#[test]
fn status_surfaces_shelved_with_for_nudge() {
    let env = repo_with_inflight_foo();
    let repo = env.repo();
    block_on(clank::cli::shelve::run_shelve(shelve_args(
        &env,
        false,
        Some("bar"),
    )))
    .unwrap();

    // Before bar exists: waiting, not ready.
    let snap = block_on(clank::cli::status::snapshot(repo, Some(env.home()))).unwrap();
    let json = snap.to_json();
    assert_eq!(json["shelved"][0]["plan"], "foo");
    assert_eq!(json["shelved"][0]["ready"], false);
    assert!(snap.to_human().contains("shelved: foo (waiting on bar)"));

    // bar runs to finish (synthetic finalize).
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");
    write(repo, ".clank/finished/bar.md", "# bar\n");
    git(repo, &["rm", "--quiet", ".clank/plans/bar.md"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[bar] finish"]);

    let snap = block_on(clank::cli::status::snapshot(repo, Some(env.home()))).unwrap();
    let json = snap.to_json();
    assert_eq!(json["shelved"][0]["ready"], true, "nudge fires: {json}");
    assert!(
        snap.to_human().contains("FINISHED; unshelve?"),
        "human nudge: {}",
        snap.to_human()
    );
}
