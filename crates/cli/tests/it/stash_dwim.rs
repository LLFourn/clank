//! `clank stash` does what it is asked: it sets the plan aside under
//! foreign commits, uncommitted work, untracked files, a detached HEAD,
//! a merge or a root intro, and refuses only a genuine conflict. Every
//! refusal moves nothing. Drives the in-process cores on real repos.

use crate::common;

use clank::cli::stash::Stage;
use common::TestEnv;
use std::path::Path;
use std::process::Command;

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
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

fn read(repo: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(repo.join(rel)).ok()
}

fn commit(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", msg]);
}

fn env() -> TestEnv {
    let env = TestEnv::init();
    env.register_team("claude", &[], &[]);
    write(
        env.repo(),
        ".clank/.gitignore",
        "/cache/\n/agents/\n/stash/\n/shelved/\n",
    );
    env
}

/// `[misc] base` (src/base.rs, src/old.rs), then plan `foo`: its intro,
/// `impl a` adding src/a.rs, and `impl b` removing src/old.rs.
fn with_foo() -> TestEnv {
    let env = env();
    let repo = env.repo();
    write(repo, "src/base.rs", "// base\n");
    write(repo, "src/old.rs", "// old\n");
    commit(repo, "[misc] base");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/a.rs", "// a\n");
    commit(repo, "[foo] impl a");
    git(repo, &["rm", "--quiet", "src/old.rs"]);
    commit(repo, "[foo] impl b");
    env
}

fn args(repo: &Path, dry: bool) -> clank::cli::StashPushArgs {
    clank::cli::StashPushArgs {
        plan: Some("foo".into()),
        repo: Some(repo.to_path_buf()),
        waiting_for: None,
        to_queue: false,
        priority: None,
        dry,
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Runtime::new().unwrap().block_on(f)
}

fn push(repo: &Path) -> anyhow::Result<()> {
    push_hooked(repo, |_, _| {})
}

fn push_hooked(repo: &Path, mut hook: impl FnMut(Stage, &Path) + Send) -> anyhow::Result<()> {
    block_on(clank::cli::stash::push_with(args(repo, false), &mut hook))
}

fn pop(repo: &Path) -> anyhow::Result<()> {
    pop_hooked(repo, |_, _| {})
}

fn pop_hooked(repo: &Path, mut hook: impl FnMut(Stage, &Path) + Send) -> anyhow::Result<()> {
    block_on(clank::cli::stash::pop_with(
        Some(repo),
        Some("foo"),
        None,
        &mut hook,
    ))
}

/// Everything a refusal must leave as it was.
#[derive(Debug, PartialEq, Eq)]
struct Seen {
    head: String,
    binding: String,
    refs: String,
    index: String,
    staged: String,
    unstaged: String,
    status: String,
    stash_record: bool,
}

fn seen(repo: &Path) -> Seen {
    Seen {
        head: git_out(repo, &["rev-parse", "HEAD"]),
        binding: git_out(repo, &["symbolic-ref", "-q", "HEAD"]),
        refs: git_out(repo, &["show-ref"]),
        index: git_out(repo, &["ls-files", "--stage"]),
        staged: git_out(repo, &["diff", "--cached"]),
        unstaged: git_out(repo, &["diff"]),
        // Clank's own ignored scratch (its fold cache, the stash lock)
        // is not the operator's.
        status: git_out(repo, &["status", "--porcelain=v1", "-uall", "--ignored"])
            .lines()
            .filter(|l| !l.starts_with("!! .clank/"))
            .collect::<Vec<_>>()
            .join("\n"),
        stash_record: repo.join(".clank/stash/foo.json").exists(),
    }
}

fn subjects(repo: &Path, rev: &str) -> Vec<String> {
    git_out(repo, &["log", "--format=%s", rev])
        .lines()
        .map(str::to_string)
        .collect()
}

/// Staged and unstaged work of every kind, for the automatic commit.
fn dirty(repo: &Path) {
    write(repo, "src/new.rs", "// staged addition\n");
    git(repo, &["add", "src/new.rs"]);
    git(repo, &["rm", "--quiet", "src/base.rs"]);
    write(repo, "src/a.rs", "// staged a\n");
    git(repo, &["add", "src/a.rs"]);
    write(repo, "src/a.rs", "// staged a, then unstaged a\n");
    write(repo, ".clank/plans/foo.md", "# foo, edited\n");
}

#[test]
fn an_untagged_commit_after_the_intro_is_kept_and_the_plan_set_aside() {
    let env = with_foo();
    let repo = env.repo();
    write(repo, "experiments/k1.py", "print('k1')\n");
    commit(repo, "ignore experiments/, add k1_generator.py");
    write(repo, "src/a.rs", "// a, again\n");
    commit(repo, "[foo] impl c");
    let author = git_out(repo, &["log", "-1", "--format=%an <%ae> %ad", "HEAD~1"]);

    push(repo).unwrap();
    let files = git_out(repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!files.contains(".clank/plans/foo.md"), "{files}");
    assert!(!files.contains("src/a.rs"), "{files}");
    assert!(
        files.contains("src/old.rs"),
        "the plan's removal is undone: {files}"
    );
    assert_eq!(
        read(repo, "experiments/k1.py").as_deref(),
        Some("print('k1')\n")
    );
    assert!(read(repo, "src/a.rs").is_none());
    assert_eq!(
        subjects(repo, "HEAD"),
        ["ignore experiments/, add k1_generator.py", "[misc] base"]
    );
    assert_eq!(
        git_out(repo, &["log", "-1", "--format=%an <%ae> %ad", "HEAD"]),
        author
    );

    pop(repo).unwrap();
    assert_eq!(read(repo, "src/a.rs").as_deref(), Some("// a, again\n"));
    assert!(read(repo, "src/old.rs").is_none());
    assert_eq!(
        read(repo, ".clank/plans/foo.md").as_deref(),
        Some("# foo\n")
    );
    let after = subjects(repo, "HEAD");
    assert_eq!(
        after
            .iter()
            .filter(|s| s.as_str() == "ignore experiments/, add k1_generator.py")
            .count(),
        1,
        "{after:?}"
    );
    assert_eq!(git_out(repo, &["status", "--porcelain"]), "");
}

#[test]
fn a_foreign_commit_that_needs_the_plan_is_refused_and_nothing_moves() {
    let env = with_foo();
    let repo = env.repo();
    write(repo, "src/a.rs", "// a, edited by someone else\n");
    commit(repo, "drive-by edit of a");
    dirty(repo);
    let before = seen(repo);
    let sha = git_out(repo, &["rev-parse", "--short=12", "HEAD"]);

    let err = push(repo).unwrap_err().to_string();
    assert!(err.contains(&sha) && err.contains("src/a.rs"), "{err}");
    assert_eq!(seen(repo), before);
}

#[test]
fn every_kind_of_uncommitted_work_is_one_automatic_commit_that_pop_restores() {
    let env = with_foo();
    let repo = env.repo();
    dirty(repo);
    let disk = |repo: &Path| {
        [
            "src/new.rs",
            "src/base.rs",
            "src/a.rs",
            ".clank/plans/foo.md",
        ]
        .map(|p| read(repo, p))
    };
    let before = disk(repo);

    push(repo).unwrap();
    assert_eq!(git_out(repo, &["status", "--porcelain"]), "");
    assert_eq!(
        subjects(repo, "refs/clank/stash/foo")[0],
        "[foo] clank stash: uncommitted work at stash time"
    );
    let record = read(repo, ".clank/stash/foo.json").unwrap();
    let snap = git_out(repo, &["rev-parse", "refs/clank/stash/foo"]);
    assert!(
        record.contains(&snap),
        "the automatic commit is set aside: {record}"
    );
    assert_eq!(read(repo, "src/base.rs").as_deref(), Some("// base\n"));
    assert!(read(repo, "src/new.rs").is_none());

    pop(repo).unwrap();
    assert_eq!(disk(repo), before);
    assert_eq!(git_out(repo, &["status", "--porcelain"]), "");
}

#[test]
fn a_clean_tree_makes_no_automatic_commit() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    let shas = git_out(repo, &["rev-list", "main..refs/clank/stash/foo"]);
    assert_eq!(shas.lines().count(), 3, "{shas}");
    assert!(
        !subjects(repo, "refs/clank/stash/foo")
            .iter()
            .any(|s| s.contains("uncommitted work"))
    );
}

#[test]
fn an_untracked_directory_where_a_file_returns_is_refused_and_nothing_moves() {
    let env = with_foo();
    let repo = env.repo();
    write(repo, "src/old.rs/note.txt", "mine\n");
    dirty(repo);
    let before = seen(repo);
    let err = push(repo).unwrap_err().to_string();
    assert!(err.contains("src/old.rs"), "{err}");
    assert_eq!(seen(repo), before);
    assert_eq!(read(repo, "src/old.rs/note.txt").as_deref(), Some("mine\n"));
}

#[test]
fn an_ignored_file_where_a_file_returns_is_refused() {
    let env = with_foo();
    let repo = env.repo();
    let exclude = git_out(repo, &["rev-parse", "--git-path", "info/exclude"]);
    std::fs::create_dir_all(repo.join(&exclude).parent().unwrap()).unwrap();
    std::fs::write(repo.join(&exclude), "src/old.rs\n").unwrap();
    write(repo, "src/old.rs", "ignored, but mine\n");
    let before = seen(repo);
    let err = push(repo).unwrap_err().to_string();
    assert!(err.contains("src/old.rs"), "{err}");
    assert_eq!(seen(repo), before);
}

#[test]
fn an_ignored_file_appearing_after_the_fence_is_not_overwritten() {
    let env = with_foo();
    let repo = env.repo();
    let exclude = git_out(repo, &["rev-parse", "--git-path", "info/exclude"]);
    std::fs::create_dir_all(repo.join(&exclude).parent().unwrap()).unwrap();
    std::fs::write(repo.join(&exclude), "src/old.rs\n").unwrap();
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    let err = push_hooked(repo, |stage, repo| {
        if stage == Stage::Fenced {
            std::fs::write(repo.join("src/old.rs"), "ignored, arrived late\n").unwrap();
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("src/old.rs"), "{err}");
    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        read(repo, "src/old.rs").as_deref(),
        Some("ignored, arrived late\n")
    );
    assert!(!repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn untracked_and_ignored_files_elsewhere_are_left_alone() {
    let env = with_foo();
    let repo = env.repo();
    write(repo, ".gitignore", "*.log\n");
    commit(repo, "[foo] ignore logs");
    write(repo, "notes.txt", "untracked\n");
    write(repo, "run.log", "ignored\n");
    push(repo).unwrap();
    assert_eq!(read(repo, "notes.txt").as_deref(), Some("untracked\n"));
    assert_eq!(read(repo, "run.log").as_deref(), Some("ignored\n"));
}

#[test]
fn a_dirty_dry_run_changes_nothing() {
    let env = with_foo();
    let repo = env.repo();
    dirty(repo);
    let before = seen(repo);
    block_on(clank::cli::stash::push_with(
        args(repo, true),
        &mut |_, _| {},
    ))
    .unwrap();
    assert_eq!(seen(repo), before);
    assert!(
        !repo.join(".clank/stash").exists(),
        "not even the lock file"
    );
}

#[test]
fn an_already_stashed_plan_is_an_answer_even_dirty_and_with_another_active() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    dirty_after_stash(repo);
    let before = seen(repo);
    push(repo).unwrap();
    assert_eq!(seen(repo), before, "no automatic commit, nothing moved");

    write(repo, ".clank/plans/bar.md", "# bar\n");
    git(repo, &["add", ".clank/plans/bar.md"]);
    git(repo, &["commit", "--quiet", "-m", "[bar] intro"]);
    let before = seen(repo);
    push(repo).unwrap();
    assert_eq!(seen(repo), before);
}

/// Dirty work on a branch where `foo` is stashed.
fn dirty_after_stash(repo: &Path) {
    write(repo, "src/base.rs", "// base, edited\n");
    write(repo, "src/staged.rs", "// staged\n");
    git(repo, &["add", "src/staged.rs"]);
}

#[test]
fn a_detached_head_is_stashed_where_it_is() {
    let env = with_foo();
    let repo = env.repo();
    git(repo, &["checkout", "--quiet", "--detach"]);
    let main = git_out(repo, &["rev-parse", "main"]);
    push(repo).unwrap();
    assert_eq!(git_out(repo, &["symbolic-ref", "-q", "HEAD"]), "");
    assert_eq!(subjects(repo, "HEAD"), ["[misc] base"]);
    assert_eq!(
        git_out(repo, &["rev-parse", "main"]),
        main,
        "main is not rewritten"
    );
}

#[test]
fn a_foreign_merge_is_kept_as_one_commit_with_its_content() {
    let env = with_foo();
    let repo = env.repo();
    git(repo, &["checkout", "--quiet", "-b", "side", "HEAD~1"]);
    write(repo, "src/side.rs", "// side\n");
    commit(repo, "side work");
    git(repo, &["checkout", "--quiet", "main"]);
    git(
        repo,
        &["merge", "--quiet", "--no-ff", "-m", "merge side", "side"],
    );
    push(repo).unwrap();
    assert_eq!(subjects(repo, "HEAD"), ["merge side", "[misc] base"]);
    assert_eq!(read(repo, "src/side.rs").as_deref(), Some("// side\n"));
    assert_eq!(
        git_out(repo, &["rev-list", "--parents", "-1", "HEAD"])
            .split(' ')
            .count(),
        2,
        "one parent"
    );
    assert!(read(repo, "src/a.rs").is_none());
}

#[test]
fn a_plan_that_is_the_whole_history_leaves_an_empty_root_and_pops_onto_it() {
    let env = env();
    let repo = env.repo();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/a.rs", "// a\n");
    commit(repo, "[foo] impl a");
    push(repo).unwrap();
    assert_eq!(
        subjects(repo, "HEAD"),
        ["clank stash: foo set aside; it was this branch's whole history"]
    );
    assert_eq!(git_out(repo, &["ls-tree", "-r", "HEAD"]), "");
    pop(repo).unwrap();
    assert_eq!(read(repo, "src/a.rs").as_deref(), Some("// a\n"));
}

#[test]
fn pop_commits_independent_dirty_work_untagged_first() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    dirty_after_stash(repo);
    pop(repo).unwrap();
    let log = subjects(repo, "HEAD");
    let at = log
        .iter()
        .position(|s| s == "clank stash pop: uncommitted work at pop time")
        .unwrap_or_else(|| panic!("{log:?}"));
    assert_eq!(&log[..at], ["[foo] impl b", "[foo] impl a", "[foo] intro"]);
    assert_eq!(
        read(repo, "src/base.rs").as_deref(),
        Some("// base, edited\n")
    );
    assert_eq!(git_out(repo, &["status", "--porcelain"]), "");
}

#[test]
fn pop_with_another_plan_active_and_dirty_work_is_refused_before_anything() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");
    dirty_after_stash(repo);
    let before = seen(repo);
    let err = pop(repo).unwrap_err().to_string();
    assert!(
        err.contains("bar") && err.contains("already active"),
        "{err}"
    );
    let mut after = seen(repo);
    after.stash_record = before.stash_record;
    assert_eq!(after, before);
    assert!(repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn pop_with_dirty_work_that_conflicts_is_refused_and_nothing_moves() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    write(repo, "src/a.rs", "// someone else's a\n");
    git(repo, &["add", "src/a.rs"]);
    let before = seen(repo);
    let err = pop(repo).unwrap_err().to_string();
    assert!(err.contains("src/a.rs"), "{err}");
    assert_eq!(seen(repo), before);
    let pick = git_out(repo, &["rev-parse", "--git-path", "CHERRY_PICK_HEAD"]);
    assert!(!repo.join(pick).exists());
}

#[test]
fn an_edit_landing_after_the_checks_is_taken_into_the_automatic_commit() {
    let env = with_foo();
    let repo = env.repo();
    write(repo, "src/base.rs", "// first edit\n");
    let mut once = true;
    push_hooked(repo, |stage, repo| {
        if stage == Stage::Checked && std::mem::take(&mut once) {
            std::fs::write(repo.join("src/base.rs"), "// newer edit\n").unwrap();
        }
    })
    .unwrap();
    assert_eq!(read(repo, "src/base.rs").as_deref(), Some("// base\n"));
    pop(repo).unwrap();
    assert_eq!(
        read(repo, "src/base.rs").as_deref(),
        Some("// newer edit\n")
    );
}

#[test]
fn staging_after_the_checks_is_rebuilt_and_kept() {
    let env = with_foo();
    let repo = env.repo();
    write(repo, "src/staged.rs", "// on disk, untracked at first\n");
    let mut once = true;
    push_hooked(repo, |stage, repo| {
        if stage == Stage::Checked && std::mem::take(&mut once) {
            git(repo, &["add", "src/staged.rs"]);
        }
    })
    .unwrap();
    assert!(
        read(repo, "src/staged.rs").is_none(),
        "staged, so set aside"
    );
    pop(repo).unwrap();
    assert_eq!(
        read(repo, "src/staged.rs").as_deref(),
        Some("// on disk, untracked at first\n")
    );
}

#[test]
fn a_switch_to_another_branch_at_the_same_commit_is_refused() {
    let env = with_foo();
    let repo = env.repo();
    let main = git_out(repo, &["rev-parse", "main"]);
    let err = push_hooked(repo, |stage, repo| {
        if stage == Stage::Checked {
            git(repo, &["checkout", "--quiet", "-b", "other"]);
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("`main`") && err.contains("`other`"), "{err}");
    assert_eq!(git_out(repo, &["rev-parse", "main"]), main);
    assert_eq!(git_out(repo, &["rev-parse", "other"]), main);
    assert!(!git_out(repo, &["show-ref"]).contains("refs/clank/stash/foo"));
    assert!(!repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn a_commit_landing_after_the_checks_is_refused() {
    let env = with_foo();
    let repo = env.repo();
    let err = push_hooked(repo, |stage, repo| {
        if stage == Stage::Checked {
            write(repo, "src/late.rs", "// late\n");
            commit(repo, "[foo] late");
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("moved"), "{err}");
    assert!(subjects(repo, "HEAD").contains(&"[foo] late".to_string()));
    assert!(!repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn a_commit_landing_after_the_fence_is_never_rewritten_away() {
    let env = with_foo();
    let repo = env.repo();
    let err = push_hooked(repo, |stage, repo| {
        if stage == Stage::Fenced {
            write(repo, "src/late.rs", "// late\n");
            commit(repo, "[foo] late");
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("did not move"), "{err}");
    assert_eq!(subjects(repo, "HEAD")[0], "[foo] late");
    assert_eq!(read(repo, "src/late.rs").as_deref(), Some("// late\n"));
    assert!(!git_out(repo, &["show-ref"]).contains("refs/clank/stash/foo"));
    assert!(!repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn an_edit_landing_after_the_fence_rolls_everything_back() {
    let env = with_foo();
    let repo = env.repo();
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    let err = push_hooked(repo, |stage, repo| {
        if stage == Stage::Fenced {
            std::fs::write(repo.join("src/a.rs"), "// edited at the last moment\n").unwrap();
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("src/a.rs"), "{err}");
    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git_out(repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(
        read(repo, "src/a.rs").as_deref(),
        Some("// edited at the last moment\n")
    );
    assert!(!git_out(repo, &["show-ref"]).contains("refs/clank/stash/foo"));
    assert!(!repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn pop_rebuilds_for_an_edit_after_the_checks() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    let mut once = true;
    pop_hooked(repo, |stage, repo| {
        if stage == Stage::Checked && std::mem::take(&mut once) {
            std::fs::write(repo.join("src/base.rs"), "// edited mid-pop\n").unwrap();
        }
    })
    .unwrap();
    assert_eq!(
        read(repo, "src/base.rs").as_deref(),
        Some("// edited mid-pop\n")
    );
    assert!(
        subjects(repo, "HEAD").contains(&"clank stash pop: uncommitted work at pop time".into())
    );
}

#[test]
fn pop_refuses_a_branch_switch_after_the_checks() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    let main = git_out(repo, &["rev-parse", "main"]);
    let err = pop_hooked(repo, |stage, repo| {
        if stage == Stage::Checked {
            git(repo, &["checkout", "--quiet", "-b", "other"]);
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("`other`"), "{err}");
    assert_eq!(git_out(repo, &["rev-parse", "main"]), main);
    assert_eq!(git_out(repo, &["rev-parse", "other"]), main);
    assert!(
        repo.join(".clank/stash/foo.json").exists(),
        "the stash is untouched"
    );
}

/// A filter git must run to write `path`, and that always fails: the
/// check before writing passes, the write itself does not.
fn failing_filter(repo: &Path, path: &str) {
    git(repo, &["config", "filter.boom.smudge", "false"]);
    git(repo, &["config", "filter.boom.clean", "cat"]);
    git(repo, &["config", "filter.boom.required", "true"]);
    let attributes = git_out(repo, &["rev-parse", "--git-path", "info/attributes"]);
    std::fs::create_dir_all(repo.join(&attributes).parent().unwrap()).unwrap();
    std::fs::write(repo.join(&attributes), format!("{path} filter=boom\n")).unwrap();
}

#[test]
fn a_push_whose_write_fails_part_way_keeps_everything_reachable_and_says_so() {
    let env = with_foo();
    let repo = env.repo();
    failing_filter(repo, "src/old.rs");
    write(repo, "src/a.rs", "// only copy of this edit\n");
    let err = push(repo).unwrap_err().to_string();
    assert!(err.contains("partly written"), "{err}");
    assert!(!err.contains("Nothing moved"), "{err}");
    assert!(git_out(repo, &["show-ref"]).contains("refs/clank/stash/foo"));
    assert!(repo.join(".clank/stash/foo.json").exists());
    assert_eq!(
        git_out(repo, &["show", "refs/clank/stash/foo:src/a.rs"]),
        "// only copy of this edit"
    );
    assert_eq!(subjects(repo, "HEAD"), ["[misc] base"]);
}

#[test]
fn a_pop_whose_write_fails_part_way_keeps_the_work_from_before_it_reachable() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    failing_filter(repo, "src/a.rs");
    write(repo, "src/base.rs", "// only copy, before the pop\n");
    let err = pop(repo).unwrap_err().to_string();
    assert!(err.contains("partly written"), "{err}");
    let log = subjects(repo, "HEAD");
    assert!(
        log.contains(&"clank stash pop: uncommitted work at pop time".to_string()),
        "{log:?}"
    );
    let snap = git_out(
        repo,
        &[
            "log",
            "--format=%H",
            "--grep=uncommitted work at pop time",
            "-1",
            "HEAD",
        ],
    );
    assert_eq!(
        git_out(repo, &["show", &format!("{snap}:src/base.rs")]),
        "// only copy, before the pop"
    );
    assert!(git_out(repo, &["show-ref"]).contains("refs/clank/stash/foo"));
    assert!(repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn a_branch_switch_after_the_fence_rewrites_neither_branch() {
    let env = with_foo();
    let repo = env.repo();
    let main = git_out(repo, &["rev-parse", "main"]);
    let err = push_hooked(repo, |stage, repo| {
        if stage == Stage::Fenced {
            git(repo, &["checkout", "--quiet", "-b", "other"]);
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("no longer"), "{err}");
    assert_eq!(git_out(repo, &["rev-parse", "main"]), main);
    assert_eq!(git_out(repo, &["rev-parse", "other"]), main);
    assert_eq!(git_out(repo, &["symbolic-ref", "HEAD"]), "refs/heads/other");
    assert_eq!(git_out(repo, &["status", "--porcelain"]), "");
    assert_eq!(read(repo, "src/a.rs").as_deref(), Some("// a\n"));
    assert!(!git_out(repo, &["show-ref"]).contains("refs/clank/stash/foo"));
    assert!(!repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn attaching_a_detached_head_after_the_fence_is_refused() {
    let env = with_foo();
    let repo = env.repo();
    git(repo, &["checkout", "--quiet", "--detach"]);
    let main = git_out(repo, &["rev-parse", "main"]);
    let err = push_hooked(repo, |stage, repo| {
        if stage == Stage::Fenced {
            git(repo, &["checkout", "--quiet", "main"]);
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("no longer"), "{err}");
    assert_eq!(git_out(repo, &["symbolic-ref", "HEAD"]), "refs/heads/main");
    assert_eq!(git_out(repo, &["rev-parse", "main"]), main);
    assert!(!repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn pop_refuses_a_branch_switch_after_the_fence() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    let main = git_out(repo, &["rev-parse", "main"]);
    let err = pop_hooked(repo, |stage, repo| {
        if stage == Stage::Fenced {
            git(repo, &["checkout", "--quiet", "-b", "other"]);
        }
    })
    .unwrap_err()
    .to_string();
    assert!(err.contains("no longer"), "{err}");
    assert_eq!(git_out(repo, &["rev-parse", "main"]), main);
    assert_eq!(git_out(repo, &["rev-parse", "other"]), main);
    assert!(repo.join(".clank/stash/foo.json").exists());
}

/// A smudge that makes the HEAD lock unwritable while the working tree
/// is being written: the write completes, or `fails` it too, and then
/// moving a detached HEAD fails (codex's fault on 56f85fb).
fn head_write_fails_after_checkout(repo: &Path, path: &str, fails: bool) {
    failing_filter(repo, path);
    let then = if fails { "false" } else { "cat" };
    git(
        repo,
        &[
            "config",
            "filter.boom.smudge",
            &format!("chmod 444 .git/HEAD.lock; {then}"),
        ],
    );
}

#[test]
fn a_detached_head_that_cannot_move_after_the_checkout_keeps_the_dirty_snapshot() {
    let env = with_foo();
    let repo = env.repo();
    git(repo, &["checkout", "--quiet", "--detach"]);
    let head = git_out(repo, &["rev-parse", "HEAD"]);
    head_write_fails_after_checkout(repo, "src/old.rs", false);
    write(repo, "src/a.rs", "// only copy of this edit\n");
    let err = format!("{:#}", push(repo).unwrap_err());
    assert!(
        err.contains("HEAD could not be moved") && err.contains("still at"),
        "{err}"
    );
    assert_eq!(git_out(repo, &["rev-parse", "HEAD"]), head);
    assert!(repo.join(".clank/stash/foo.json").exists());
    assert_eq!(
        git_out(repo, &["show", "refs/clank/stash/foo:src/a.rs"]),
        "// only copy of this edit"
    );
}

#[test]
fn a_pop_whose_detached_head_cannot_move_keeps_the_pre_pop_snapshot() {
    let env = env();
    let repo = env.repo();
    write(repo, "src/base.rs", "one\ntwo\nthree\nfour\nfive\n");
    commit(repo, "[misc] base");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    write(repo, "src/base.rs", "PLAN\ntwo\nthree\nfour\nfive\n");
    commit(repo, "[foo] intro");
    push(repo).unwrap();
    git(repo, &["checkout", "--quiet", "--detach"]);
    head_write_fails_after_checkout(repo, "src/base.rs", true);
    write(repo, "src/base.rs", "one\ntwo\nthree\nfour\nONLY COPY\n");
    let err = format!("{:#}", pop(repo).unwrap_err());
    assert!(err.contains("refs/clank/stash-pop/foo"), "{err}");
    let snap = git_out(
        repo,
        &[
            "log",
            "--format=%H",
            "--grep=uncommitted work at pop time",
            "-1",
            "refs/clank/stash-pop/foo",
        ],
    );
    assert!(!snap.is_empty(), "the pre-pop snapshot is reachable");
    assert_eq!(
        git_out(repo, &["show", &format!("{snap}:src/base.rs")]),
        "one\ntwo\nthree\nfour\nONLY COPY"
    );
    assert!(repo.join(".clank/stash/foo.json").exists());
}

#[test]
fn a_successful_pop_leaves_no_ref_of_its_own() {
    let env = with_foo();
    let repo = env.repo();
    push(repo).unwrap();
    dirty_after_stash(repo);
    pop(repo).unwrap();
    assert!(
        !git_out(repo, &["show-ref"]).contains("refs/clank/stash"),
        "{}",
        git_out(repo, &["show-ref"])
    );
}

#[test]
fn a_pop_after_a_partial_one_refuses_and_keeps_its_recovery_ref() {
    let env = env();
    let repo = env.repo();
    write(repo, "src/base.rs", "one\ntwo\nthree\nfour\nfive\n");
    commit(repo, "[misc] base");
    write(repo, ".clank/plans/foo.md", "# foo\n");
    write(repo, "src/base.rs", "PLAN\ntwo\nthree\nfour\nfive\n");
    commit(repo, "[foo] intro");
    push(repo).unwrap();
    git(repo, &["checkout", "--quiet", "--detach"]);
    head_write_fails_after_checkout(repo, "src/base.rs", true);
    write(repo, "src/base.rs", "one\ntwo\nthree\nfour\nONLY COPY\n");
    pop(repo).unwrap_err();
    let held = git_out(repo, &["rev-parse", "refs/clank/stash-pop/foo"]);

    // The operator fixes the filter and tidies the checkout, but has not
    // recovered from the held result yet.
    git(repo, &["config", "--unset", "filter.boom.required"]);
    git(repo, &["config", "filter.boom.smudge", "cat"]);
    git(repo, &["reset", "--quiet", "--hard"]);
    let _ = std::fs::remove_file(repo.join(".clank/plans/foo.md"));
    let err = pop(repo).unwrap_err().to_string();
    assert!(
        err.contains("stopped part way") && err.contains("refs/clank/stash-pop/foo"),
        "{err}"
    );
    assert_eq!(
        git_out(repo, &["rev-parse", "refs/clank/stash-pop/foo"]),
        held
    );
    assert!(repo.join(".clank/stash/foo.json").exists());

    // Recovered and removed by hand: pop works again.
    git(repo, &["update-ref", "-d", "refs/clank/stash-pop/foo"]);
    pop(repo).unwrap();
    assert!(!repo.join(".clank/stash/foo.json").exists());
}
