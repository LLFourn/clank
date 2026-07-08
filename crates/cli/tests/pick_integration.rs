//! In-process tests for `clank pick`. Drives `pick::run` directly — no
//! binary spawning; git is spawned for fixture setup (allowed).

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
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

fn pick_args(repo: &Path, plans: &[&str], from: &str) -> clank::cli::PickArgs {
    clank::cli::PickArgs {
        plans: plans.iter().map(|s| s.to_string()).collect(),
        from: from.to_string(),
        repo: Some(repo.to_path_buf()),
        squash: false,
        purge: false,
        dry: false,
    }
}

/// base (main) → side branch with plan `foo` (intro + one code commit) →
/// back on main. Returns the side branch's tip sha.
fn side_branch_with_foo(env: &TestEnv) -> String {
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, ".clank/plans/foo.md", "# foo\n\nthe foo plan\n");
    commit(repo, "[foo] intro");
    write(repo, "src/foo.rs", "// foo impl\n");
    commit(repo, "[foo] implement");
    let tip = git_out(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "main"]);
    // The target moves on from the branch point — the representative case
    // (and it forces genuinely new shas on the copies; picking onto an
    // IDENTICAL base in the same second can reproduce identical commits).
    write(
        repo,
        "main.txt",
        "main moved on
",
    );
    commit(repo, "main work");
    tip
}

#[test]
fn pick_copies_an_active_plan_and_leaves_the_source_untouched() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let side_tip = side_branch_with_foo(&env);

    block_on(clank::cli::pick::run(pick_args(repo, &["foo"], "side")))
        .expect("pick should succeed");

    // Target: plan file present, both commits replayed in order.
    assert!(repo.join(".clank/plans/foo.md").exists());
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "4");
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] implement"
    );
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s", "HEAD~1"]),
        "[foo] intro"
    );
    // New shas (a copy, not a ref move).
    assert_ne!(git_out(repo, &["rev-parse", "HEAD"]), side_tip);
    // Source byte-identical.
    assert_eq!(git_out(repo, &["rev-parse", "side"]), side_tip);
}

#[test]
fn pick_copies_a_finished_autosquashed_plan_as_one_commit() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "side"]);
    // A finished plan in the autosquash shape: ONE commit carrying code +
    // finished/<stem>.md (its portable unit).
    write(repo, "src/bar.rs", "// bar\n");
    write(repo, ".clank/finished/bar.md", "# bar\n\ndone\n");
    commit(repo, "[bar] whole plan as one commit");
    git(repo, &["checkout", "-q", "main"]);

    block_on(clank::cli::pick::run(pick_args(repo, &["bar"], "side"))).expect("pick finished plan");

    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "2");
    assert!(repo.join(".clank/finished/bar.md").exists());
    assert!(repo.join("src/bar.rs").exists());
}

#[test]
fn picks_replay_in_source_order_regardless_of_argument_order() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, ".clank/plans/first.md", "# first\n\nbody\n");
    commit(repo, "[first] intro");
    write(repo, ".clank/plans/second.md", "# second\n\nbody\n");
    commit(repo, "[second] intro");
    git(repo, &["checkout", "-q", "main"]);

    // Ask for second FIRST — replay must still be first, then second.
    block_on(clank::cli::pick::run(pick_args(
        repo,
        &["second", "first"],
        "side",
    )))
    .expect("multi-pick");

    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s", "HEAD~1"]),
        "[first] intro",
        "source order, not argument order"
    );
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[second] intro"
    );
}

#[test]
fn pick_refuses_a_stem_already_on_the_target() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let _ = side_branch_with_foo(&env);
    // An active `foo` on main too.
    write(repo, ".clank/plans/foo.md", "# foo local\n\nbody\n");
    commit(repo, "[foo] local intro");

    let err = block_on(clank::cli::pick::run(pick_args(repo, &["foo"], "side")))
        .expect_err("collision must refuse");
    assert!(err.to_string().contains("already ACTIVE"), "{err}");
}

#[test]
fn pick_refuses_interleaved_foreign_commits_and_names_them() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    write(repo, "src/stray.rs", "// stray\n");
    commit(repo, "stray mid-plan commit");
    let stray = git_out(repo, &["rev-parse", "HEAD"]);
    write(repo, ".clank/plans/foo.md", "# foo v2\n\nbody\n");
    commit(repo, "[foo] revise");
    git(repo, &["checkout", "-q", "main"]);

    let err = block_on(clank::cli::pick::run(pick_args(repo, &["foo"], "side")))
        .expect_err("interleaved foreign refuses");
    let text = err.to_string();
    assert!(text.contains("interleaved"), "{text}");
    assert!(
        text.contains(&stray[..7]) && text.contains("stray mid-plan commit"),
        "offender named: {text}"
    );
}

#[test]
fn pick_dry_is_a_strict_noop() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let _ = side_branch_with_foo(&env);
    let head_before = git_out(repo, &["rev-parse", "HEAD"]);

    let mut args = pick_args(repo, &["foo"], "side");
    args.dry = true;
    block_on(clank::cli::pick::run(args)).expect("dry pick");

    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head_before);
    assert!(!repo.join(".clank/plans/foo.md").exists());
}

#[test]
fn pick_dry_refuses_a_dirty_tree_exactly_like_the_live_run() {
    // Dry equals execute: a preview must never green-light an operation
    // the real run refuses (ruthless aeb97ff).
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let _ = side_branch_with_foo(&env);
    write(
        repo,
        "main.txt",
        "uncommitted edit
",
    );

    for dry in [true, false] {
        let mut args = pick_args(repo, &["foo"], "side");
        args.dry = dry;
        let err = block_on(clank::cli::pick::run(args)).expect_err("dirty refuses");
        assert!(err.to_string().contains("working tree dirty"), "{err}");
    }
}

#[test]
fn pick_conflict_stops_with_abort_guidance_and_source_untouched() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    // Conflicting file content on both branches.
    write(repo, "shared.txt", "base\n");
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    write(repo, "shared.txt", "side version\n");
    commit(repo, "[foo] intro + edit shared");
    let side_tip = git_out(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "main"]);
    write(repo, "shared.txt", "main version\n");
    commit(repo, "diverge shared");

    let err = block_on(clank::cli::pick::run(pick_args(repo, &["foo"], "side")))
        .expect_err("conflict surfaces");
    let text = err.to_string();
    assert!(text.contains("git cherry-pick --abort"), "{text}");
    assert_eq!(
        git_out(repo, &["rev-parse", "side"]),
        side_tip,
        "source untouched"
    );
    // Leave the fixture's cherry-pick state cleanly for the tempdir drop.
    git(repo, &["cherry-pick", "--abort"]);
}

// ── pick-purge-and-squash ──

/// side branch with plan `bar`: intro (.clank only), code commit, and a
/// clank-only body revision — the shape that exercises strip + drop.
fn side_branch_with_bar(env: &TestEnv) -> String {
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, ".clank/plans/bar.md", "# bar\n\nthe bar plan\n");
    commit(repo, "[bar] intro");
    write(repo, "src/bar.rs", "// bar impl\n");
    commit(repo, "[bar] implement");
    write(repo, ".clank/plans/bar.md", "# bar\n\nrevised body\n");
    commit(repo, "[bar] revise plan");
    let tip = git_out(repo, &["rev-parse", "HEAD"]);
    git(repo, &["checkout", "-q", "main"]);
    write(repo, "main.txt", "main moved on\n");
    commit(repo, "main work");
    tip
}

#[test]
fn pick_squash_collapses_a_plan_into_one_tagged_commit() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let side_tip = side_branch_with_bar(&env);

    let mut args = pick_args(repo, &["bar"], "side");
    args.squash = true;
    block_on(clank::cli::pick::run(args)).expect("squash pick succeeds");

    // ONE commit for the whole plan, tagged (it carries the plan file).
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
    let subject = git_out(repo, &["log", "-1", "--format=%s"]);
    assert!(subject.starts_with("[bar] "), "tagged: {subject}");
    let body = git_out(repo, &["log", "-1", "--format=%b"]);
    assert!(
        body.contains("clank pick --squash"),
        "provenance WHY for an unfinished plan: {body}"
    );
    // The collapsed tree carries the plan's END state.
    assert!(repo.join("src/bar.rs").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join(".clank/plans/bar.md")).unwrap(),
        "# bar\n\nrevised body\n"
    );
    assert_eq!(
        git_out(repo, &["rev-parse", "side"]),
        side_tip,
        "source untouched"
    );
}

#[test]
fn pick_purge_strips_clank_and_drops_empty_commits() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    side_branch_with_bar(&env);

    let mut args = pick_args(repo, &["bar"], "side");
    args.purge = true;
    block_on(clank::cli::pick::run(args)).expect("purge pick succeeds");

    // Only the code commit lands: intro and the body revision are
    // .clank-only, so they are EMPTY after the strip and drop.
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[bar] implement"
    );
    assert!(repo.join("src/bar.rs").exists());
    let tree = git_out(repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tree.contains(".clank/plans/bar.md"),
        "the pick's plan artifacts never land: {tree}"
    );
    // The TARGET's own tracked .clank content is preserved — the strip
    // set is what the pick INTRODUCES, never the target's files (the
    // fixture's `git add -A` tracks .clank/config.json on main).
    assert!(
        tree.contains(".clank/config.json"),
        "target-owned .clank content preserved: {tree}"
    );
}

#[test]
fn pick_purge_squash_yields_one_clean_commit() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    side_branch_with_bar(&env);

    let mut args = pick_args(repo, &["bar"], "side");
    args.purge = true;
    args.squash = true;
    block_on(clank::cli::pick::run(args)).expect("purge+squash pick succeeds");

    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
    let subject = git_out(repo, &["log", "-1", "--format=%s"]);
    assert!(
        !subject.starts_with("[bar]"),
        "untagged: the commit carries no plan file: {subject}"
    );
    assert!(repo.join("src/bar.rs").exists());
    let tree = git_out(repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        !tree.contains(".clank/plans/bar.md"),
        "clean commit — no plan artifacts: {tree}"
    );
}

#[test]
fn pick_dry_squash_purge_previews_the_shape_and_is_a_noop() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    side_branch_with_bar(&env);
    let before = git_out(repo, &["rev-parse", "HEAD"]);

    for (squash, purge) in [(true, false), (false, true), (true, true)] {
        let mut args = pick_args(repo, &["bar"], "side");
        args.squash = squash;
        args.purge = purge;
        args.dry = true;
        block_on(clank::cli::pick::run(args)).expect("dry succeeds");
        assert_eq!(
            git_out(repo, &["rev-parse", "HEAD"]),
            before,
            "--dry is a strict no-op"
        );
        // Tracked state untouched (the fold's untracked .clank/cache
        // is clank's own working data, not a pick side effect).
        assert_eq!(git_out(repo, &["status", "--porcelain", "-uno"]).trim(), "");
    }

    // Dry-vs-live agreement (one-computation): the squash dry names one
    // collapsed commit; the live run then produces exactly one commit.
    let mut args = pick_args(repo, &["bar"], "side");
    args.squash = true;
    block_on(clank::cli::pick::run(args)).expect("live squash");
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn pick_collapse_refuses_interleaved_plans() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    git(repo, &["checkout", "-q", "-b", "side"]);
    write(repo, ".clank/plans/a.md", "# a\n\nplan a\n");
    commit(repo, "[a] intro");
    write(repo, ".clank/plans/b.md", "# b\n\nplan b\n");
    commit(repo, "[b] intro");
    write(repo, "a.rs", "// a\n");
    commit(repo, "[a] impl");
    write(repo, "b.rs", "// b\n");
    commit(repo, "[b] impl");
    git(repo, &["checkout", "-q", "main"]);

    let mut args = pick_args(repo, &["a", "b"], "side");
    args.squash = true;
    let err = block_on(clank::cli::pick::run(args))
        .unwrap_err()
        .to_string();
    assert!(err.contains("interleave"), "{err}");
    // Plain pick of the same pair still works.
    block_on(clank::cli::pick::run(pick_args(repo, &["a", "b"], "side")))
        .expect("plain pick of interleaved plans");
}
