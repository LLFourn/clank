//! In-process tests for `clank finish`. Drives `finish::run` directly — no
//! binary spawning; git is spawned for fixture setup (allowed).

use crate::common;

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

fn finish_args(repo: &Path, plan: &str, squash: Option<&str>) -> clank::cli::FinishArgs {
    clank::cli::FinishArgs {
        plan: Some(plan.into()),
        repo: Some(repo.to_path_buf()),
        message: vec![],
        purge: false,
        squash: squash.map(str::to_string),
        no_squash: false,
        into_branch: None,
        dry: false,
        force: false,
        no_cache: true,
    }
}

#[test]
fn finish_refuses_an_unreviewed_plan_and_hints_force() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["finish foo now".into(), "it is wanted regardless".into()];
    let err = block_on(clank::cli::finish::run(args)).expect_err("unreviewed must refuse");
    let text = err.to_string();
    assert!(text.contains("hasn't been reviewed"), "{text}");
    assert!(
        text.contains("--force"),
        "waivable refusal must hint --force: {text}"
    );
}

#[test]
fn finish_force_bypasses_the_review_gate() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["ship foo anyway".into(), "lloyd's call".into()];
    args.force = true;
    block_on(clank::cli::finish::run(args)).expect("force finish on unreviewed gate");

    assert!(repo.join(".clank/finished/foo.md").exists());
    assert!(!repo.join(".clank/plans/foo.md").exists());
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] ship foo anyway",
        "the finalize commit carries the forced message"
    );
}

#[test]
fn finish_force_still_refuses_a_dirty_plan_file() {
    // Force waives the review gate ONLY — file-safety refusals stand,
    // and a mixed blocker set refuses whole (force never partially
    // applies).
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    write(
        repo,
        ".clank/plans/foo.md",
        "# foo\n\nEDITED, uncommitted\n",
    );

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["finish foo now".into(), "it is wanted regardless".into()];
    args.force = true;
    let err = block_on(clank::cli::finish::run(args)).expect_err("dirty plan file must refuse");
    let text = err.to_string();
    assert!(text.contains("uncommitted"), "{text}");
    assert!(
        !text.contains("--force finalizes anyway"),
        "no force hint when force can't apply: {text}"
    );
}

#[test]
fn finish_force_still_refuses_an_open_block() {
    // An open block is a pending HUMAN question, not a review verdict —
    // force must not bury it.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    write(
        repo,
        ".clank/agents/codex/blocks/which-api.md",
        "Which API shape?\n",
    );

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["finish foo now".into(), "it is wanted regardless".into()];
    args.force = true;
    let err = block_on(clank::cli::finish::run(args)).expect_err("open block must refuse");
    assert!(err.to_string().contains("block"), "{err}");
}

#[test]
fn finish_refuses_an_open_block_on_a_master_only_repo() {
    // codex 137bd3e: the first block fix rode the gate state, which a
    // master-only repo waives entirely (`any_registered_reviewers`),
    // so the finalize still buried the question. OpenBlock is its own
    // unconditional reason — and --force must not waive it either.
    let env = TestEnv::init();
    env.register_team("claude", &[], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    write(
        repo,
        ".clank/agents/claude/blocks/which-api.md",
        "Which API shape?\n",
    );

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["finish foo now".into(), "it is wanted regardless".into()];
    let err = block_on(clank::cli::finish::run(args)).expect_err("master-only block must refuse");
    assert!(err.to_string().contains("block"), "{err}");

    let mut forced = finish_args(repo, "foo", None);
    forced.message = vec!["finish foo now".into(), "it is wanted regardless".into()];
    forced.force = true;
    let err =
        block_on(clank::cli::finish::run(forced)).expect_err("force must not waive the block");
    assert!(err.to_string().contains("block"), "{err}");
    assert!(repo.join(".clank/plans/foo.md").exists(), "not finalized");
}

#[test]
fn finish_refuses_an_open_block_even_when_the_gate_is_finished() {
    // Pre-force, finish never consulted blocks — a finalize could bury
    // an unanswered human question. Block precedence now mirrors
    // derive_status: pending block dominates ANY gate verdict.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    let intro = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &intro[..7]),
        "FINISHED ship it\n",
    );
    write(
        repo,
        ".clank/agents/codex/blocks/which-api.md",
        "Which API shape?\n",
    );

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["finish foo now".into(), "it is wanted regardless".into()];
    let err = block_on(clank::cli::finish::run(args)).expect_err("open block dominates FINISHED");
    assert!(err.to_string().contains("block"), "{err}");
}

/// `--purge` on a dirty repo, on the current branch: it DOES change
/// the tree, so the worktree would have to move and the refusal
/// stands — and because the refusal is found first, nothing is
/// finalized and not one byte of the uncommitted work is touched.
///
/// The other refusal tests reach the blocker a different way (an
/// existing `--into-branch`, which is rejected before the tree is ever
/// considered) or call the engine directly, below the boundary this
/// protects (codex on eca016b).
#[test]
fn a_dirty_purge_refuses_and_leaves_everything_where_it_was() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    let intro = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &intro[..7]),
        "FINISHED ship it\n",
    );

    write(repo, "README", "base, edited\n");
    write(repo, "staged.rs", "// staged\n");
    git(repo, &["add", "staged.rs"]);
    write(repo, "loose.txt", "loose\n");
    let before = (
        git_out(repo, &["rev-parse", "HEAD"]),
        git_out(repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        git_out(repo, &["diff"]),
        git_out(repo, &["diff", "--cached"]),
        std::fs::read(repo.join("loose.txt")).unwrap(),
    );
    assert!(
        !before.2.is_empty() && !before.3.is_empty(),
        "the fixture must be dirty both ways"
    );

    let mut args = finish_args(repo, "foo", None);
    args.purge = true;
    args.message = vec![
        "strip foo artifacts".into(),
        "the plan is done and the bookkeeping is noise".into(),
    ];
    let err = block_on(clank::cli::finish::run(args))
        .expect_err("a purge changes the tree, so it needs a clean worktree");
    assert!(
        err.to_string().contains("working tree dirty"),
        "with the message it always had, got: {err}"
    );

    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        before.0,
        "HEAD unmoved"
    );
    assert_eq!(
        git_out(repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        before.1,
        "branch unmoved"
    );
    assert!(
        repo.join(".clank/plans/foo.md").exists(),
        "the plan is still active"
    );
    assert!(
        !repo.join(".clank/finished/foo.md").exists(),
        "and nothing was finalized"
    );
    assert_eq!(
        git_out(repo, &["diff"]),
        before.2,
        "unstaged work untouched"
    );
    assert_eq!(
        git_out(repo, &["diff", "--cached"]),
        before.3,
        "staged work untouched"
    );
    assert_eq!(
        std::fs::read(repo.join("loose.txt")).unwrap(),
        before.4,
        "and the untracked file, byte for byte"
    );
}

/// Autosquash is `--squash` by another route, and a dirty repo is no
/// more its business than it is `--squash`'s.
#[test]
fn autosquash_works_on_a_dirty_repo_too() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    write(repo, "src/foo.rs", "// foo\n");
    commit(repo, "[foo] implement");
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &head[..7]),
        "FINISHED ship it\n",
    );
    // `finish.autosquash` on, and a plain finish with a message.
    let cfg = std::fs::read_to_string(repo.join(".clank/config.json")).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
    v["finish"] = serde_json::json!({ "autosquash": true });
    write(
        repo,
        ".clank/config.json",
        &serde_json::to_string_pretty(&v).unwrap(),
    );

    write(repo, "README", "base, edited\n");
    let unstaged = git_out(repo, &["diff"]);
    assert!(!unstaged.is_empty(), "the fixture must be dirty");

    let mut args = finish_args(repo, "foo", None);
    args.message = vec![
        "all of it".into(),
        "it exists because the thing needed doing".into(),
    ];
    block_on(clank::cli::finish::run(args)).expect("autosquash on a dirty repo");

    assert_eq!(
        git_out(repo, &["rev-list", "--count", "HEAD"]),
        "2",
        "autosquash collapsed the plan to one commit"
    );
    assert_eq!(git_out(repo, &["diff"]), unstaged, "the edit survived");
}

/// `clank finish --squash` on a DIRTY repo: the plan finalizes, the
/// range collapses, and the uncommitted work is exactly where it was.
///
/// This used to finalize and THEN refuse, leaving the plan finished
/// but unsquashed. The refusal was over-broad — a squash lands the
/// same tree it started from, so the working copy never needed to
/// move.
#[test]
fn finish_squash_works_on_a_dirty_repo_and_keeps_the_work() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    write(repo, "src/foo.rs", "// foo\n");
    commit(repo, "[foo] implement");
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &head[..7]),
        "FINISHED ship it\n",
    );

    // Dirty, three ways.
    write(repo, "README", "base, edited\n");
    write(repo, "staged.rs", "// staged\n");
    git(repo, &["add", "staged.rs"]);
    write(repo, "loose.txt", "loose\n");
    let unstaged = git_out(repo, &["diff"]);
    let staged = git_out(repo, &["diff", "--cached"]);
    assert!(
        !unstaged.is_empty() && !staged.is_empty(),
        "dirty both ways"
    );

    let mut args = finish_args(repo, "foo", None);
    args.message = vec![
        "all of it".into(),
        "it exists because the thing needed doing".into(),
    ];
    args.squash = Some("[foo] all of it\n\nkept as one commit".into());
    block_on(clank::cli::finish::run(args)).expect("a dirty tree is no reason to refuse a squash");

    assert_eq!(
        git_out(repo, &["rev-list", "--count", "HEAD"]),
        "2",
        "the base, and the whole plan collapsed into one commit"
    );
    assert!(
        git_out(repo, &["log", "-1", "--format=%s"]).contains("all of it"),
        "the squash message landed"
    );
    assert_eq!(git_out(repo, &["diff"]), unstaged, "unstaged work survived");
    assert_eq!(
        git_out(repo, &["diff", "--cached"]),
        staged,
        "staged work survived"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("loose.txt")).unwrap(),
        "loose\n",
        "and the untracked file, byte for byte"
    );
}

#[test]
fn tui_composed_squash_message_squashes_a_no_squash_finished_plan() {
    // The TUI squash path end-to-end minus the widget
    // (tui-squash-message-body): a plan finished with --no-squash keeps
    // its commits; squashing later with a composed subject+finalize-body
    // message must pass validation and land the composed message.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(
        repo, "README", "base
",
    );
    commit(repo, "base");
    write(
        repo,
        ".clank/plans/foo.md",
        "# foo

body
",
    );
    commit(repo, "[foo] intro");
    write(
        repo,
        "src/foo.rs",
        "// foo
",
    );
    commit(repo, "[foo] implement");
    let intro_plus = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &intro_plus[..7]),
        "FINISHED ship it
",
    );
    let mut args = finish_args(repo, "foo", None);
    args.message = vec![
        "make foo work".into(),
        "foo exists because the thing needed doing".into(),
    ];
    args.no_squash = true;
    block_on(clank::cli::finish::run(args)).expect("no-squash finish");
    assert!(
        git_out(repo, &["rev-list", "--count", "HEAD"]) == "4",
        "commits kept"
    );

    // What the TUI submit arm does: typed subject + finalize body.
    let finalize_body = git_out(repo, &["log", "-1", "--format=%b"]);
    assert!(finalize_body.contains("needed doing"), "{finalize_body}");
    let composed = format!(
        "collapse foo

{finalize_body}"
    );
    let squash = finish_args(repo, "foo", Some(&composed));
    block_on(clank::cli::finish::run(squash)).expect("composed squash passes validation");

    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] collapse foo"
    );
    assert!(
        git_out(repo, &["log", "-1", "--format=%b"]).contains("needed doing"),
        "original WHY rides into the squash commit"
    );
}

#[test]
fn a_refused_squash_finalizes_nothing() {
    // codex 053f9d1: `finish --squash` stamps the transient finalize commit
    // with the (validated) squash message BEFORE the squash runs, so a squash
    // that is REFUSED (here: `--into-branch` naming a branch that already
    // exists) leaves that message on HEAD — never the `[stem] finish`
    // placeholder.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();

    // A plan with a reviewable commit + a FINISHED review → gate Ready.
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    let intro = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &intro[..7]),
        "FINISHED ship it\n",
    );

    let msg = "collapse foo into one\n\nso the branch reads as a single commit";
    // `--squash` finalizes (stamping the squash message), then the squash is
    // refused: the target branch exists.
    git_out(repo, &["branch", "taken"]);
    let mut args = finish_args(repo, "foo", Some(msg));
    args.into_branch = Some("taken".into());
    let err = block_on(clank::cli::finish::run(args))
        .expect_err("squash must be refused when the target branch exists");
    assert!(
        err.to_string().contains("already exists"),
        "expected the existing-branch refusal, got: {err}"
    );

    // Nothing landed. The refusal is found BEFORE the finalize commit
    // is written, so there is no message on HEAD to get right — which
    // is a stronger answer to codex 053f9d1 than stamping the correct
    // one on a commit the operator did not want.
    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        intro,
        "a refused squash finalizes nothing"
    );
    assert!(
        repo.join(".clank/plans/foo.md").exists(),
        "the plan is still active"
    );
    assert!(
        !repo.join(".clank/finished/foo.md").exists(),
        "and was not moved to finished/"
    );
}

#[test]
fn a_refused_purge_finalizes_nothing() {
    // codex 53a9edb: `--purge` on a READY plan CREATES the finalize commit,
    // then runs the strip rewrite — which can be refused (here: the
    // `--into-branch` target already exists), leaving the finalize commit
    // on HEAD. It must carry the validated `-m`, never `[stem] finish`.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();

    // A plan with a reviewable commit + a FINISHED review → gate Ready.
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    let intro = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &intro[..7]),
        "FINISHED ship it\n",
    );

    let msg = "strip foo artifacts\n\nthe plan is done and the .clank bookkeeping is noise";
    git_out(repo, &["branch", "taken"]);
    let mut args = finish_args(repo, "foo", None);
    args.purge = true;
    args.into_branch = Some("taken".into());
    args.message = vec![msg.to_string()];
    let err = block_on(clank::cli::finish::run(args))
        .expect_err("purge rewrite must be refused when the target branch exists");
    assert!(
        err.to_string().contains("already exists"),
        "expected the existing-branch refusal, got: {err}"
    );

    // Nothing landed — same as the squash case, and for the same
    // reason: the refusal is found before the finalize commit exists.
    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        intro,
        "a refused purge finalizes nothing"
    );
    assert!(
        repo.join(".clank/plans/foo.md").exists(),
        "the plan is still active"
    );
}

// Setup: a plan with a reviewable commit + a FINISHED review → gate Ready.
fn ready_plan(env: &TestEnv) {
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    let intro = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &intro[..7]),
        "FINISHED ship it\n",
    );
}

#[test]
fn finish_message_gets_the_plan_tag_even_when_untagged() {
    // ruthless 28e3be4: the finalize commit renames the plan file, so a custom
    // `-m` without `[<stem>]` must still land tagged — else fix_commit_tag.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    ready_plan(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["make foo work".into(), "because it was broken".into()];
    block_on(clank::cli::finish::run(args)).expect("finish should succeed");

    // A `[foo]`-tagged subject on the plan-file-touching finish commit is
    // exactly G==T — no fix_commit_tag.
    let subject = git_out(repo, &["log", "-1", "--format=%s"]);
    assert_eq!(
        subject, "[foo] make foo work",
        "finish subject must carry the plan tag"
    );
}

#[test]
fn squash_message_gets_the_plan_tag_even_when_untagged() {
    // The squash commit collapses the plan-file rename in, so an untagged
    // `--squash` MSG must land tagged too.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    ready_plan(&env);

    let mut args = finish_args(repo, "foo", None);
    args.squash = Some("collapse foo\n\na single commit reads cleaner".into());
    block_on(clank::cli::finish::run(args)).expect("squash finish should succeed");

    let subject = git_out(repo, &["log", "-1", "--format=%s"]);
    assert_eq!(
        subject, "[foo] collapse foo",
        "squash subject must carry the plan tag"
    );
}

// Merge `finish.autosquash` into the repo config WITHOUT clobbering the roster
// (register_team wrote the roster into the same .clank/config.json).
fn set_autosquash(repo: &Path, on: bool) {
    let p = repo.join(".clank/config.json");
    let mut v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap_or_else(|_| "{}".into())).unwrap();
    v["finish"] = serde_json::json!({ "autosquash": on });
    std::fs::write(&p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

// A base commit (so the plan intro isn't the root), then a Ready plan: intro
// + a FINISHED review on it. Call AFTER any config mutation so `base` commits
// a clean tree.
fn ready_plan_on_base(env: &TestEnv) {
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    let intro = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &intro[..7]),
        "FINISHED ship it\n",
    );
}

#[test]
fn autosquash_config_collapses_plan_to_one_tagged_commit() {
    // finish.autosquash on → a plain `finish -m …` squashes the plan into ONE
    // commit carrying the (auto-tagged) finish message.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "one commit reads cleaner".into()];
    block_on(clank::cli::finish::run(args)).expect("autosquash finish should succeed");

    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] wrap up foo",
        "squashed into one tagged commit"
    );
    // base + the single squashed plan commit (NOT base + intro + finish = 3).
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "2");
    assert!(
        repo.join(".clank/finished/foo.md").exists(),
        "finalize snapshot preserved"
    );
}

#[test]
fn without_autosquash_the_plan_keeps_its_commits() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, false);
    ready_plan_on_base(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "keep the commits".into()];
    block_on(clank::cli::finish::run(args)).expect("finish should succeed");

    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] wrap up foo"
    );
    // base + intro + finish — not squashed.
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn no_squash_flag_opts_out_of_autosquash() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "keep the commits this time".into()];
    args.no_squash = true;
    block_on(clank::cli::finish::run(args)).expect("finish should succeed");

    // --no-squash keeps the individual commits despite autosquash being on.
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn autosquash_with_no_message_still_rejects() {
    // -m stays mandatory under autosquash: no -m → args.squash stays None →
    // normal path → validation rejects.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let args = finish_args(repo, "foo", None); // no message
    let err = block_on(clank::cli::finish::run(args))
        .expect_err("no -m must reject even with autosquash on");
    assert!(
        err.to_string().contains("finish needs"),
        "educational message-required error, got: {err}"
    );
}

#[test]
fn finish_dash_m_rewords_a_plan_that_is_not_at_head() {
    // The user's complaint: `clank finish <plan> -m "..."` on a plan whose
    // finish commit is buried under later work must DWIM — reword it in place
    // and replay the stacked commits — not error "must be HEAD". With
    // autosquash the default, this is the common case.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    // Finish foo → collapses to ONE commit at HEAD (autosquash on protected main).
    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "the original summary".into()];
    block_on(clank::cli::finish::run(args)).expect("initial finish");
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "2");

    // Stack a later plan's commit on top, burying foo's finalize commit.
    write(repo, "src/later.rs", "// later work\n");
    commit(repo, "[bar] later work");
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");

    // Reword foo's (now buried) finish message. Bare -m, no --squash.
    let mut args = finish_args(repo, "foo", None);
    args.message = vec![
        "reworded foo summary".into(),
        "a clearer whole-plan why".into(),
    ];
    block_on(clank::cli::finish::run(args)).expect("off-head reword should DWIM, not error");

    // History unchanged in length; foo's finalize (HEAD~1) carries the new
    // message; the stacked `[bar]` commit is preserved on top.
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s", "HEAD~1"]),
        "[foo] reworded foo summary",
        "buried finalize was reworded in place"
    );
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[bar] later work",
        "the stacked commit is replayed unchanged on top"
    );
}

#[test]
fn finish_dash_m_rewords_when_the_finalize_is_head() {
    // Reword goes through ONE engine path regardless of position: when the
    // finalize IS HEAD it's a zero-descendant reword — message replaced,
    // finished file untouched, history length unchanged.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "the original summary".into()];
    block_on(clank::cli::finish::run(args)).expect("initial finish");
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "2");
    let finished_body = std::fs::read_to_string(repo.join(".clank/finished/foo.md")).unwrap();

    let mut args = finish_args(repo, "foo", None);
    args.message = vec![
        "better foo summary".into(),
        "a clearer why for the plan".into(),
    ];
    block_on(clank::cli::finish::run(args)).expect("at-HEAD reword");

    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] better foo summary"
    );
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(
        std::fs::read_to_string(repo.join(".clank/finished/foo.md")).unwrap(),
        finished_body,
        "reword must not touch the finished file"
    );
}

#[test]
fn finish_dash_m_dry_previews_the_reword_without_changing_anything() {
    // --dry threads into the engine's dry mode — the SAME computation the live
    // run applies, printed instead of executed. Nothing may move.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "the original summary".into()];
    block_on(clank::cli::finish::run(args)).expect("initial finish");
    write(repo, "src/later.rs", "// later work\n");
    commit(repo, "[bar] later work");
    let head_before = git_out(repo, &["rev-parse", "HEAD"]);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["reworded".into(), "a clearer whole-plan why".into()];
    args.dry = true;
    block_on(clank::cli::finish::run(args)).expect("dry reword");

    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        head_before,
        "--dry must not move any ref"
    );
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s", "HEAD~1"]),
        "[foo] wrap up foo",
        "--dry must not rewrite the finalize message"
    );
}

#[test]
fn autosquash_squashes_on_main() {
    // The natural clank workflow finalizes on the working branch, which
    // is `main`/`master`; collapsing the plan there is the point.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "one commit on main".into()];
    block_on(clank::cli::finish::run(args)).expect("autosquash squashes on main");

    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] wrap up foo"
    );
    assert_eq!(
        git_out(repo, &["rev-list", "--count", "HEAD"]),
        "2",
        "collapsed to base + one commit on main"
    );
}

#[test]
fn fresh_squash_dry_is_a_strict_noop_then_live_agrees() {
    // The one-computation rule for the FRESH composite: --dry runs the same
    // preview + engine path as live (over a synthetic finalize commit) and
    // mutates NOTHING; the live run then executes the plan the dry printed.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let head_before = git_out(repo, &["rev-parse", "HEAD"]);
    let refs_before = git_out(repo, &["for-each-ref"]);

    // Fresh --dry (autosquash fills squash + allow flag; plan not finalized).
    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "one commit reads cleaner".into()];
    args.dry = true;
    block_on(clank::cli::finish::run(args)).expect("fresh dry should succeed");

    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        head_before,
        "--dry must not move HEAD"
    );
    assert_eq!(
        git_out(repo, &["for-each-ref"]),
        refs_before,
        "--dry must not create or move any ref"
    );
    assert!(
        repo.join(".clank/plans/foo.md").exists(),
        "--dry must not finalize the plan file"
    );

    // The live run executes what the dry previewed: base + ONE squashed commit.
    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "one commit reads cleaner".into()];
    block_on(clank::cli::finish::run(args)).expect("live finish");
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] wrap up foo"
    );
}

#[test]
fn fresh_squash_dry_reports_the_same_blocker_live_refuses() {
    // Blocker parity: an explicit --squash --into-branch naming a branch
    // that exists. The live run refuses; the --dry — the same
    // computation — reports the blocker and changes nothing.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    ready_plan_on_base(&env);
    git_out(repo, &["branch", "taken"]);

    let head_before = git_out(repo, &["rev-parse", "HEAD"]);
    let msg = "collapse foo\n\nso the branch reads as one commit";
    let mut args = finish_args(repo, "foo", Some(msg));
    args.into_branch = Some("taken".into());
    args.dry = true;
    block_on(clank::cli::finish::run(args)).expect("dry prints blockers, exits clean");
    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        head_before,
        "blocked dry changes nothing"
    );
    assert!(repo.join(".clank/plans/foo.md").exists());

    let mut args = finish_args(repo, "foo", Some(msg));
    args.into_branch = Some("taken".into());
    let err = block_on(clank::cli::finish::run(args))
        .expect_err("live refuses when the target branch exists");
    assert!(err.to_string().contains("already exists"), "got: {err}");
}

#[test]
fn fresh_purge_dry_is_a_strict_noop() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    ready_plan_on_base(&env);

    let head_before = git_out(repo, &["rev-parse", "HEAD"]);
    let refs_before = git_out(repo, &["for-each-ref"]);
    let mut args = finish_args(repo, "foo", None);
    args.purge = true;
    args.dry = true;
    args.message = vec!["strip foo".into(), "bookkeeping is noise now".into()];
    block_on(clank::cli::finish::run(args)).expect("purge dry");

    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head_before);
    assert_eq!(git_out(repo, &["for-each-ref"]), refs_before);
    assert!(repo.join(".clank/plans/foo.md").exists());
}

#[test]
fn buried_squash_collapses_only_the_plans_own_range_and_restacks() {
    // The buried-plan squash bug: --squash must collapse [intro..finalized_at]
    // only — commits AFTER the plan are restacked individually (never
    // foreign-refused), and HEAD's tree is unchanged.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, false);
    ready_plan_on_base(&env);

    // Finish foo WITHOUT squash → base, intro, finalize = 3 commits.
    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "original two-commit shape".into()];
    block_on(clank::cli::finish::run(args)).expect("plain finish");
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");

    // Bury it under a later ad-hoc commit.
    write(repo, "src/later.rs", "// later\n");
    commit(repo, "later work");
    let tree_before = git_out(repo, &["rev-parse", "HEAD^{tree}"]);
    // Feedback on the later commit — must follow it through the restack.
    let later_sha = git_out(repo, &["rev-parse", "HEAD"]);
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{}.md", &later_sha[..7]),
        "APPROVE looks good\n",
    );

    // Retroactive squash of the BURIED plan.
    let msg = "collapse foo\n\nfoo reads as one commit now";
    let args = finish_args(repo, "foo", Some(msg));
    block_on(clank::cli::finish::run(args)).expect("buried squash must work");

    // base + squashed foo + restacked later = 3; HEAD tree unchanged.
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD^{tree}"]),
        tree_before,
        "plain squash must not change HEAD's tree"
    );
    assert_eq!(git_out(repo, &["log", "-1", "--format=%s"]), "later work");
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s", "HEAD~1"]),
        "[foo] collapse foo",
        "the plan's own two commits collapsed into one"
    );
    // The restacked commit's feedback migrated to its new sha.
    let new_later = git_out(repo, &["rev-parse", "HEAD"]);
    assert_ne!(new_later, later_sha, "restack rewrote the sha");
    let feedback_dir = repo.join(".clank/agents/codex/feedback");
    let migrated = std::fs::read_dir(&feedback_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with(&new_later[..7])
        });
    assert!(migrated, "feedback must follow the restacked commit");
}

#[test]
fn buried_squash_dry_is_a_noop_then_live_agrees() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, false);
    ready_plan_on_base(&env);
    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "original shape".into()];
    block_on(clank::cli::finish::run(args)).expect("plain finish");
    write(repo, "src/later.rs", "// later\n");
    commit(repo, "later work");

    let head_before = git_out(repo, &["rev-parse", "HEAD"]);
    let msg = "collapse foo\n\nfoo reads as one commit now";
    let mut args = finish_args(repo, "foo", Some(msg));
    args.dry = true;
    block_on(clank::cli::finish::run(args)).expect("dry buried squash");
    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        head_before,
        "--dry must not move HEAD"
    );

    let args = finish_args(repo, "foo", Some(msg));
    block_on(clank::cli::finish::run(args)).expect("live executes what dry previewed");
    assert_eq!(git_out(repo, &["rev-list", "--count", "HEAD"]), "3");
}

#[test]
fn buried_squash_refuses_interleaved_foreign_and_names_it() {
    // A foreign commit INSIDE [intro..finalized_at] still refuses (tree
    // replay can't preserve it) — and the error NAMES the offender.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, "README", "base\n");
    commit(repo, "base");
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    // Interleaved mid-plan ad-hoc commit (untagged → foreign to foo).
    write(repo, "src/stray.rs", "// stray\n");
    commit(repo, "stray mid-plan commit");
    let stray_sha = git_out(repo, &["rev-parse", "HEAD"]);
    // Simulate the finalize.
    std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
    std::fs::rename(
        repo.join(".clank/plans/foo.md"),
        repo.join(".clank/finished/foo.md"),
    )
    .unwrap();
    commit(repo, "[foo] finish");
    write(repo, "src/later.rs", "// later\n");
    commit(repo, "later work");

    let head_before = git_out(repo, &["rev-parse", "HEAD"]);
    let msg = "collapse foo\n\nshould refuse: stray is interleaved";
    let args = finish_args(repo, "foo", Some(msg));
    let err = block_on(clank::cli::finish::run(args)).expect_err("interleaved foreign refuses");
    let text = err.to_string();
    assert!(text.contains("interleaved"), "got: {text}");
    assert!(
        text.contains(&stray_sha[..7]) && text.contains("stray mid-plan commit"),
        "error must NAME the offender; got: {text}"
    );
    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head_before);
}

#[test]
fn buried_purge_squash_strips_artifacts_from_restacked_commits() {
    // --purge --squash on a buried plan: restacked descendants' trees still
    // CONTAIN the inherited finished/<stem>.md — the restack must strip it.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, false);
    ready_plan_on_base(&env);
    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "original shape".into()];
    block_on(clank::cli::finish::run(args)).expect("plain finish");
    write(repo, "src/later.rs", "// later\n");
    commit(repo, "later work");

    let msg = "collapse and strip foo\n\nthe bookkeeping is noise now";
    let mut args = finish_args(repo, "foo", Some(msg));
    args.purge = true;
    block_on(clank::cli::finish::run(args)).expect("buried purge+squash");

    // HEAD (the restacked later commit) must NOT contain foo's artifacts.
    let tree = git_out(repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(
        tree.lines().all(|l| l != ".clank/finished/foo.md"),
        "restacked tree must be stripped of the finalize snapshot; got:\n{tree}"
    );
    assert!(tree.lines().any(|l| l == "src/later.rs"));
    assert_eq!(git_out(repo, &["log", "-1", "--format=%s"]), "later work");
}
