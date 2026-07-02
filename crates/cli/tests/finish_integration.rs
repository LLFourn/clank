//! In-process tests for `clank finish`. Drives `finish::run` directly — no
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

fn finish_args(
    repo: &Path,
    plan: &str,
    squash: Option<&str>,
    amend: bool,
) -> clank::cli::FinishArgs {
    clank::cli::FinishArgs {
        plan: Some(plan.into()),
        repo: Some(repo.to_path_buf()),
        amend,
        message: vec![],
        purge: false,
        squash: squash.map(str::to_string),
        into_branch: None,
        allow_rewrite_protected: false,
        dry: false,
        no_cache: true,
    }
}

#[test]
fn refused_squash_leaves_the_validated_message_not_the_placeholder() {
    // codex 053f9d1: `finish --squash` stamps the finalize/amend commit with
    // the (validated) squash message BEFORE the squash runs, so a squash that
    // is REFUSED (here: protected `main`) leaves that message on HEAD — never
    // the `[stem] finish` placeholder.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();

    // A plan, then simulate finish by moving it into finished/ with a finalize
    // commit at HEAD.
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
    std::fs::rename(
        repo.join(".clank/plans/foo.md"),
        repo.join(".clank/finished/foo.md"),
    )
    .unwrap();
    commit(repo, "[foo] finish");

    let msg = "collapse foo into one\n\nso the branch reads as a single commit";
    // `--amend --squash` on the already-finished plan: amend stamps the squash
    // message, then the squash is refused on protected `main`.
    let err = block_on(clank::cli::finish::run(finish_args(
        repo,
        "foo",
        Some(msg),
        true,
    )))
    .expect_err("squash must be refused on protected main");
    assert!(
        err.to_string().contains("protected"),
        "expected protected-branch refusal, got: {err}"
    );

    // HEAD carries the validated squash subject (plan-tagged), NOT the placeholder.
    let subject = git_out(repo, &["log", "-1", "--format=%s"]);
    assert_eq!(
        subject, "[foo] collapse foo into one",
        "refused squash must leave the landing message"
    );
    assert_ne!(
        subject, "[foo] finish",
        "placeholder must not remain on HEAD"
    );
}

#[test]
fn refused_purge_leaves_the_validated_message_not_the_placeholder() {
    // codex 53a9edb: `--purge` on a READY plan CREATES the finalize commit,
    // then runs the strip rewrite — which can be refused (protected `main`),
    // leaving the finalize commit on HEAD. It must carry the validated `-m`,
    // never `[stem] finish`.
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
    let mut args = finish_args(repo, "foo", None, false);
    args.purge = true;
    args.message = vec![msg.to_string()];
    let err = block_on(clank::cli::finish::run(args))
        .expect_err("purge rewrite must be refused on protected main");
    assert!(
        err.to_string().contains("protected"),
        "expected protected-branch refusal, got: {err}"
    );

    let subject = git_out(repo, &["log", "-1", "--format=%s"]);
    assert_eq!(
        subject, "[foo] strip foo artifacts",
        "refused purge must leave the validated message"
    );
    assert_ne!(
        subject, "[foo] finish",
        "placeholder must not remain on HEAD"
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

    let mut args = finish_args(repo, "foo", None, false);
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

    let mut args = finish_args(repo, "foo", None, false);
    args.squash = Some("collapse foo\n\na single commit reads cleaner".into());
    args.allow_rewrite_protected = true; // let the squash run on `main`
    block_on(clank::cli::finish::run(args)).expect("squash finish should succeed");

    let subject = git_out(repo, &["log", "-1", "--format=%s"]);
    assert_eq!(
        subject, "[foo] collapse foo",
        "squash subject must carry the plan tag"
    );
}
