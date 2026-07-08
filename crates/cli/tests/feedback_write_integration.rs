//! In-process test: `feedback write` normalizes a leading verdict
//! restatement out of the message (strip-verdict-restatement) — the
//! declared --verdict is the single source of truth, so a message
//! like "CONTINUE: looks good" lands on disk as `CONTINUE looks good`.

mod common;

use common::TestEnv;
use std::path::Path;
use std::process::Command;

use clank::cli::{FeedbackArgs, FeedbackCmd, FeedbackWriteArgs, VerdictArg};
use clank_core::feedback_body::{parse_summary, parse_verdict};
use clank_core::vocab::Verdict;

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

fn head_sha(repo: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn write_strips_a_restated_verdict_from_the_message() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let sha = head_sha(repo);

    clank::cli::feedback::run(FeedbackArgs {
        command: FeedbackCmd::Write(FeedbackWriteArgs {
            repo: Some(repo.to_path_buf()),
            commit: sha.clone(),
            verdict: VerdictArg::Continue,
            author: "codex".into(),
            message: "CONTINUE: looks good\n\nDetails survive.".into(),
        }),
    })
    .await
    .unwrap();

    // The wire path may use the short or full sha form — read the one
    // file the write produced.
    let dir = repo.join(".clank/agents/codex/feedback");
    let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert_eq!(entries.len(), 1, "exactly one feedback file written");
    let path = entries[0].as_ref().unwrap().path();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(
        body.starts_with("CONTINUE looks good\n"),
        "restatement stripped from the composed first line: {body:?}"
    );
    assert_eq!(parse_verdict(&body), Verdict::Continue);
    assert_eq!(parse_summary(&body), "looks good");
    assert!(body.contains("\n\nDetails survive."), "{body:?}");
}
