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

fn finish_args(repo: &Path, plan: &str, squash: Option<&str>) -> clank::cli::FinishArgs {
    clank::cli::FinishArgs {
        plan: Some(plan.into()),
        repo: Some(repo.to_path_buf()),
        message: vec![],
        purge: false,
        squash: squash.map(str::to_string),
        no_squash: false,
        into_branch: None,
        allow_rewrite_protected: false,
        dry: false,
        no_cache: true,
    }
}

#[test]
fn refused_squash_leaves_the_validated_message_not_the_placeholder() {
    // codex 053f9d1: `finish --squash` stamps the transient finalize commit
    // with the (validated) squash message BEFORE the squash runs, so a squash
    // that is REFUSED (here: protected `main`) leaves that message on HEAD —
    // never the `[stem] finish` placeholder.
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
    // refused on protected `main` (no --allow-rewrite-protected).
    let err = block_on(clank::cli::finish::run(finish_args(repo, "foo", Some(msg))))
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
    let mut args = finish_args(repo, "foo", None);
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
    args.allow_rewrite_protected = true; // let the squash run on `main`
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
    args.allow_rewrite_protected = true; // main is protected; let the squash run
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
fn autosquash_squashes_on_protected_main_without_the_flag() {
    // Part 2 (option A): autosquash implies allow_rewrite_protected, so it
    // collapses the plan on protected `main` WITHOUT `--allow-rewrite-protected`
    // — the natural clank workflow runs on the protected branch.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    set_autosquash(repo, true);
    ready_plan_on_base(&env);

    let mut args = finish_args(repo, "foo", None);
    args.message = vec!["wrap up foo".into(), "one commit on main".into()];
    // allow_rewrite_protected stays FALSE — autosquash must supply it.
    assert!(!args.allow_rewrite_protected);
    block_on(clank::cli::finish::run(args)).expect("autosquash squashes on protected main");

    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%s"]),
        "[foo] wrap up foo"
    );
    assert_eq!(
        git_out(repo, &["rev-list", "--count", "HEAD"]),
        "2",
        "collapsed to base + one commit on protected main"
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
    // Blocker parity: an explicit --squash on protected `main` without
    // --allow-rewrite-protected. The live run refuses; the --dry — the same
    // computation — reports the blocker and changes nothing.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    ready_plan_on_base(&env);

    let head_before = git_out(repo, &["rev-parse", "HEAD"]);
    let msg = "collapse foo\n\nso the branch reads as one commit";
    let mut args = finish_args(repo, "foo", Some(msg));
    args.dry = true;
    block_on(clank::cli::finish::run(args)).expect("dry prints blockers, exits clean");
    assert_eq!(
        git_out(repo, &["rev-parse", "HEAD"]),
        head_before,
        "blocked dry changes nothing"
    );
    assert!(repo.join(".clank/plans/foo.md").exists());

    let args = finish_args(repo, "foo", Some(msg));
    let err = block_on(clank::cli::finish::run(args)).expect_err("live refuses on protected main");
    assert!(err.to_string().contains("protected"), "got: {err}");
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
