//! Admission must preserve the losing input, including across different verbs.
use crate::common::TestEnv;
use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
fn write(repo: &Path, path: &str, body: &str) {
    let path = repo.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}
fn commit(repo: &Path, subject: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", subject]);
}
fn fixture() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("master", &[], &[]);
    write(
        env.repo(),
        ".clank/.gitignore",
        "/cache/\n/agents/\n/stash/\n/shelved/\n/queue/\n/blocks/\n/config.json\n",
    );
    write(env.repo(), "base.rs", "// base\n");
    commit(env.repo(), "base");
    env
}
fn push(repo: &Path, name: &str) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(clank::cli::stash::run_push(clank::cli::StashPushArgs {
            plan: Some(name.into()),
            repo: Some(repo.into()),
            waiting_for: None,
            to_queue: false,
            priority: None,
            dry: false,
        }))
}
fn pop(repo: &Path, name: &str) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(clank::cli::stash::run_unshelve_alias(
            clank::cli::UnshelveArgs {
                plan: Some(name.into()),
                repo: Some(repo.into()),
            },
        ))
}
fn stashed_foo() -> TestEnv {
    let env = fixture();
    write(env.repo(), ".clank/plans/foo.md", "# foo\n");
    commit(env.repo(), "[foo] intro");
    write(env.repo(), "foo.rs", "// foo implementation\n");
    commit(env.repo(), "[foo] implementation");
    push(env.repo(), "foo").unwrap();
    env
}
fn active(repo: &Path) -> String {
    clank::rebuild::rebuild_repo_sync_with_policy(repo, clank::rebuild::CachePolicy::Use)
        .unwrap()
        .fold
        .active_plan
        .unwrap()
        .key
        .as_str()
        .to_owned()
}

#[test]
fn occupied_slot_refuses_both_promotion_surfaces_without_consuming_queue_or_index() {
    let env = fixture();
    let repo = env.repo();
    write(repo, ".clank/plans/live.md", "# live\n");
    commit(repo, "[live] intro");
    // A blocked plan still owns the slot. Admission does not exempt it.
    write(
        repo,
        ".clank/agents/master/blocks/hold.md",
        "waiting for a human\n",
    );
    write(
        repo,
        ".clank/queue/100-next.md",
        "# next\n\nkeep this body\n",
    );
    write(repo, "user.rs", "// staged user work\n");
    git(repo, &["add", "user.rs"]);
    let status = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(clank::cli::status::snapshot(repo, Some(env.home())))
        .unwrap()
        .to_json();
    assert_eq!(status["active_plan"]["gate_state"], "blocked");
    let entry = clank::cli::queue::entry_by_stem(repo, "100-next").unwrap();
    let before = (
        git(repo, &["rev-parse", "HEAD"]),
        git(repo, &["write-tree"]),
    );
    for result in [
        clank::cli::queue::promote(repo, "next"),
        clank::cli::queue::promote_entry(repo, &entry),
    ] {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("`live` is already active"), "{error}");
    }
    assert_eq!(
        std::fs::read_to_string(&entry.path).unwrap(),
        "# next\n\nkeep this body\n"
    );
    assert!(!repo.join(".clank/plans/next.md").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join(".clank/plans/live.md")).unwrap(),
        "# live\n"
    );
    assert_eq!(
        (
            git(repo, &["rev-parse", "HEAD"]),
            git(repo, &["write-tree"])
        ),
        before
    );
    assert!(
        !repo.join(".clank/stash").exists(),
        "promotion creates no stash/runtime file in the repo"
    );
}

#[test]
fn concurrent_promotions_through_repo_aliases_accept_one_and_keep_the_other_queued() {
    let env = fixture();
    let repo = env.repo();
    write(repo, ".clank/queue/100-first.md", "# first\n");
    write(repo, ".clank/queue/200-second.md", "# second\n");
    let entries = [
        clank::cli::queue::entry_by_stem(repo, "100-first").unwrap(),
        clank::cli::queue::entry_by_stem(repo, "200-second").unwrap(),
    ];
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let alias = env.home().join("repo-alias");
    std::os::unix::fs::symlink(repo, &alias).unwrap();
    let jobs: Vec<_> = entries
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, entry)| {
            let repo = if i == 0 {
                repo.to_owned()
            } else {
                alias.clone()
            };
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                clank::cli::queue::promote_entry(&repo, &entry)
            })
        })
        .collect();
    let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    let winner = results.iter().position(|r| r.is_ok()).unwrap();
    let loser = 1 - winner;
    assert!(
        results[loser]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("already active")
    );
    assert_eq!(active(repo), entries[winner].name.as_str());
    assert!(!entries[winner].path.exists());
    assert_eq!(
        std::fs::read_to_string(&entries[loser].path).unwrap(),
        format!("# {}\n", entries[loser].name.as_str())
    );
    assert_eq!(git(repo, &["rev-list", "--count", "HEAD"]), "2");
    assert_eq!(
        std::fs::read_dir(repo.join(".clank/plans"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn unfinish_reopens_the_empty_slot_and_refuses_to_replace_an_occupied_one() {
    let env = fixture();
    let repo = env.repo();
    write(repo, ".clank/plans/first.md", "# first\n");
    commit(repo, "[first] intro");
    std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
    std::fs::rename(
        repo.join(".clank/plans/first.md"),
        repo.join(".clank/finished/first.md"),
    )
    .unwrap();
    commit(repo, "[first] finish");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let args = || clank::cli::UnfinishArgs {
        plan: Some("first".into()),
        repo: Some(repo.into()),
    };
    runtime.block_on(clank::cli::unfinish::run(args())).unwrap();
    assert_eq!(active(repo), "first");
    assert!(!repo.join(".clank/finished/first.md").exists());
    let head = git(repo, &["rev-parse", "HEAD"]);
    let error = runtime
        .block_on(clank::cli::unfinish::run(args()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("`first` is already active"), "{error}");
    assert_eq!(git(repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(active(repo), "first");
}

#[test]
fn different_active_plan_refuses_restore_and_pick_without_touching_stashed_work() {
    let env = stashed_foo();
    let repo = env.repo();
    write(repo, ".clank/plans/live.md", "# live\n");
    commit(repo, "[live] intro");
    let record = std::fs::read(repo.join(".clank/stash/foo.json")).unwrap();
    let head = git(repo, &["rev-parse", "HEAD"]);
    let protected = git(repo, &["rev-parse", "refs/clank/stash/foo"]);
    let error = pop(repo, "foo").unwrap_err().to_string();
    assert!(error.contains("`live` is already active"), "{error}");
    let pick = clank::cli::PickArgs {
        plans: vec!["foo".into()],
        from: "refs/clank/stash/foo".into(),
        repo: Some(repo.into()),
        dry: false,
        squash: false,
        purge: false,
    };
    let error = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(clank::cli::pick::run(pick))
        .unwrap_err()
        .to_string();
    assert!(error.contains("`live` is already active"), "{error}");
    assert_eq!(git(repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(repo, &["rev-parse", "refs/clank/stash/foo"]), protected);
    assert_eq!(
        std::fs::read(repo.join(".clank/stash/foo.json")).unwrap(),
        record
    );
    assert!(!repo.join(".clank/plans/foo.md").exists());
    assert!(!repo.join("foo.rs").exists());
}

#[test]
fn promotion_and_restore_share_admission_and_preserve_the_losing_input() {
    let env = stashed_foo();
    let repo = env.repo();
    write(repo, ".clank/queue/100-next.md", "# next\n");
    let entry = clank::cli::queue::entry_by_stem(repo, "100-next").unwrap();
    let record = std::fs::read(repo.join(".clank/stash/foo.json")).unwrap();
    let protected = git(repo, &["rev-parse", "refs/clank/stash/foo"]);
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));
    let (path, gate, queued) = (repo.to_owned(), start.clone(), entry.clone());
    let promoting = std::thread::spawn(move || {
        gate.wait();
        clank::cli::queue::promote_entry(&path, &queued)
    });
    let (path, gate) = (repo.to_owned(), start.clone());
    let restoring = std::thread::spawn(move || {
        gate.wait();
        pop(&path, "foo")
    });
    let promoted = promoting.join().unwrap();
    let restored = restoring.join().unwrap();
    assert_ne!(
        promoted.is_ok(),
        restored.is_ok(),
        "promote={promoted:?}, restore={restored:?}"
    );
    if promoted.is_ok() {
        assert_eq!(active(repo), "next");
        assert!(!entry.path.exists());
        assert_eq!(
            std::fs::read(repo.join(".clank/stash/foo.json")).unwrap(),
            record
        );
        assert_eq!(git(repo, &["rev-parse", "refs/clank/stash/foo"]), protected);
        assert!(restored.unwrap_err().to_string().contains("already active"));
    } else {
        assert_eq!(active(repo), "foo");
        assert_eq!(std::fs::read_to_string(&entry.path).unwrap(), "# next\n");
        assert!(!repo.join(".clank/stash/foo.json").exists());
        assert_eq!(
            std::fs::read_to_string(repo.join("foo.rs")).unwrap(),
            "// foo implementation\n"
        );
        assert!(promoted.unwrap_err().to_string().contains("already active"));
    }
    let state =
        clank::rebuild::rebuild_repo_sync_with_policy(repo, clank::rebuild::CachePolicy::Use)
            .unwrap();
    assert!(
        state.fold.warnings.is_empty(),
        "the rejected attempt never entered Git history"
    );
}

#[test]
fn code_only_pick_keeps_the_occupied_slot_and_source_stash() {
    let env = stashed_foo();
    let repo = env.repo();
    write(repo, ".clank/plans/live.md", "# live\n");
    commit(repo, "[live] intro");
    let record = std::fs::read(repo.join(".clank/stash/foo.json")).unwrap();
    let protected = git(repo, &["rev-parse", "refs/clank/stash/foo"]);
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(clank::cli::pick::run(clank::cli::PickArgs {
            plans: vec!["foo".into()],
            from: "refs/clank/stash/foo".into(),
            repo: Some(repo.into()),
            dry: false,
            squash: false,
            purge: true,
        }))
        .unwrap();
    assert_eq!(active(repo), "live");
    assert!(!repo.join(".clank/plans/foo.md").exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("foo.rs")).unwrap(),
        "// foo implementation\n"
    );
    assert_eq!(
        std::fs::read(repo.join(".clank/stash/foo.json")).unwrap(),
        record
    );
    assert_eq!(git(repo, &["rev-parse", "refs/clank/stash/foo"]), protected);
}
