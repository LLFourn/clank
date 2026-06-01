//! Integration tests for `clank rewire` + the `post-rewrite`
//! hook installed by `clank init`.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    git(path, &["init", "--quiet", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@test"]);
    git(path, &["config", "user.name", "test"]);
    git(path, &["config", "commit.gpgsign", "false"]);
    dir
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

fn run_clank(repo: &Path, args: &[&str]) -> std::process::Output {
    Command::new(clank_bin())
        .args(args)
        .arg("--repo")
        .arg(repo)
        .env("HOME", repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .output()
        .expect("spawn clank")
}

#[test]
fn clank_init_installs_post_rewrite_hook() {
    let dir = init_repo();
    let out = run_clank(dir.path(), &["init", "--yes"]);
    assert!(
        out.status.success(),
        "clank init failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let hook = dir.path().join(".git/hooks/post-rewrite");
    assert!(hook.is_file(), "post-rewrite hook should be installed");
    let body = std::fs::read_to_string(&hook).unwrap();
    assert!(body.contains("clank rewire --from-stdin"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}

#[test]
fn clank_init_installs_hook_in_linked_worktree() {
    let main_dir = init_repo();
    let main_repo = main_dir.path();
    // Seed a commit so we can branch off.
    write(main_repo, "README.md", "x");
    git(main_repo, &["add", "-A"]);
    git(main_repo, &["commit", "--quiet", "-m", "seed"]);

    // Create a linked worktree (sibling path; git creates the dir).
    let wt_parent = tempfile::tempdir().unwrap();
    let wt_path = wt_parent.path().join("wt");
    git(
        main_repo,
        &[
            "worktree",
            "add",
            wt_path.to_str().unwrap(),
            "-b",
            "feature",
        ],
    );

    // Run clank init INSIDE the linked worktree.
    let out = run_clank(&wt_path, &["init", "--yes"]);
    assert!(
        out.status.success(),
        "clank init in linked worktree failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    // The hook should land in the main repo's shared
    // .git/hooks/ — that's where git looks for them from any
    // worktree.
    let hook = main_repo.join(".git/hooks/post-rewrite");
    assert!(
        hook.is_file(),
        "post-rewrite hook should be installed in main repo's .git/hooks/, even when init runs from the linked worktree"
    );
    let body = std::fs::read_to_string(&hook).unwrap();
    assert!(body.contains("clank rewire --from-stdin"));
}

#[test]
fn rewire_from_stdin_copies_feedback_for_simple_rename() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let old_sha = head_sha(repo);

    // Seed feedback as full-SHA filename so the scope-shas fold
    // doesn't pick it up via short form.
    let old_short = &old_sha[..7];
    let src = repo.join(format!(".clank/agents/alice/feedback/{old_short}.md"));
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "FINISHED ship it\n").unwrap();

    // Now amend the commit → new sha. We need a deterministic
    // post-rewrite pair to test against; just synthesize one.
    write(repo, "src/lib.rs", "// impl\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--amend", "--quiet", "--no-edit"]);
    let new_sha = head_sha(repo);

    let stdin_body = format!("{old_sha} {new_sha} amend\n");

    let mut child = Command::new(clank_bin())
        .args(["rewire", "--from-stdin"])
        .arg("--repo")
        .arg(repo)
        .env("HOME", repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rewire");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "rewire failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    // The reader's lookup tries full first, then short. We
    // want SOMETHING that maps to new_sha to be present.
    let new_short = &new_sha[..7];
    let candidate_full = repo.join(format!(".clank/agents/alice/feedback/{new_sha}.md"));
    let candidate_short = repo.join(format!(".clank/agents/alice/feedback/{new_short}.md"));
    assert!(
        candidate_full.is_file() || candidate_short.is_file(),
        "expected a feedback file for new sha; checked {} and {}",
        candidate_full.display(),
        candidate_short.display(),
    );
    let body = std::fs::read_to_string(if candidate_full.is_file() {
        &candidate_full
    } else {
        &candidate_short
    })
    .unwrap();
    assert_eq!(body, "FINISHED ship it\n");
}

#[test]
fn rewire_squash_keeps_only_latest_old() {
    // Synthesize three rebase pairs all squashing to one new
    // sha. Only the LAST old gets its feedback copied.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, "README.md", "x");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "seed"]);

    let old_a = "a".repeat(40);
    let old_b = "b".repeat(40);
    let old_c = "c".repeat(40);
    let new = head_sha(repo);

    // Feedback ONLY on old_a (earliest). Should NOT be copied —
    // last-old (old_c) has no feedback, so the squash drops all.
    let short_a = &old_a[..7];
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{short_a}.md"),
        "APPROVE\n",
    );

    let stdin_body = format!("{old_a} {new}\n{old_b} {new}\n{old_c} {new}\n");
    let mut child = Command::new(clank_bin())
        .args(["rewire", "--from-stdin"])
        .arg("--repo")
        .arg(repo)
        .env("HOME", repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rewire");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "rewire failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    // No feedback should land on the new sha.
    let new_short = &new[..7];
    let new_full = &new;
    let candidate_short = repo.join(format!(".clank/agents/alice/feedback/{new_short}.md"));
    let candidate_full = repo.join(format!(".clank/agents/alice/feedback/{new_full}.md"));
    assert!(
        !candidate_short.is_file() && !candidate_full.is_file(),
        "squash with no feedback on latest old should produce no copy"
    );

    // The original feedback at <short_a>.md must still exist
    // (we never move sources).
    let src = repo.join(format!(".clank/agents/alice/feedback/{short_a}.md"));
    assert!(
        src.is_file(),
        "source feedback file should be left in place"
    );
}

#[test]
fn rewire_does_not_touch_index() {
    let dir = init_repo();
    let repo = dir.path();
    // Standard root .gitignore matching what clank init writes.
    write(
        repo,
        ".gitignore",
        "/target/\n.clank/*\n!.clank/plans/\n!.clank/finished/\n",
    );
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let old_sha = head_sha(repo);

    let short = &old_sha[..7];
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{short}.md"),
        "APPROVE\n",
    );

    // Amend → new sha.
    git(repo, &["commit", "--amend", "--quiet", "--no-edit"]);
    let new_sha = head_sha(repo);

    let stdin_body = format!("{old_sha} {new_sha}\n");
    let mut child = Command::new(clank_bin())
        .args(["rewire", "--from-stdin"])
        .arg("--repo")
        .arg(repo)
        .env("HOME", repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rewire");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());

    // git status --porcelain must be empty: feedback files are
    // gitignored and rewire never stages anything.
    let porcelain = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&porcelain.stdout);
    assert!(
        stdout.trim().is_empty(),
        "rewire must not touch the index; got status:\n{stdout}"
    );
}

#[test]
fn installed_hook_rewires_on_real_amend() {
    // End-to-end: clank init installs the hook; git's amend
    // triggers it; rewire copies feedback to the new SHA. No
    // manual `clank rewire` invocation.
    let dir = init_repo();
    let repo = dir.path();
    // Init so the hook is in place. Run with HOME pointed
    // somewhere innocuous to avoid touching the user's
    // real config / agent state.
    let out = run_clank(repo, &["init", "--yes"]);
    assert!(
        out.status.success(),
        "clank init failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let old_sha = head_sha(repo);

    let old_short = &old_sha[..7];
    let src = repo.join(format!(".clank/agents/alice/feedback/{old_short}.md"));
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, "FINISHED ship it\n").unwrap();

    // Put the test clank binary on PATH so the installed
    // post-rewrite hook (`exec clank rewire --from-stdin`)
    // can find it.
    let clank_dir = Path::new(clank_bin()).parent().unwrap();
    let existing_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!(
        "{}:{}",
        clank_dir.display(),
        existing_path,
    );

    // Stage a real change so amend actually rewrites the commit.
    write(repo, "src/lib.rs", "// impl\n");
    git(repo, &["add", "-A"]);

    // Real amend → git fires post-rewrite → hook fires clank rewire.
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "--amend", "--quiet", "--no-edit"])
        .env("PATH", &new_path)
        .env("HOME", repo)
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLANK_AGENT")
        .status()
        .expect("git amend");
    assert!(status.success(), "git amend failed");
    let new_sha = head_sha(repo);
    assert_ne!(old_sha, new_sha);

    // Feedback should now be findable for the new sha (full
    // form or short, depending on what canonical_feedback_path
    // chose). At least one variant must exist.
    let new_short = &new_sha[..7];
    let candidate_full = repo.join(format!(".clank/agents/alice/feedback/{new_sha}.md"));
    let candidate_short = repo.join(format!(".clank/agents/alice/feedback/{new_short}.md"));
    assert!(
        candidate_full.is_file() || candidate_short.is_file(),
        "installed post-rewrite hook should have copied feedback for {new_sha}; \
         checked {} and {}",
        candidate_full.display(),
        candidate_short.display(),
    );
    let body = std::fs::read_to_string(if candidate_full.is_file() {
        &candidate_full
    } else {
        &candidate_short
    })
    .unwrap();
    assert_eq!(body, "FINISHED ship it\n");
}

#[test]
fn rewire_followed_by_feedback_write_overwrites_rewired_copy() {
    // Regression for codex's shadow concern: rewire writes for
    // a new sha, then a later `clank feedback write` for the
    // same author + sha must win on a subsequent read.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", "[foo] intro"]);
    let old_sha = head_sha(repo);

    let short_old = &old_sha[..7];
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{short_old}.md"),
        "APPROVE old\n",
    );

    // Amend → new sha.
    git(repo, &["commit", "--amend", "--quiet", "--no-edit"]);
    let new_sha = head_sha(repo);

    let stdin_body = format!("{old_sha} {new_sha}\n");
    let mut child = Command::new(clank_bin())
        .args(["rewire", "--from-stdin"])
        .arg("--repo")
        .arg(repo)
        .env("HOME", repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rewire");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_body.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success());

    // Now overwrite via feedback write — different verdict.
    let short_new = &new_sha[..7];
    let out = run_clank(
        repo,
        &[
            "feedback",
            "write",
            "--commit",
            short_new,
            "--verdict",
            "finished",
            "--author",
            "alice",
            "-m",
            "ship it",
        ],
    );
    assert!(
        out.status.success(),
        "feedback write failed: {}",
        String::from_utf8_lossy(&out.stderr),
    );

    // Read back via clank feedback read; the FINISHED verdict
    // must win, not the rewired APPROVE.
    let out = run_clank(repo, &["feedback", "read", "--commit", &new_sha]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("FINISHED"),
        "later feedback write must win; got:\n{stdout}"
    );
    assert!(
        !stdout.contains("APPROVE old"),
        "rewired APPROVE must not shadow the new FINISHED; got:\n{stdout}"
    );
}
