//! In-process tests: `clank feedback read --plan` reads a whole
//! review cycle (the-nudge-names-the-command). The per-commit
//! reading it already had is unchanged, `-j` included.

use crate::common;

use common::TestEnv;
use std::path::Path;
use std::process::Command;

use clank::cli::{FeedbackArgs, FeedbackCmd, FeedbackReadArgs, FeedbackWriteArgs, VerdictArg};

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

fn commit(repo: &Path, subject: &str) -> String {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", subject]);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

async fn review(repo: &Path, sha: &str, author: &str, verdict: VerdictArg, message: &str) {
    clank::cli::feedback::run(FeedbackArgs {
        command: FeedbackCmd::Write(FeedbackWriteArgs {
            repo: Some(repo.to_path_buf()),
            commit: sha.to_string(),
            verdict,
            author: author.to_string(),
            message: message.to_string(),
        }),
    })
    .await
    .expect("feedback write");
}

fn read(repo: &Path, plan: Option<&str>, commit: Option<&str>, json: bool) -> FeedbackReadArgs {
    FeedbackReadArgs {
        commit: commit.map(str::to_string),
        plan: plan.map(str::to_string),
        repo: Some(repo.to_path_buf()),
        json,
    }
}

/// A plan with an intro and one milestone, each reviewed.
async fn two_reviewed_commits(env: &TestEnv) -> (String, String) {
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n\nThe body.\n");
    let intro = commit(repo, "[foo] intro");
    write(repo, "src/a.txt", "one\n");
    let m1 = commit(repo, "[foo] the first milestone");
    review(
        repo,
        &intro,
        "codex",
        VerdictArg::Continue,
        "scope is right\n\nBody of the intro review.",
    )
    .await;
    review(
        repo,
        &m1,
        "codex",
        VerdictArg::RequestChanges,
        "the thing is unfixed",
    )
    .await;
    (intro, m1)
}

#[tokio::test]
async fn a_plan_reads_as_a_cycle_oldest_first() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let (intro, m1) = two_reviewed_commits(&env).await;

    let out = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, false))
        .await
        .unwrap();

    let at_intro = out.find(&intro[..7]).expect("intro commit in the reading");
    let at_m1 = out.find(&m1[..7]).expect("milestone commit in the reading");
    assert!(
        at_intro < at_m1,
        "oldest first — intro before the milestone:\n{out}"
    );
    assert!(out.starts_with("plan foo\n"), "{out}");
    assert!(out.contains("codex  CONTINUE: scope is right"), "{out}");
    assert!(
        out.contains("codex  REQUEST_CHANGES: the thing is unfixed"),
        "{out}"
    );
    assert!(out.contains("Body of the intro review."), "{out}");
}

#[tokio::test]
async fn bare_plan_reads_the_active_plan() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    two_reviewed_commits(&env).await;

    let named = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, false))
        .await
        .unwrap();
    let inferred = clank::cli::feedback::read_rendered(read(repo, Some(""), None, false))
        .await
        .unwrap();
    assert_eq!(inferred, named, "bare --plan is the one active plan");
}

#[tokio::test]
async fn an_unknown_plan_names_the_plans_there_are() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    two_reviewed_commits(&env).await;

    let err = clank::cli::feedback::read_rendered(read(repo, Some("nope"), None, false))
        .await
        .expect_err("unknown plan is an error");
    let msg = err.to_string();
    assert!(msg.contains("nope"), "{msg}");
    assert!(msg.contains("foo.md"), "names the plans there are: {msg}");
}

#[tokio::test]
async fn a_plan_with_no_feedback_says_so() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let out = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, false))
        .await
        .unwrap();
    assert_eq!(
        out,
        "no feedback for plan foo (1 commit, none reviewed yet)\n"
    );
}

#[tokio::test]
async fn an_unreviewed_commit_is_shown_as_unreviewed() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let (intro, _) = two_reviewed_commits(&env).await;
    write(repo, "src/b.txt", "two\n");
    let m2 = commit(repo, "[foo] the second milestone");

    let out = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, false))
        .await
        .unwrap();
    assert!(
        out.contains(&format!("commit {}\n  no feedback\n", &m2[..7])),
        "the commit awaiting review is in the cycle, marked:\n{out}"
    );
    assert!(out.contains(&intro[..7]), "{out}");
}

#[tokio::test]
async fn plan_json_is_one_value_carrying_each_commit() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let (intro, m1) = two_reviewed_commits(&env).await;

    let raw = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, true))
        .await
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).expect("one JSON value");
    assert_eq!(doc["plan"], "foo");
    let commits = doc["commits"].as_array().expect("commits array");
    assert_eq!(commits.len(), 2);
    assert_eq!(commits[0]["commit"], intro);
    assert_eq!(commits[1]["commit"], m1);

    // Each group's rows ARE the per-commit reading, unchanged.
    for sha in [&intro, &m1] {
        let per_commit = clank::cli::feedback::read_rendered(read(repo, None, Some(sha), true))
            .await
            .unwrap();
        let rows: serde_json::Value = serde_json::from_str(&per_commit).unwrap();
        let group = commits
            .iter()
            .find(|c| c["commit"] == sha.as_str())
            .unwrap();
        assert_eq!(group["feedback"], rows, "group rows == `--commit -j` rows");
    }
}

#[tokio::test]
async fn the_per_commit_reading_is_unchanged() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let (_, m1) = two_reviewed_commits(&env).await;

    let text = clank::cli::feedback::read_rendered(read(repo, None, Some(&m1), false))
        .await
        .unwrap();
    assert_eq!(
        text,
        format!(
            "commit {}\n  codex  REQUEST_CHANGES: the thing is unfixed\n    .clank/agents/codex/feedback/{}.md\n",
            &m1[..7],
            &m1[..7],
        ),
        "the commit reading is its own block, with no plan header",
    );

    let rows: serde_json::Value = serde_json::from_str(
        &clank::cli::feedback::read_rendered(read(repo, None, Some(&m1), true))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(rows.as_array().expect("a bare array").len(), 1);
    assert_eq!(rows[0]["verdict"], "request_changes");
}

#[tokio::test]
async fn a_commit_with_no_feedback_still_says_so_in_prose() {
    // Predates --plan: `-j` on an unreviewed commit prints this line
    // rather than an empty array, and agents read around it.
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    let intro = commit(repo, "[foo] intro");

    for json in [false, true] {
        let out = clank::cli::feedback::read_rendered(read(repo, None, Some(&intro), json))
            .await
            .unwrap();
        assert_eq!(out, format!("no feedback for {}\n", &intro[..7]));
    }
}

fn head(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// `finish` with a squash collapses the plan into one commit nobody
/// reviewed, and the cycle's verdicts stay keyed by shas that are no
/// longer in history. The reading must not call that "none reviewed
/// yet" — every plan in a repo ends up in this state (ruthless
/// 863f359).
// `finish` reaches for `block_in_place` (rewire), which the
// single-threaded test runtime refuses.
#[tokio::test(flavor = "multi_thread")]
async fn a_finished_plan_does_not_claim_it_was_never_reviewed() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    let (intro, m1) = two_reviewed_commits(&env).await;
    review(repo, &m1, "codex", VerdictArg::Finished, "ship it").await;

    clank::cli::finish::run(clank::cli::FinishArgs {
        plan: Some("foo".into()),
        repo: Some(repo.to_path_buf()),
        message: vec![],
        purge: false,
        squash: Some("collapse foo\n\nit exists because the thing needed doing".into()),
        no_squash: false,
        into_branch: None,
        dry: false,
        force: false,
        no_cache: true,
    })
    .await
    .expect("squash finish");

    let finalized = head(repo);
    let out = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, false))
        .await
        .unwrap();
    assert!(
        !out.contains("none reviewed yet"),
        "the plan went through the gate — this is the false reading:\n{out}"
    );
    assert!(
        out.contains(&format!("was finished at {}", &finalized[..7])),
        "names the commit finish left behind:\n{out}"
    );
    assert!(
        out.contains("clank feedback read --commit"),
        "names what does still read a sha:\n{out}"
    );
    // The cycle's verdicts are still on disk, under shas the squash
    // removed from history — which is why reading by plan cannot
    // reach them.
    assert!(
        repo.join(format!(".clank/agents/codex/feedback/{}.md", &intro[..7]))
            .exists(),
        "the intro's verdict is still keyed by its own sha",
    );

    let doc: serde_json::Value = serde_json::from_str(
        &clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, true))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        doc["finalized_at"], finalized,
        "the JSON carries the same fact the prose does: {doc}"
    );
}

fn finish_args(repo: &Path, squash: Option<&str>) -> clank::cli::FinishArgs {
    clank::cli::FinishArgs {
        plan: Some("foo".into()),
        repo: Some(repo.to_path_buf()),
        message: vec![
            "collapse foo".into(),
            "it exists because the thing needed doing".into(),
        ],
        purge: false,
        squash: squash.map(str::to_string),
        no_squash: squash.is_none(),
        into_branch: None,
        dry: false,
        force: true,
        no_cache: true,
    }
}

/// A plan introduced again after a finish is in BOTH the active and
/// the finished set, and its commits are the new cycle's. Pairing the
/// old cycle's finalize with them says the live plan is over (codex
/// a7f47e4).
#[tokio::test(flavor = "multi_thread")]
async fn a_reintroduced_plan_is_not_finished() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    clank::cli::finish::run(finish_args(
        repo,
        Some("collapse foo\n\nbecause it was time"),
    ))
    .await
    .expect("first finish");

    write(repo, ".clank/plans/foo.md", "# foo, again\n");
    let second_intro = commit(repo, "[foo] intro");

    let out = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, false))
        .await
        .unwrap();
    assert!(
        !out.contains("was finished"),
        "the live cycle is not over:\n{out}"
    );
    assert_eq!(
        out,
        "no feedback for plan foo (1 commit, none reviewed yet)\n"
    );

    let doc: serde_json::Value = serde_json::from_str(
        &clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, true))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(doc["finalized_at"], serde_json::Value::Null, "{doc}");
    assert_eq!(doc["commits"][0]["commit"], second_intro, "{doc}");
}

/// A finish that did NOT collapse the plan leaves its commits in
/// history. Empty feedback there is an absence, not evidence of a
/// squash (codex a7f47e4).
#[tokio::test(flavor = "multi_thread")]
async fn an_unsquashed_finish_with_no_feedback_blames_no_squash() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    let intro = commit(repo, "[foo] intro");
    write(repo, "src/a.txt", "one\n");
    commit(repo, "[foo] the first milestone");
    clank::cli::finish::run(finish_args(repo, None))
        .await
        .expect("no-squash finish");

    let out = clank::cli::feedback::read_rendered(read(repo, Some("foo"), None, false))
        .await
        .unwrap();
    assert!(
        !out.contains("collapsed"),
        "nothing was collapsed — its commits are still in history:\n{out}"
    );
    assert!(
        out.starts_with("no feedback for plan foo (finished at "),
        "{out}"
    );
    assert!(out.contains("none of them reviewed"), "{out}");
    assert_eq!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "-t", &intro])
            .output()
            .unwrap()
            .stdout,
        b"commit\n",
        "the intro is still in history",
    );
}
