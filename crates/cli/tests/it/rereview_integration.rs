//! `clank rereview` — re-open a review round on a plan's latest
//! reviewable commit.
//!
//! Promotion writes a synthetic CONTINUE for the demoted master so the
//! gate does not rewind onto a self-review. That verdict is a roster
//! artefact; this is how the new master converts it into a real one.

use crate::common;

use clank_core::ids::CommitSha;
use common::TestEnv;
use std::path::Path;

fn git(repo: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

fn capture(repo: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn head_sha(repo: &Path) -> CommitSha {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    CommitSha::parse(String::from_utf8(out.stdout).unwrap().trim()).unwrap()
}

fn branch(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// The commit's message as stored, straight from git — never decoded,
/// so a lossy rewrite cannot hide inside the comparison.
fn raw_message(repo: &Path, sha: &CommitSha) -> Vec<u8> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "commit", sha.as_str()])
        .output()
        .unwrap();
    // The message is everything after the blank line ending the header.
    let body = out.stdout;
    let at = body
        .windows(2)
        .position(|w| w == b"\n\n")
        .expect("commit object has a header terminator");
    body[at + 2..].to_vec()
}

fn reviews_for(repo: &Path, sha: &CommitSha) -> Vec<String> {
    use clank_core::wait::PlanStateLookup;
    clank::fs_plan_state_lookup::FsPlanStateLookup::new(repo, Some(sha))
        .reviews_for(sha)
        .into_iter()
        .map(|r| r.author.as_str().to_string())
        .collect()
}

async fn write_feedback(
    repo: &Path,
    author: &str,
    sha: &CommitSha,
    verdict: clank::cli::VerdictArg,
) {
    use clank::cli::{FeedbackArgs, FeedbackCmd, FeedbackWriteArgs};
    clank::cli::feedback::run(FeedbackArgs {
        command: FeedbackCmd::Write(FeedbackWriteArgs {
            repo: Some(repo.to_path_buf()),
            commit: sha.as_str().to_string(),
            verdict,
            author: author.into(),
            message: "looks fine".into(),
        }),
    })
    .await
    .unwrap();
}

/// A plan with one reviewable commit, on the default branch.
fn plan_repo() -> TestEnv {
    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "master", &["rev"], &[]);
    let repo = env.repo();
    std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
    std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    env
}

/// The property a pinned committer date silently breaks, and a
/// wall-clock fix breaks differently: BACK TO BACK, with no sleeping.
/// A sleep here would mean the guarantee still rests on the clock.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rereview_mints_a_new_target_every_time_including_immediately_again() {
    let env = plan_repo();
    let repo = env.repo();
    assert_eq!(branch(repo), "main", "and on the PROTECTED default branch");

    let first = head_sha(repo);
    let (old1, new1) = clank::cli::rereview::rereview_plan(repo, None)
        .await
        .expect("rereview on a protected branch must be allowed");
    assert_eq!(old1, first);
    assert_ne!(new1, old1);
    assert_eq!(head_sha(repo), new1, "the branch moved to it");

    let (old2, new2) = clank::cli::rereview::rereview_plan(repo, None)
        .await
        .expect("and again, immediately");
    assert_eq!(old2, new1, "the second run targets the first's output");
    assert_ne!(
        new2, old2,
        "a pinned committer date makes THIS run a silent no-op"
    );
    assert_ne!(new2, new1);
    assert_ne!(new2, first);
}

/// The mechanism: the target loses its feedback so every reviewer owes
/// a fresh verdict; descendants keep theirs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rereview_drops_the_targets_feedback_and_keeps_its_descendants() {
    let env = plan_repo();
    let repo = env.repo();
    let target = head_sha(repo);
    write_feedback(repo, "rev", &target, clank::cli::VerdictArg::Continue).await;
    assert_eq!(reviews_for(repo, &target), vec!["rev".to_string()]);

    // An unrelated commit stacked on top — the buried-tip case.
    std::fs::write(repo.join("note.txt"), "n\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "chore: note"]);
    let descendant = head_sha(repo);
    write_feedback(repo, "rev", &descendant, clank::cli::VerdictArg::Continue).await;

    let (old, new) = clank::cli::rereview::rereview_plan(repo, None)
        .await
        .unwrap();
    assert_eq!(old, target, "the PLAN's tip, not the branch tip");

    assert!(
        reviews_for(repo, &new).is_empty(),
        "the re-opened commit carries no verdicts — that absence IS the reset"
    );
    assert_eq!(
        reviews_for(repo, &old),
        vec!["rev".to_string()],
        "the old sha keeps its feedback, inert"
    );

    // The descendant was replayed and kept its own genuine review.
    let new_head = head_sha(repo);
    assert_ne!(new_head, descendant, "the descendant was replayed");
    assert_eq!(
        reviews_for(repo, &new_head),
        vec!["rev".to_string()],
        "a descendant's real review follows it across the rewrite"
    );
}

/// Git commit messages are BYTES. A message with a valid tagged
/// subject and an invalid-UTF-8 body must come through a rereview
/// unchanged, byte for byte.
///
/// The first version of this laundered the message through
/// `BStr::to_string()`, which replaces invalid sequences with U+FFFD —
/// so the command that promises an unchanged message quietly rewrote
/// it (codex on acff986). Asserted on raw bytes: any comparison that
/// decodes first cannot see the bug it is checking for.
///
/// The commit object is written DIRECTLY, because `git commit -F`
/// cannot produce this input — it warns and transcodes the byte to
/// valid UTF-8 (0xFF became `c3 bf`), which would have made this test
/// pass against the lossy code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rereview_preserves_a_message_that_is_not_valid_utf8() {
    let env = TestEnv::init();
    common::register_team(env.home(), env.repo(), "master", &["rev"], &[]);
    let repo = env.repo();
    std::fs::create_dir_all(repo.join(".clank/plans")).unwrap();
    std::fs::write(repo.join(".clank/plans/foo.md"), "# foo\n").unwrap();
    git(repo, &["add", "-A"]);

    let tree = capture(repo, &["write-tree"]);
    let mut obj: Vec<u8> = Vec::new();
    obj.extend_from_slice(format!("tree {tree}\n").as_bytes());
    obj.extend_from_slice(b"author t <t@t> 1787651127 +1000\n");
    obj.extend_from_slice(b"committer t <t@t> 1787651127 +1000\n\n");
    obj.extend_from_slice(b"[foo] intro\n\nbody with a raw byte: ");
    obj.push(0xFF);
    obj.push(b'\n');
    let obj_path = repo.join("raw-commit.obj");
    std::fs::write(&obj_path, &obj).unwrap();
    let sha = capture(
        repo,
        &[
            "hash-object",
            "-w",
            "-t",
            "commit",
            obj_path.to_str().unwrap(),
        ],
    );
    std::fs::remove_file(&obj_path).unwrap();
    git(repo, &["update-ref", "refs/heads/main", &sha]);
    git(repo, &["reset", "--hard", "--quiet", "refs/heads/main"]);

    let before = raw_message(repo, &head_sha(repo));
    assert!(
        String::from_utf8(before.clone()).is_err(),
        "precondition: the message really is not valid UTF-8"
    );

    let (_, new) = clank::cli::rereview::rereview_plan(repo, None)
        .await
        .unwrap();
    let after = raw_message(repo, &new);
    assert_eq!(
        after, before,
        "the message must survive as BYTES; a lossy decode turns 0xFF \
         into U+FFFD and leaves a string compare passing"
    );
}

/// An unknown plan is refused by name, rather than silently rewriting
/// whatever happens to be active.
///
/// NOT tested here: the "plan exists but has no reviewable commit"
/// branch. Committing a plan file IS a plan-touching commit, so every
/// plan the fold can resolve already has a reviewable tip — that guard
/// is defensive, and pretending a test covers it would be worse than
/// saying so. (Discovered by writing the test and watching it rewrite
/// a commit I had labelled "not a plan commit": the message is not
/// what makes a commit reviewable, touching the plan file is.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rereview_refuses_a_plan_it_cannot_resolve() {
    let env = plan_repo();
    let err = clank::cli::rereview::rereview_plan(env.repo(), Some("no-such-plan"))
        .await
        .expect_err("an unknown plan must not fall back to the active one");
    let text = format!("{err:#}");
    assert!(
        text.contains("no-such-plan"),
        "the refusal names what was asked for: {text}"
    );
}
