//! In-process test: the status snapshot carries recent oneline log
//! lines for the TUI's live-log pane (status-tui-live-log).

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

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, body).unwrap();
}

fn commit(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", msg]);
}

#[test]
fn snapshot_carries_recent_oneline_log_with_reviews() {
    let env = TestEnv::init();
    env.register_team("claude", &["codex"], &[]);
    let repo = env.repo();
    write(repo, ".clank/.gitignore", "/cache/\n/agents/\n/shelved/\n");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let intro = {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    write(
        repo,
        &format!(".clank/agents/codex/feedback/{intro}.md"),
        "CONTINUE lgtm\n",
    );
    write(repo, "src/a.rs", "// a\n");
    commit(repo, "[foo] impl a");

    let snap = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(clank::cli::status::snapshot(repo, Some(env.home())))
        .unwrap();
    let lines = snap.log_lines_for_test();
    let joined = lines.join("\n");
    // Umbrella shape (log-plan-umbrellas): plan header at col 0,
    // commits beneath with the [foo] prefix STRIPPED.
    assert!(
        lines.iter().any(|l| l == "foo"),
        "umbrella header:\n{joined}"
    );
    assert!(!joined.contains("[foo]"), "prefix stripped:\n{joined}");
    assert!(joined.contains(" intro"), "got:\n{joined}");
    assert!(joined.contains(" impl a"), "got:\n{joined}");
    assert!(
        joined.contains("✓ codex: lgtm"),
        "review sub-line:\n{joined}"
    );
    // Chronological: intro before impl.
    assert!(
        joined.find(" impl a").unwrap() < joined.find(" intro").unwrap(),
        "newest first:\n{joined}"
    );
}
