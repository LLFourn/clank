//! Integration tests for `clank purge --drop` (Phase 5 of
//! `clank-purge-drop`).
//!
//! The load-bearing test is `drop_drops_mixed_plan_and_code_commits`
//! — it proves `--drop` differs from today's `clank purge`. Today's
//! `clank purge` keeps the code (rewrites the commit to strip
//! `.clank/` paths); `--drop` removes the whole commit including
//! the code.

mod common;

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

fn init_repo_with_master() -> common::TestEnv {
    let env = common::TestEnv::init();
    let path = env.repo();
    write(path, ".clank/.gitignore", "/agents/\n/cache/\n");
    write(path, ".gitignore", ".clank/agents/\n.clank/cache/\n");
    // Register claude as master + codex reviewer via the real
    // cores (`dogfood-init-setup-in-tests`) so the gate has a
    // registered reviewer and doesn't auto-approve.
    env.register_team("claude", &["codex"], &[]);
    write(
        path,
        ".clank/agents/claude/config.json",
        &serde_json::to_string_pretty(&clank_core::agent_config::AgentConfig::default()).unwrap(),
    );
    write(path, "README.md", "seed\n");
    git(path, &["add", "-A"]);
    git(path, &["commit", "--quiet", "-m", "seed"]);
    env
}

fn write(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

fn run_purge(env: &common::TestEnv, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(clank_bin());
    cmd.arg("purge")
        .arg("--repo")
        .arg(env.repo())
        .env("HOME", env.home())
        .arg("--yes")
        .arg("--allow-rewrite-protected") // test repos init on main
        .args(args);
    cmd.output().expect("spawn clank purge")
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

#[test]
fn drop_drops_mixed_plan_and_code_commits() {
    // The load-bearing test: prove `--drop` differs from today's
    // `clank purge`. Setup a plan with a mixed plan+code commit;
    // run `--drop`; assert the CODE is gone (today's purge would
    // have kept it).
    let env = init_repo_with_master();
    let repo = env.repo();
    // Plan intro (plan-body only).
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    // Mixed commit: plan revision + code.
    write(repo, ".clank/plans/foo.md", "# foo v2\n");
    write(repo, "src/foo.rs", "fn main() {}\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] implement"]);

    // Sanity: src/foo.rs exists BEFORE drop.
    assert!(
        repo.join("src/foo.rs").exists(),
        "pre-condition: src/foo.rs should exist before drop"
    );

    let out = run_purge(&env, &["foo", "--drop"]);
    assert!(
        out.status.success(),
        "purge --drop failed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Post-condition: src/foo.rs is GONE from the working tree.
    // (Today's `clank purge` without --drop would have KEPT it.)
    assert!(
        !repo.join("src/foo.rs").exists(),
        "src/foo.rs should be GONE after --drop — that's the entire point. \
         If this assertion fails, --drop is behaving like a plain purge."
    );
    // Plan file also gone.
    assert!(
        !repo.join(".clank/plans/foo.md").exists(),
        "plan file should be gone after --drop"
    );
}

#[test]
fn drop_drops_plan_only_commits() {
    // Plan with only plan-body commits (no code). --drop succeeds
    // and the plan history is gone.
    let env = init_repo_with_master();
    let repo = env.repo();
    write(repo, ".clank/plans/alpha.md", "# v1\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[alpha] intro"]);
    write(repo, ".clank/plans/alpha.md", "# v2\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[alpha] revise"]);

    let out = run_purge(&env, &["alpha", "--drop"]);
    assert!(
        out.status.success(),
        "purge --drop failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!repo.join(".clank/plans/alpha.md").exists());
}

#[test]
fn drop_does_not_write_queue_or_stub() {
    // Critical: --drop must NOT save the plan body anywhere
    // (that's `clank demote`'s job). Asserts no queue or stub
    // files materialize.
    let env = init_repo_with_master();
    let repo = env.repo();
    write(repo, ".clank/plans/beta.md", "# beta\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[beta] intro"]);

    let out = run_purge(&env, &["beta", "--drop"]);
    assert!(out.status.success());

    // No queue file, no stub file.
    let queue_dir = repo.join(".clank/queue");
    let stubs_dir = repo.join(".clank/stubs");
    if queue_dir.exists() {
        let entries: Vec<_> = std::fs::read_dir(&queue_dir)
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert!(
            !entries
                .iter()
                .any(|e| e.file_name().to_string_lossy().contains("beta")),
            "no beta queue file should be written; got: {:?}",
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }
    assert!(
        !stubs_dir.join("beta.md").exists(),
        "no beta stub file should be written"
    );
}

#[test]
fn drop_into_branch_preview_does_not_touch_current_branch() {
    // --drop --into-branch <name> writes the rewritten chain to
    // <name>, leaves the current branch untouched.
    let env = init_repo_with_master();
    let repo = env.repo();
    write(repo, ".clank/plans/gamma.md", "# gamma\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[gamma] intro"]);
    let head_before = head_sha(repo);

    let out = run_purge(&env, &["gamma", "--drop", "--into-branch", "scrubbed"]);
    assert!(
        out.status.success(),
        "purge --drop --into-branch failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    // Current branch untouched.
    assert_eq!(
        head_sha(repo),
        head_before,
        "current branch must be untouched under --into-branch"
    );
    // Plan still in current branch's working tree (we never
    // checked out the new branch).
    assert!(repo.join(".clank/plans/gamma.md").exists());

    // Target branch exists.
    let branches = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["branch", "--list", "scrubbed"])
        .output()
        .unwrap();
    let listed = String::from_utf8_lossy(&branches.stdout);
    assert!(
        listed.contains("scrubbed"),
        "scrubbed branch should be created; got: {listed}"
    );
}

#[test]
fn drop_dry_does_not_touch_refs() {
    let env = init_repo_with_master();
    let repo = env.repo();
    write(repo, ".clank/plans/delta.md", "# delta\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[delta] intro"]);
    let head_before = head_sha(repo);

    let out = run_purge(&env, &["delta", "--drop", "--dry"]);
    assert!(
        out.status.success(),
        "--drop --dry failed: stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(head_sha(repo), head_before, "--dry must not move HEAD");
    assert!(repo.join(".clank/plans/delta.md").exists());
}

#[test]
fn drop_protected_branch_refusal_inherited() {
    let env = init_repo_with_master();
    let repo = env.repo();
    write(repo, ".clank/plans/epsilon.md", "# eps\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[epsilon] intro"]);
    let head_before = head_sha(repo);

    // Use a separate command-builder so we can OMIT
    // --allow-rewrite-protected (run_purge always passes it).
    // HOME must point at env.home() — the team lives there now —
    // so the command reaches the protected-branch refusal rather
    // than failing earlier on a missing user-scope team (codex
    // 9b497a5).
    let out = Command::new(clank_bin())
        .arg("purge")
        .arg("--repo")
        .arg(repo)
        .env("HOME", env.home())
        .arg("--yes")
        .args(["epsilon", "--drop"])
        .output()
        .expect("spawn clank purge");
    assert!(
        !out.status.success(),
        "--drop on protected branch must refuse without --allow-rewrite-protected; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );
    // Assert the refusal is the PROTECTED-BRANCH one, not some
    // earlier unrelated failure — otherwise the test passes for
    // the wrong reason.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("protected") || stderr.contains("--allow-rewrite-protected"),
        "expected protected-branch refusal naming the override; got: {stderr}"
    );
    assert_eq!(head_sha(repo), head_before, "refusal must leave HEAD alone");
}
