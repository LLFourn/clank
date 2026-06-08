//! Regression test for codex d772410: repo-scope
//! `clank agent add <label>` with no `--tool` writes a by-name
//! `TeamEntry`, which MUST validate that `<label>` exists in
//! user-scope `agents` before persisting — otherwise a typo
//! corrupts the repo `team` array and every resolver-backed
//! command (`status`, `wfw`, `agent list`) fails closed with
//! `UnknownAgent`.

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

#[test]
fn repo_scope_byname_add_rejects_undeclared_label_and_does_not_persist() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "--quiet", "--initial-branch=main"]);

    // Repo has a team field (so `agent add` gets past the
    // no-team guard). HOME points at a dir with NO user-scope
    // `agents`, so `phantom` is undeclared.
    std::fs::create_dir_all(repo.join(".clank")).unwrap();
    std::fs::write(repo.join(".clank/config.json"), r#"{"team": "dev"}"#).unwrap();
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(clank_bin())
        .args(["agent", "add", "phantom", "--repo"])
        .arg(repo)
        .env("HOME", home.path())
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank agent add");

    assert!(
        !out.status.success(),
        "by-name add of an undeclared label must fail; stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not declared in user-scope"),
        "error should name the undeclared-agent problem; got: {stderr}"
    );

    // The repo config must be untouched — no `phantom` entry
    // persisted.
    let body = std::fs::read_to_string(repo.join(".clank/config.json")).unwrap();
    assert!(
        !body.contains("phantom"),
        "repo team array must NOT contain the rejected label; got: {body}"
    );
}
