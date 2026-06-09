//! Integration tests for `clank finish --squash` / `--purge`
//! being idempotent on an ALREADY-FINISHED plan
//! (`finish-squash-idempotent-on-finished`).

mod common;

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

fn first_parent_count(repo: &Path) -> usize {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--first-parent", "--count", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn run_finish(env: &common::TestEnv, args: &[&str]) -> std::process::Output {
    env.clank()
        .arg("finish")
        // Test repos init on `main` (protected); the rewrite path
        // needs the override to run in place.
        .arg("--allow-rewrite-protected")
        .args(args)
        .arg("--repo")
        .arg(env.repo())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank finish")
}

/// A finished plan `foo` with several squashable impl commits in
/// its [intro, finalize] range. Synthesized (master-only team,
/// synthetic finalize) so the squash has real commits to collapse.
fn finished_plan_with_impl_commits() -> common::TestEnv {
    let env = common::TestEnv::init();
    env.register_team("claude", &[], &[]);
    let repo = env.repo();
    // Gitignore the local-only paths the fold writes (cache/agents)
    // so the fold doesn't dirty the worktree and trip finish's
    // rewrite precondition — mirrors what `clank init` scaffolds.
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/a.rs", "// a\n");
    commit(repo, "[foo] impl a");
    write(repo, "src/b.rs", "// b\n");
    commit(repo, "[foo] impl b");
    // Synthetic finalize: move the plan into `.clank/finished/`.
    write(repo, ".clank/finished/foo.md", "# foo\n");
    git(repo, &["rm", "--quiet", ".clank/plans/foo.md"]);
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] finish"]);
    env
}

#[test]
fn finish_squash_on_finished_plan_collapses_and_keeps_clank() {
    let env = finished_plan_with_impl_commits();
    let repo = env.repo();
    assert!(repo.join(".clank/finished/foo.md").is_file());
    let before = first_parent_count(repo);

    let out = run_finish(&env, &["--squash", "Implement foo", "foo"]);
    assert!(
        out.status.success(),
        "finish --squash on a finished plan must WORK (not no-op); stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // --squash PRESERVES the `.clank/finished` snapshot.
    assert!(
        repo.join(".clank/finished/foo.md").is_file(),
        "finish --squash must keep the finished snapshot (.clank/ preserved)"
    );
    // The impl commits collapsed → fewer commits than before.
    let after = first_parent_count(repo);
    assert!(
        after < before,
        "squash should collapse the range; before={before} after={after}"
    );
}

#[test]
fn finish_squash_is_idempotent_on_rerun() {
    // The plan's NAMED property (ruthless 9d70cf3 concern 1):
    // running `finish --squash` twice yields the SAME HEAD sha —
    // the second run is a no-op, not a re-churn.
    let env = finished_plan_with_impl_commits();
    let repo = env.repo();

    let out1 = run_finish(&env, &["--squash", "Implement foo", "foo"]);
    assert!(
        out1.status.success(),
        "first squash failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let sha1 = head_sha(repo);

    let out2 = run_finish(&env, &["--squash", "Implement foo", "foo"]);
    assert!(
        out2.status.success(),
        "re-squash must succeed (idempotent), not error; stderr=`{}`",
        String::from_utf8_lossy(&out2.stderr)
    );
    let sha2 = head_sha(repo);

    assert_eq!(
        sha1, sha2,
        "finish --squash must be idempotent: a second run must not churn the HEAD sha"
    );
}

#[test]
fn finish_dry_squash_on_finished_plan_does_not_claim_to_create_finalize() {
    // codex c0c34ef: the --dry preview for the non-amend
    // already-finished rewrite path must NOT say "would create
    // finalize commit" (the finalize already exists) — it should
    // say the finalize is left as-is, and preview the squash. And
    // --dry must not move HEAD.
    let env = finished_plan_with_impl_commits();
    let repo = env.repo();
    let before = head_sha(repo);

    let out = run_finish(&env, &["--dry", "--squash", "Implement foo", "foo"]);
    assert!(
        out.status.success(),
        "dry squash failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("would create finalize commit"),
        "dry squash on a FINISHED plan must not claim to create a finalize commit; got:\n{stdout}"
    );
    assert!(
        stdout.contains("already finished") && stdout.contains("squash"),
        "dry output should note the plan is already finished and preview the squash; got:\n{stdout}"
    );
    assert_eq!(before, head_sha(repo), "--dry must not move HEAD");
}

#[test]
fn finish_purge_on_finished_plan_strips_clank() {
    let env = finished_plan_with_impl_commits();
    let repo = env.repo();

    let out = run_finish(&env, &["--purge", "foo"]);
    assert!(
        out.status.success(),
        "finish --purge on a finished plan must work; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !repo.join(".clank/finished/foo.md").exists(),
        "finish --purge must strip the finished snapshot"
    );
}

#[test]
fn finish_purge_rerun_reports_plan_gone_cleanly() {
    // The --purge arm of the re-run question (ruthless 64d458b).
    // Unlike --squash (which KEEPS the plan record → idempotent
    // same-sha re-run), --purge is destructive: it removes the
    // plan from history entirely. So a SECOND --purge can't find
    // the plan — and that's correct, not a bug. Pin that the
    // re-run fails with a CLEAR "not found" message (no panic, no
    // empty-range error) and does NOT mutate HEAD.
    let env = finished_plan_with_impl_commits();
    let repo = env.repo();

    let out1 = run_finish(&env, &["--purge", "foo"]);
    assert!(
        out1.status.success(),
        "first purge failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let head_after_purge = head_sha(repo);

    let out2 = run_finish(&env, &["--purge", "foo"]);
    assert!(
        !out2.status.success(),
        "second purge should fail cleanly (plan is gone), not succeed"
    );
    let stderr = String::from_utf8_lossy(&out2.stderr);
    assert!(
        stderr.contains("not found"),
        "re-purge of a purged plan should report it's not found (the plan was removed); got: {stderr}"
    );
    assert_eq!(
        head_after_purge,
        head_sha(repo),
        "a failed re-purge must not mutate HEAD"
    );
}
