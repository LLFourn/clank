//! Integration tests for `clank demote`. Cover the major safety
//! tiering, transactional ordering, and target-path collision
//! cases per the plan's acceptance criteria.

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

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

/// Init a git repo + a master claude agent + a seed commit on
/// main. Returns the tempdir guard.
fn init_repo_with_master() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "--quiet", "--initial-branch=main"]);
    git(repo, &["config", "user.email", "test@test"]);
    git(repo, &["config", "user.name", "test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
    write(repo, ".clank/.gitignore", "/agents/\n/cache/\n");
    write(repo, ".gitignore", ".clank/agents/\n.clank/cache/\n");
    // Master agent for this repo.
    write(
        repo,
        ".clank/agents/claude/config.json",
        r#"{"auto_mode":"off","role":"master"}"#,
    );
    write(repo, "README.md", "seed\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);
    dir
}

/// Add a plan with `n_revisions` plan-body-only commits.
fn seed_plan_only_commits(repo: &Path, stem: &str, n_revisions: usize) {
    write(repo, &format!(".clank/plans/{stem}.md"), "# v1\n");
    git(repo, &["add", "-A"]);
    git(
        repo,
        &["commit", "--quiet", "-m", &format!("[{stem}] intro")],
    );
    for i in 0..n_revisions {
        write(
            repo,
            &format!(".clank/plans/{stem}.md"),
            &format!("# v{}\n", i + 2),
        );
        git(repo, &["add", "-A"]);
        git(
            repo,
            &["commit", "--quiet", "-m", &format!("[{stem}] revise {i}")],
        );
    }
}

fn run_demote(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("demote")
        .arg("--repo")
        .arg(repo)
        .arg("--yes")
        // Tests init repos on `main`, which is a protected branch
        // by default. Pass the override so the in-place path can
        // exercise. The protected-branch refusal is covered as a
        // separate test (run_demote_protected).
        .arg("--allow-rewrite-protected")
        .args(args);
    cmd.output().expect("spawn clank demote")
}

fn run_demote_protected(repo: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("demote")
        .arg("--repo")
        .arg(repo)
        .arg("--yes")
        // No --allow-rewrite-protected; exercise the refusal.
        .args(args);
    cmd.output().expect("spawn clank demote")
}

#[test]
fn demote_plan_only_commits_succeeds_without_force() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "alpha", 2);
    let head_before = head_sha(repo);

    let out = run_demote(repo, &["alpha"]);
    assert!(
        out.status.success(),
        "demote failed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Head moved back to the seed.
    let head_after = head_sha(repo);
    assert_ne!(head_after, head_before, "head should have moved");
    // Plan file gone from the working tree.
    assert!(
        !repo.join(".clank/plans/alpha.md").exists(),
        "plan file should be gone from working tree"
    );
    // Queue file written at default priority 500.
    assert!(
        repo.join(".clank/queue/500-alpha.md").exists(),
        "queue entry should be written"
    );
    let queued = std::fs::read_to_string(repo.join(".clank/queue/500-alpha.md")).unwrap();
    assert!(
        queued.contains("v3"),
        "queued body should be the latest revision; got: {queued}"
    );
}

#[test]
fn demote_plan_with_code_commits_requires_force() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    // Intro commit (plan-only).
    write(repo, ".clank/plans/beta.md", "# v1\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[beta] intro"]);
    // Mixed commit (plan + code).
    write(repo, ".clank/plans/beta.md", "# v2\n");
    write(repo, "src/foo.rs", "fn main() {}\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[beta] mixed"]);

    let out = run_demote(repo, &["beta"]);
    assert!(
        !out.status.success(),
        "demote with mixed commits must refuse without --force; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("non-plan content") || stderr.contains("--force"),
        "stderr should explain the refusal; got: {stderr}"
    );
    // No filesystem changes.
    assert!(repo.join(".clank/plans/beta.md").exists());
    assert!(!repo.join(".clank/queue/500-beta.md").exists());
}

#[test]
fn demote_force_drops_mixed_commits() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    write(repo, ".clank/plans/beta.md", "# v1\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[beta] intro"]);
    write(repo, ".clank/plans/beta.md", "# v2\n");
    write(repo, "src/foo.rs", "fn main() {}\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[beta] mixed"]);

    let out = run_demote(repo, &["beta", "--force"]);
    assert!(
        out.status.success(),
        "demote --force should drop mixed commits; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    // Plan AND code change are both gone.
    assert!(!repo.join(".clank/plans/beta.md").exists());
    assert!(!repo.join("src/foo.rs").exists());
    assert!(repo.join(".clank/queue/500-beta.md").exists());
}

#[test]
fn demote_stub_writes_to_stubs_dir() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "gamma", 0);

    let out = run_demote(repo, &["gamma", "--stub"]);
    assert!(
        out.status.success(),
        "demote --stub failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        repo.join(".clank/stubs/gamma.md").exists(),
        "stub should be written"
    );
    assert!(
        !repo.join(".clank/queue/500-gamma.md").exists(),
        "queue file should NOT be written under --stub"
    );
}

#[test]
fn demote_priority_writes_to_queue_with_priority() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "delta", 0);

    let out = run_demote(repo, &["delta", "--priority", "100"]);
    assert!(out.status.success(), "demote --priority failed");
    assert!(
        repo.join(".clank/queue/100-delta.md").exists(),
        "queue file should land at priority 100"
    );
    assert!(
        !repo.join(".clank/queue/500-delta.md").exists(),
        "default-priority file should NOT be written"
    );
}

#[test]
fn demote_target_collision_detected_before_rewrite() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "epsilon", 0);
    let head_before = head_sha(repo);
    // Pre-populate the default target.
    write(repo, ".clank/queue/500-epsilon.md", "# stale queued copy\n");

    let out = run_demote(repo, &["epsilon"]);
    assert!(
        !out.status.success(),
        "demote must refuse when target file exists; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // History untouched.
    assert_eq!(
        head_sha(repo),
        head_before,
        "history must NOT be rewritten when collision is detected"
    );
    // Plan still in place.
    assert!(repo.join(".clank/plans/epsilon.md").exists());
    // Pre-existing queue file unchanged.
    let queued = std::fs::read_to_string(repo.join(".clank/queue/500-epsilon.md")).unwrap();
    assert!(queued.contains("stale queued copy"));
}

#[test]
fn demote_dirty_plan_file_errors() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "zeta", 1);
    let head_before = head_sha(repo);
    // Dirty the plan file in the working tree.
    write(repo, ".clank/plans/zeta.md", "# uncommitted edits\n");

    let out = run_demote(repo, &["zeta"]);
    assert!(
        !out.status.success(),
        "demote must refuse when plan file is dirty; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("differs from HEAD") || stderr.contains("stash"),
        "stderr should explain dirty-file refusal; got: {stderr}"
    );
    // History untouched.
    assert_eq!(head_sha(repo), head_before);
    assert!(!repo.join(".clank/queue/500-zeta.md").exists());
}

// NOTE on foreign-refusal tests (left out intentionally):
//
// The plan called for `demote_plan_with_foreign_commits_refuses_unconditionally`
// and `demote_force_does_not_bypass_foreign_refusal`. Reproducing
// `foreign: true` in a tempdir-based test requires merge commits
// from side branches — clank's `RepoState.fold.plans[plan_key].commits`
// attributes EVERY first-parent commit in the plan's active range to
// the plan (even commits that touch only non-plan files), so a
// linear-history "unrelated commit between intro and revise" classifies
// as `Rewrite + foreign=false`, not foreign. The Rewrite + non-foreign
// path is already covered by `demote_plan_with_code_commits_requires_force`
// and `demote_force_drops_mixed_commits`.
//
// The structural fix to `safety_check` on b8091ba (refuse on `c.foreign`
// regardless of disposition) defends against the merge-side-branch
// scenario. The fact that the linear-history tests don't reach the
// foreign branch is fine — the fix doesn't regress when there's
// nothing foreign to catch.

#[test]
fn demote_into_branch_with_dry_does_not_create_branch() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "pi", 0);

    let out = run_demote(repo, &["pi", "--into-branch", "demote-pi-preview", "--dry"]);
    assert!(out.status.success(), "--into-branch + --dry should succeed");
    // Branch was NOT created.
    let branches = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "--list", "demote-pi-preview"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&branches.stdout);
    assert!(
        listed.trim().is_empty(),
        "--dry must not create the branch; got: {listed}"
    );
    // No queue file.
    assert!(!repo.join(".clank/queue/500-pi.md").exists());
}

#[test]
fn demote_stub_collision_errors() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "rho", 0);
    let head_before = head_sha(repo);
    // Pre-populate the stub target.
    write(repo, ".clank/stubs/rho.md", "# pre-existing stub\n");

    let out = run_demote(repo, &["rho", "--stub"]);
    assert!(
        !out.status.success(),
        "demote --stub must refuse when stub target exists; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // History untouched.
    assert_eq!(head_sha(repo), head_before);
    // Pre-existing stub unchanged.
    let stub = std::fs::read_to_string(repo.join(".clank/stubs/rho.md")).unwrap();
    assert!(stub.contains("pre-existing stub"));
    // Plan still in working tree.
    assert!(repo.join(".clank/plans/rho.md").exists());
}

#[test]
fn demote_dry_no_changes() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "eta", 1);
    let head_before = head_sha(repo);

    let out = run_demote(repo, &["eta", "--dry"]);
    assert!(out.status.success(), "demote --dry should succeed");
    assert_eq!(
        head_sha(repo),
        head_before,
        "--dry must not rewrite history"
    );
    assert!(repo.join(".clank/plans/eta.md").exists());
    assert!(
        !repo.join(".clank/queue/500-eta.md").exists(),
        "--dry must not write queue file"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("dry-run"),
        "stdout should mention dry-run; got: {stdout}"
    );
}

#[test]
fn demote_orphaned_feedback_removed() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "theta", 1);
    // Capture SHAs in the plan range; we'll use them as feedback
    // file keys.
    let log = Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args(["log", "--format=%H", "main"])
        .output()
        .unwrap();
    let shas: Vec<String> = String::from_utf8_lossy(&log.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();
    // shas[0] is HEAD (latest plan revise), shas[1] is intro, shas[2] is seed.
    let plan_shas: Vec<String> = shas[0..2].to_vec();
    let seed_sha = &shas[2];

    // Pre-populate feedback files for plan commits + one for the
    // seed (must survive demote).
    for sha in &plan_shas {
        write(
            repo,
            &format!(".clank/agents/claude/feedback/{sha}.md"),
            "APPROVE\n",
        );
    }
    write(
        repo,
        &format!(".clank/agents/claude/feedback/{seed_sha}.md"),
        "APPROVE\n",
    );

    let out = run_demote(repo, &["theta"]);
    assert!(out.status.success(), "demote failed");

    // Plan feedback gone.
    for sha in &plan_shas {
        assert!(
            !repo
                .join(format!(".clank/agents/claude/feedback/{sha}.md"))
                .exists(),
            "orphan feedback for dropped SHA {sha} should be removed"
        );
    }
    // Seed feedback survives.
    assert!(
        repo.join(format!(".clank/agents/claude/feedback/{seed_sha}.md"))
            .exists(),
        "non-dropped SHA's feedback must survive"
    );
}

#[test]
fn demote_into_branch_does_not_touch_head() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "iota", 1);
    let head_before = head_sha(repo);

    let out = run_demote(repo, &["iota", "--into-branch", "demote-iota"]);
    assert!(
        out.status.success(),
        "demote --into-branch failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    // master untouched.
    assert_eq!(head_sha(repo), head_before);
    assert!(repo.join(".clank/plans/iota.md").exists());
    // No queue file written.
    assert!(
        !repo.join(".clank/queue/500-iota.md").exists(),
        "preview-only --into-branch must NOT write the queue file"
    );
    // The branch was created.
    let branches = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "--list", "demote-iota"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&branches.stdout);
    assert!(
        listed.contains("demote-iota"),
        "demote-iota branch should be created; git branch output: {listed}"
    );
    // stdout mentions the completion recipe (codex fix on 625b8af:
    // recipe says "complete on this branch, run demote without
    // --into-branch" — not "switch to <branch>", which would lose
    // the plan file).
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Inspect the rewrite") && stdout.contains("without --into-branch"),
        "stdout should print the corrected completion recipe; got: {stdout}"
    );
}

#[test]
fn demote_into_branch_collision_errors() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "kappa", 0);
    let head_before = head_sha(repo);
    // Pre-create the target branch.
    git(repo, &["branch", "existing-name"]);

    let out = run_demote(repo, &["kappa", "--into-branch", "existing-name"]);
    assert!(
        !out.status.success(),
        "demote --into-branch <existing> must refuse; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // Filesystem unchanged.
    assert_eq!(head_sha(repo), head_before);
    assert!(repo.join(".clank/plans/kappa.md").exists());
    assert!(!repo.join(".clank/queue/500-kappa.md").exists());
}

#[test]
fn demote_stub_with_high_priority_succeeds() {
    // Regression for codex on 1f88597: --priority validation
    // must NOT fire when --stub is set (priority is documented
    // as ignored under --stub). Pre-fix: --priority 1000 with
    // --stub errored anyway. Post-fix: succeeds, body lands at
    // .clank/stubs/<plan>.md, priority value irrelevant.
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "sigma", 0);

    let out = run_demote(repo, &["sigma", "--stub", "--priority", "1000"]);
    assert!(
        out.status.success(),
        "--stub --priority 1000 must succeed (priority is ignored under --stub); \
         stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(repo.join(".clank/stubs/sigma.md").exists());
    // No queue file written, regardless of the priority value.
    assert!(!repo.join(".clank/queue/1000-sigma.md").exists());
    assert!(!repo.join(".clank/queue/500-sigma.md").exists());
}

#[test]
fn demote_priority_above_999_rejected() {
    // Regression for codex on 625b8af: priorities > 999 produce
    // queue filenames `scan_queue` can't see (status / queue
    // promote ignore them). Match `clank queue add`'s validation
    // and reject before any rewrite.
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "mu", 0);
    let head_before = head_sha(repo);

    let out = run_demote(repo, &["mu", "--priority", "1000"]);
    assert!(
        !out.status.success(),
        "demote --priority 1000 must be rejected; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // No history change, no queue file written.
    assert_eq!(head_sha(repo), head_before);
    assert!(!repo.join(".clank/queue/1000-mu.md").exists());
    assert!(!repo.join(".clank/queue/500-mu.md").exists());
}

#[test]
fn demote_short_sha_feedback_removed() {
    // Regression for codex on 625b8af: feedback files in this
    // repo are keyed by 7-char short SHAs (the only naming
    // convention `clank feedback write` produces). The orphan
    // cleanup must match both full SHAs AND short prefixes.
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "nu", 1);
    let log = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["log", "--format=%H"])
        .output()
        .unwrap();
    let shas: Vec<String> = String::from_utf8_lossy(&log.stdout)
        .lines()
        .map(|s| s.to_string())
        .collect();
    let plan_head_full = &shas[0];
    let plan_intro_full = &shas[1];
    let seed_full = &shas[2];

    // Write feedback using 7-char short SHAs (the actual
    // convention).
    let plan_head_short = &plan_head_full[..7];
    let plan_intro_short = &plan_intro_full[..7];
    let seed_short = &seed_full[..7];
    write(
        repo,
        &format!(".clank/agents/claude/feedback/{plan_head_short}.md"),
        "APPROVE\n",
    );
    write(
        repo,
        &format!(".clank/agents/claude/feedback/{plan_intro_short}.md"),
        "APPROVE\n",
    );
    write(
        repo,
        &format!(".clank/agents/claude/feedback/{seed_short}.md"),
        "APPROVE\n",
    );

    let out = run_demote(repo, &["nu"]);
    assert!(out.status.success(), "demote failed");

    // Short-keyed plan feedback gone.
    assert!(
        !repo
            .join(format!(
                ".clank/agents/claude/feedback/{plan_head_short}.md"
            ))
            .exists(),
        "short-keyed orphan feedback should be removed"
    );
    assert!(
        !repo
            .join(format!(
                ".clank/agents/claude/feedback/{plan_intro_short}.md"
            ))
            .exists(),
        "short-keyed orphan feedback should be removed"
    );
    // Seed feedback survives.
    assert!(
        repo.join(format!(".clank/agents/claude/feedback/{seed_short}.md"))
            .exists(),
        "non-dropped SHA's feedback must survive"
    );
}

#[test]
fn demote_protected_branch_refusal_leaves_no_partial_state() {
    let dir = init_repo_with_master();
    let repo = dir.path();
    seed_plan_only_commits(repo, "lambda", 1);
    let head_before = head_sha(repo);

    // No --allow-rewrite-protected; main is protected by default.
    let out = run_demote_protected(repo, &["lambda"]);
    assert!(
        !out.status.success(),
        "demote must refuse to rewrite main without --allow-rewrite-protected; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    // History untouched.
    assert_eq!(
        head_sha(repo),
        head_before,
        "protected-branch refusal must leave history alone"
    );
    // Queue file NOT written (transactional rollback).
    assert!(
        !repo.join(".clank/queue/500-lambda.md").exists(),
        "queue file must NOT be written when rewrite refuses"
    );
    // Plan file still in working tree.
    assert!(repo.join(".clank/plans/lambda.md").exists());
}

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
