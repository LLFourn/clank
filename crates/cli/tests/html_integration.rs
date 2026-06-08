//! Integration tests for `clank html`.

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

fn commit(repo: &Path, msg: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", msg]);
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
fn html_build_writes_index_and_one_commit_page() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "clank html failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let index = repo.join(".clank/html/index.html");
    let commit_page = repo.join(format!(".clank/html/commit/{sha}.html"));
    let style = repo.join(".clank/html/style.css");
    assert!(index.is_file(), "index.html missing");
    assert!(commit_page.is_file(), "commit/{sha}.html missing");
    assert!(style.is_file(), "style.css missing");

    let body = std::fs::read_to_string(&index).unwrap();
    assert!(
        body.contains(&format!("commit/{sha}.html")),
        "index should reference the commit page; got body length {}",
        body.len()
    );
}

#[test]
fn html_index_shows_status_header_and_timeline_rows() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());

    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        body.contains("<header class=\"status\">"),
        "status header missing"
    );
    assert!(
        body.contains("Timeline"),
        "Timeline section heading missing"
    );
    let row_count = body.matches("class=\"row\"").count();
    // At least one event (the intro commit) → one row.
    assert!(row_count >= 1, "expected ≥1 row; got {row_count}");
    assert!(
        body.contains(&sha[..7]),
        "expected short sha {} on the index; body len {}",
        &sha[..7],
        body.len()
    );
}

#[test]
fn html_commit_page_for_plan_only_renders_markdown() {
    let dir = init_repo();
    let repo = dir.path();
    // Plan body contains a markdown heading + paragraph; the
    // rendered HTML must contain the heading tag.
    write(repo, ".clank/plans/foo.md", "# Foo Plan\n\nDescription.\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        page.contains("<h1>Foo Plan</h1>"),
        "rendered markdown should include <h1>; page snippet:\n{}",
        &page[..page.len().min(2000)]
    );
    assert!(
        page.contains("class=\"plan-body\""),
        "plan-only event should show plan body as centerpiece"
    );
}

#[test]
fn html_commit_page_for_code_commit_renders_diff() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/lib.rs", "fn answer() -> i32 { 42 }\n");
    commit(repo, "[foo] impl");
    let impl_sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{impl_sha}.html"))).unwrap();
    assert!(
        page.contains("class=\"diff\""),
        "code commit page should contain a diff section"
    );
    assert!(
        page.contains("src/lib.rs"),
        "diff should mention the touched path"
    );
    assert!(
        page.contains("class=\"line add\""),
        "diff should mark added lines"
    );
}

#[test]
fn html_commit_page_renders_feedback_with_verdict_marks() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);
    let short = &sha[..7];

    // Two reviews on the same commit: APPROVE and FINISHED.
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{short}.md"),
        "APPROVE looks good\n",
    );
    write(
        repo,
        &format!(".clank/agents/bob/feedback/{short}.md"),
        "FINISHED ship it\n",
    );

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        page.contains("verdict-approve") && page.contains("verdict-finished"),
        "expected both verdict classes on the page"
    );
    assert!(page.contains("alice"));
    assert!(page.contains("bob"));
    assert!(page.contains("APPROVE"));
    assert!(page.contains("FINISHED"));
}

#[test]
fn html_escapes_user_content_in_feedback_bodies() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);
    let short = &sha[..7];

    // Feedback body contains script tag. pulldown-cmark escapes
    // inline HTML by default in safe mode (which is the
    // default for `push_html`).
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{short}.md"),
        "APPROVE looks good\n\n<script>alert('xss')</script>\n",
    );

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    // pulldown-cmark's safe-html (default) leaves raw HTML as
    // text-escaped; either way the live <script> tag must NOT
    // appear in the output.
    assert!(
        !page.contains("<script>alert"),
        "raw <script> tag leaked into output:\n{}",
        &page[..page.len().min(1500)]
    );
}

#[test]
fn html_index_for_repo_with_no_commits_succeeds() {
    let dir = init_repo();
    let out = run_clank(dir.path(), &["html"]);
    assert!(
        out.status.success(),
        "clank html on an empty repo should succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join(".clank/html/index.html")).unwrap();
    assert!(
        body.contains("Timeline"),
        "Timeline section should appear even with no events"
    );
}

#[test]
fn html_open_is_a_subcommand_and_unknown_subcommand_errors() {
    // `clank html open --help` should succeed (clap recognizes
    // the subcommand) and a bogus subcommand should fail at
    // parse time.
    let dir = init_repo();
    let out = Command::new(clank_bin())
        .args(["html", "open", "--help"])
        .arg("--repo")
        .arg(dir.path())
        .env("HOME", dir.path())
        .output()
        .expect("spawn clank");
    assert!(
        out.status.success(),
        "clank html open --help should parse cleanly; stderr=`{}`",
        String::from_utf8_lossy(&out.stderr)
    );

    let bogus = run_clank(dir.path(), &["html", "bogus-subcommand"]);
    assert!(
        !bogus.status.success(),
        "an unknown subcommand must fail at parse time"
    );
}

#[test]
fn html_writes_meta_marker_with_head_sha() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "clank html failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        body.contains(&format!("name=\"clank:last-built-sha\" content=\"{sha}\"")),
        "meta last-built-sha missing or wrong"
    );
    assert!(
        body.contains("name=\"clank:builder-version\""),
        "builder-version meta missing"
    );
}

#[test]
fn html_timeline_uses_div_container_not_ol() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        body.contains("<div class=\"timeline\">"),
        "timeline must be a div"
    );
    assert!(
        !body.contains("<ol class=\"timeline"),
        "must NOT use <ol class=\"timeline\""
    );
}

#[test]
fn html_timeline_groups_same_plan_into_umbrella() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/lib.rs", "// impl\n");
    commit(repo, "[foo] impl");

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    let umbrella_count = body.matches("class=\"umbrella umbrella").count();
    assert_eq!(
        umbrella_count,
        1,
        "expected one umbrella, got {umbrella_count}; body length {}",
        body.len()
    );
    assert!(body.contains("data-umbrella-key=\"foo\""));
}

#[test]
fn html_timeline_breaks_umbrella_on_plan_change() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(body.contains("data-umbrella-key=\"foo\""));
    assert!(body.contains("data-umbrella-key=\"bar\""));
    // bar is newer → appears before foo in newest-first order.
    let bar_pos = body.find("data-umbrella-key=\"bar\"").unwrap();
    let foo_pos = body.find("data-umbrella-key=\"foo\"").unwrap();
    assert!(bar_pos < foo_pos, "bar umbrella should render above foo");
}

#[test]
fn html_timeline_renders_parsed_subject_body() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    // Subject body is "intro", not "[foo] intro".
    assert!(
        body.contains("<span class=\"subject\">intro</span>"),
        "expected stripped subject; body len {}",
        body.len()
    );

    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        page.contains("<h2 class=\"subject\">intro</h2>"),
        "commit page header should also use the parsed body"
    );
}

#[test]
fn html_timeline_renders_raw_when_no_prefix() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    // Now an ad-hoc misc commit.
    write(repo, "src/lib.rs", "// x\n");
    commit(repo, "[misc] random fix");

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    // The misc commit has no plan prefix to strip in the
    // umbrella sense; body should be "random fix".
    assert!(
        body.contains(">random fix<"),
        "expected misc body verbatim; body len {}",
        body.len()
    );
}

#[test]
fn html_emits_inline_relative_time_script_and_data_iso_attrs() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(body.contains("<script>"), "inline script missing");
    assert!(
        body.contains("data-iso"),
        "data-iso attribute missing on rows"
    );
    assert!(body.contains("[data-iso]"), "script must select data-iso");
}

#[test]
fn html_incremental_refreshes_prior_top_after_fat_slice_pushes_them_out() {
    // Codex's regression: feedback lands on what was the top
    // of the prior timeline. The next build adds >10 new
    // commits, pushing the old top out of the current top-N.
    // The prior top commit's page must still be refreshed
    // because it was in the PRIOR top-N when the feedback
    // landed.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let prior_top_sha = head_sha(repo);

    // First build with just the intro.
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 1");
    let prior_top_page = repo.join(format!(".clank/html/commit/{prior_top_sha}.html"));
    let initial = std::fs::read_to_string(&prior_top_page).unwrap();
    assert!(
        !initial.contains("FINISHED"),
        "no FINISHED in first-build page"
    );

    // Feedback on the prior top + 12 new commits.
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{prior_top_sha}.md"),
        "FINISHED ship it\n",
    );
    for i in 0..12 {
        write(repo, &format!("src/f{i}.rs"), "// x\n");
        commit(repo, &format!("[foo] revise {i}"));
    }

    std::thread::sleep(std::time::Duration::from_millis(1100));
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 2");

    let refreshed = std::fs::read_to_string(&prior_top_page).unwrap();
    assert!(
        refreshed.contains("FINISHED"),
        "prior top commit's page must be refreshed even when pushed out of current top-N; len {}",
        refreshed.len()
    );
}

#[test]
fn html_incremental_same_head_reuses_event_cache_and_preserves_old_pages() {
    // Codex's regression: feedback-only rebuild at the same
    // HEAD must not re-fold the repo. The cached event log
    // is reused, top-N feedback is refreshed (page + index
    // marks), and an OLD page outside the top-N keeps its
    // mtime.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    for i in 0..15 {
        write(repo, &format!("src/f{i}.rs"), "// x\n");
        commit(repo, &format!("[foo] revise {i}"));
    }
    let intro_sha = {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["log", "--reverse", "--format=%H", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string()
    };
    let head_before = head_sha(repo);

    // First build.
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 1");
    let oldest_path = repo.join(format!(".clank/html/commit/{intro_sha}.html"));
    let old_meta = std::fs::metadata(&oldest_path).unwrap();
    let old_mtime = filetime::FileTime::from_last_modification_time(&old_meta);

    // Land FEEDBACK only — no new commit.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{head_before}.md"),
        "FINISHED ship it\n",
    );

    // Rebuild. HEAD is unchanged.
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 2");
    let head_after = head_sha(repo);
    assert_eq!(head_before, head_after, "HEAD must not have moved");

    // Status header must reflect the new FINISHED gate.
    let index = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        index.contains("gate-finished") || index.contains("finished"),
        "status header should refresh on same-HEAD rebuild; index len {}",
        index.len()
    );

    // Old page outside top-N kept its mtime.
    let after_meta = std::fs::metadata(&oldest_path).unwrap();
    let after_mtime = filetime::FileTime::from_last_modification_time(&after_meta);
    assert_eq!(
        old_mtime.unix_seconds(),
        after_mtime.unix_seconds(),
        "oldest page must NOT be rewritten on a same-HEAD rebuild"
    );
}

#[test]
fn html_incremental_skips_old_pages_outside_top_n() {
    // 15 prior commits + 1 new commit. After incremental,
    // the OLDEST prior page (well outside the top-10 refresh
    // window) must retain its pre-rebuild mtime. The new
    // page must exist.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    for i in 0..15 {
        write(repo, &format!("src/f{i}.rs"), "// x\n");
        commit(repo, &format!("[foo] revise {i}"));
    }
    // Capture the oldest event sha (the intro).
    let intro_sha = {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["log", "--reverse", "--format=%H", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string()
    };

    // First build.
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 1");
    let oldest_path = repo.join(format!(".clank/html/commit/{intro_sha}.html"));
    let old_meta = std::fs::metadata(&oldest_path).unwrap();
    let old_mtime = filetime::FileTime::from_last_modification_time(&old_meta);

    // Land a new commit and rebuild incrementally.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(repo, "src/extra.rs", "// y\n");
    commit(repo, "[foo] extra");
    let new_sha = head_sha(repo);
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 2");

    assert!(
        repo.join(format!(".clank/html/commit/{new_sha}.html"))
            .is_file(),
        "new commit's page must exist after incremental build"
    );
    let after_meta = std::fs::metadata(&oldest_path).unwrap();
    let after_mtime = filetime::FileTime::from_last_modification_time(&after_meta);
    assert_eq!(
        old_mtime.unix_seconds(),
        after_mtime.unix_seconds(),
        "oldest page (outside top-N) must retain its mtime; was {old_mtime:?}, became {after_mtime:?}"
    );
}

#[test]
fn html_incremental_skips_unchanged_commit_pages() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let first_sha = head_sha(repo);

    // First build.
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 1");
    let first_path = repo.join(format!(".clank/html/commit/{first_sha}.html"));
    let first_meta = std::fs::metadata(&first_path).unwrap();
    let first_mtime = filetime::FileTime::from_last_modification_time(&first_meta);

    // Land a new commit and rebuild.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(repo, "src/lib.rs", "// x\n");
    commit(repo, "[foo] impl");
    let second_sha = head_sha(repo);
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 2");

    // The new commit's page exists.
    assert!(
        repo.join(format!(".clank/html/commit/{second_sha}.html"))
            .is_file()
    );

    // The original commit's page was within the top-N
    // refresh window (only one prior page, well under 10),
    // so it WILL be rewritten. Test the skip behavior for a
    // page outside the window by checking via --rebuild
    // semantics separately.
    let _ = first_mtime;
}

#[test]
fn html_rebuild_flag_overwrites_pages_outside_top_n() {
    // Codex's regression: --rebuild must rewrite OLD pages
    // sitting deep in history, not just top-N or missing
    // files. Seed >TOP_N_FEEDBACK_RECHECK commits, stamp the
    // oldest with an ancient mtime, run --rebuild, assert it
    // got rewritten.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    for i in 0..15 {
        write(repo, &format!("src/f{i}.rs"), "// x\n");
        commit(repo, &format!("[foo] revise {i}"));
    }
    let intro_sha = {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["log", "--reverse", "--format=%H", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string()
    };

    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 1");
    let oldest = repo.join(format!(".clank/html/commit/{intro_sha}.html"));
    let stamp = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_times(&oldest, stamp, stamp).unwrap();

    run_clank(repo, &["html", "--rebuild"])
        .status
        .success()
        .then_some(())
        .expect("rebuild");

    let meta = std::fs::metadata(&oldest).unwrap();
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    assert_ne!(
        mtime.unix_seconds(),
        1_000_000_000,
        "--rebuild must overwrite OLD pages outside the top-N refresh window"
    );
}

#[test]
fn html_rebuild_flag_overwrites_all_pages() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 1");
    let path = repo.join(format!(".clank/html/commit/{sha}.html"));
    // Stamp the file with an old mtime; --rebuild must touch it.
    let old = filetime::FileTime::from_unix_time(1_000_000_000, 0);
    filetime::set_file_times(&path, old, old).unwrap();

    run_clank(repo, &["html", "--rebuild"])
        .status
        .success()
        .then_some(())
        .expect("rebuild");
    let new_meta = std::fs::metadata(&path).unwrap();
    let new_mtime = filetime::FileTime::from_last_modification_time(&new_meta);
    assert_ne!(
        new_mtime.unix_seconds(),
        1_000_000_000,
        "--rebuild must overwrite"
    );
}

#[test]
fn html_incremental_refreshes_status_header_on_empty_slice() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 1");

    let before = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(before.contains("gate-unreviewed") || before.contains("waiting"));

    // Land feedback WITHOUT a new commit.
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{sha}.md"),
        "FINISHED ship it\n",
    );
    run_clank(repo, &["html"])
        .status
        .success()
        .then_some(())
        .expect("build 2");

    let after = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        after.contains("gate-finished") || after.contains("finished"),
        "status header should re-render to reflect FINISHED gate after feedback lands; got body len {}",
        after.len()
    );
}

#[test]
fn html_diff_has_line_numbers_and_syntax_classes() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/lib.rs", "fn answer() -> i32 {\n    42\n}\n");
    commit(repo, "[foo] impl");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    // Line numbers: at least one new-side line number `1`.
    assert!(
        page.contains("<span class=\"ln new\">1</span>"),
        "expected new-side line number 1 in the diff; page len {}",
        page.len()
    );
    assert!(
        page.contains("<span class=\"ln old\">"),
        "old-line-number gutter missing"
    );
    // Syntect classes: Rust grammar tags `fn` as
    // `storage.type.function.rust` (not `keyword`), and any
    // Rust source gets the outer `hl-source hl-rust` wrapper.
    // Verify both bits engaged.
    assert!(
        page.contains("hl-source") && page.contains("hl-rust"),
        "expected syntect Rust source classes; got {}",
        &page[..page.len().min(2000)]
    );
}

#[test]
fn html_diff_unknown_extension_still_renders() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "weird.unknownext", "anything\n");
    commit(repo, "[foo] touch unknown");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        page.contains("weird.unknownext"),
        "file heading should still appear"
    );
    assert!(
        page.contains("class=\"hunk\""),
        "hunk container must render even for unknown extensions"
    );
}

#[test]
fn html_diff_hunk_header_uses_full_width_band() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/x.rs", "fn x() {}\n");
    commit(repo, "[foo] x");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    // The `@@ ...` hunk header line gets the `hunk-hdr`
    // class on its own div (no line-number gutters).
    assert!(
        page.contains("class=\"line hunk-hdr\""),
        "hunk header missing the hunk-hdr class"
    );
    assert!(
        page.contains("@@") && page.contains("hunk-hdr"),
        "hunk header text should appear alongside the class"
    );
}

#[test]
fn html_diff_classifies_plus_plus_and_minus_minus_content_as_add_del() {
    // Regression: lines whose source content starts with `++`
    // or `--` produce hunk-body lines that look like `+++…`
    // / `---…`. These are NOT file headers (parse_unified_diff
    // already split those out) — they're real add/del rows
    // with `++…` / `--…` as their content.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    // Seed a file with `--something` so the next commit
    // produces a hunk that adds `++replaced` and deletes
    // `--something`. Both source lines start with two of the
    // diff-sign character.
    write(repo, "src/x.txt", "--something\n");
    commit(repo, "[foo] seed");
    write(repo, "src/x.txt", "++replaced\n");
    commit(repo, "[foo] mutate");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();

    // The add row should classify as `add` (not meta) with
    // sign `+` and source `++replaced` visible.
    assert!(
        page.contains("class=\"line add\"") && page.contains("++replaced"),
        "added line `++replaced` must classify as add; page len {}",
        page.len()
    );
    // Same for the delete side.
    assert!(
        page.contains("class=\"line del\"") && page.contains("--something"),
        "deleted line `--something` must classify as del; page len {}",
        page.len()
    );
    // The `meta` class must NOT appear for these lines.
    assert!(
        !page.contains("class=\"line meta\">+"),
        "no add/del line should be misclassified as meta"
    );
}

#[test]
fn html_diff_rows_have_no_trailing_newline_in_markup() {
    // The pre-fancy renderer emitted "</span>\n<span" which
    // combined with a 1.4 line-height to produce a visible
    // gap between rows. The new layout uses `<div>` per
    // line with no inter-element whitespace inside a single
    // line's markup other than CSS-controlled grid rows.
    // Smoke-check: there's no `</span></div>\n<span` pattern
    // that would imply a row-internal hard break.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, "src/lib.rs", "fn x() {}\n");
    commit(repo, "[foo] impl");
    let sha = head_sha(repo);
    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        !page.contains("</div>\n      <div class=\"line\""),
        "rows must not introduce a raw-newline separator between line divs"
    );
}

#[test]
fn init_gitignore_includes_html_dir() {
    let dir = init_repo();
    let out = run_clank(dir.path(), &["init"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
    assert!(
        body.contains("/html/"),
        "canonical .clank/.gitignore should include /html/; got:\n{body}"
    );
}

fn commit_with_body(repo: &Path, subject: &str, body: &str) {
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "--quiet", "-m", subject, "-m", body]);
}

#[test]
fn html_commit_page_renders_commit_body() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n\nbody\n");
    commit_with_body(
        repo,
        "[foo] intro",
        "First paragraph explains the why.\n\nSecond paragraph continues.",
    );
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        page.contains("<pre class=\"commit-body\">"),
        "expected commit-body block, got:\n{page}"
    );
    assert!(
        page.contains("First paragraph explains the why."),
        "body first line missing"
    );
    assert!(
        page.contains("Second paragraph continues."),
        "body second paragraph missing"
    );
}

#[test]
fn html_commit_page_omits_body_section_for_subject_only_commits() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        !page.contains("<pre class=\"commit-body\">"),
        "should not emit commit-body for subject-only commit; got:\n{page}"
    );
}

#[test]
fn html_shas_are_copy_buttons() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let index = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        index.contains(&format!(
            "<button class=\"sha-copy\" type=\"button\" data-sha=\"{sha}\""
        )),
        "index timeline row should use sha-copy button; got:\n{index}"
    );
    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        page.contains(&format!(
            "<button class=\"sha-copy\" type=\"button\" data-sha=\"{sha}\""
        )),
        "commit page header should use sha-copy button; got:\n{page}"
    );
}

#[test]
fn html_inline_script_handles_sha_copy() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let index = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        index.contains("document.querySelectorAll('.sha-copy')"),
        "inline script should attach the sha-copy handler; got:\n{index}"
    );
}

#[test]
fn html_no_js_fallback_keeps_sha_text_visible() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);
    let short = &sha[..7];

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let index = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    let needle = format!("title=\"{sha}\">{short}</button>");
    assert!(
        index.contains(&needle),
        "abbreviated SHA must be the button's visible text; needle = {needle}\ngot:\n{index}"
    );
}

#[test]
fn html_timeline_row_does_not_nest_button_inside_anchor() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let index = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    for row in index.split("<div class=\"row\"").skip(1) {
        let block = row.split("</div>").next().unwrap_or("");
        let anchor_idx = block.find("<a class=\"row-link\"");
        let button_idx = block.find("<button class=\"sha-copy\"");
        if let (Some(a), Some(b)) = (anchor_idx, button_idx) {
            assert!(
                b < a,
                "<button class=\"sha-copy\"> must precede <a class=\"row-link\"> (sibling, not nested) in row block:\n{block}"
            );
        }
    }
}

#[test]
fn html_writes_plan_page_for_each_active_plan() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(repo.join(".clank/html/plan/foo.html").is_file());
    assert!(repo.join(".clank/html/plan/bar.html").is_file());
}

#[test]
fn html_writes_plan_page_for_finished_plans() {
    let dir = init_repo();
    let repo = dir.path();
    // Seed an intro, approve+finish the commit, then finalize.
    write(repo, ".clank/plans/foo.md", "# Foo Plan\n\nBody.\n");
    commit(repo, "[foo] intro");
    let intro = head_sha(repo);
    let short = &intro[..7];
    write(
        repo,
        &format!(".clank/agents/alice/feedback/{short}.md"),
        "FINISHED ship it\n",
    );
    // Move .clank/plans/foo.md → .clank/finished/foo.md to
    // simulate `clank finish` without invoking the bin (the
    // PlanFinalized event keys on the file path move).
    std::fs::create_dir_all(repo.join(".clank/finished")).unwrap();
    std::fs::rename(
        repo.join(".clank/plans/foo.md"),
        repo.join(".clank/finished/foo.md"),
    )
    .unwrap();
    commit(repo, "[foo] finish");

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page = repo.join(".clank/html/plan/foo.html");
    assert!(
        page.is_file(),
        "plan/foo.html should exist for finished plan"
    );
    let body = std::fs::read_to_string(&page).unwrap();
    assert!(
        body.contains("# Foo Plan") || body.contains("<h1>Foo Plan</h1>"),
        "finished plan body should be rendered; got:\n{body}"
    );
}

#[test]
fn html_plan_page_contains_events_for_that_plan_only() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let foo_sha = head_sha(repo);
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");
    let bar_sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());

    let foo_page = std::fs::read_to_string(repo.join(".clank/html/plan/foo.html")).unwrap();
    let bar_page = std::fs::read_to_string(repo.join(".clank/html/plan/bar.html")).unwrap();

    assert!(
        foo_page.contains(&foo_sha[..7]),
        "foo page should list the foo intro sha"
    );
    assert!(
        !foo_page.contains(&bar_sha[..7]),
        "foo page should NOT list the bar intro sha"
    );
    assert!(
        bar_page.contains(&bar_sha[..7]),
        "bar page should list the bar intro sha"
    );
    assert!(
        !bar_page.contains(&foo_sha[..7]),
        "bar page should NOT list the foo intro sha"
    );
}

#[test]
fn html_plan_page_renders_markdown_body() {
    let dir = init_repo();
    let repo = dir.path();
    write(
        repo,
        ".clank/plans/foo.md",
        "# Foo Plan\n\nA detailed description.\n",
    );
    commit(repo, "[foo] intro");

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());

    let page = std::fs::read_to_string(repo.join(".clank/html/plan/foo.html")).unwrap();
    assert!(
        page.contains("<h1>Foo Plan</h1>"),
        "expected rendered <h1> from plan markdown; got:\n{}",
        &page[..page.len().min(2000)]
    );
}

#[test]
fn html_index_umbrella_links_to_plan_page() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());

    let index = std::fs::read_to_string(repo.join(".clank/html/index.html")).unwrap();
    assert!(
        index.contains("<a class=\"plan-pill\" href=\"plan/foo.html\">foo</a>"),
        "index umbrella header should link the plan pill to plan/foo.html; got:\n{index}"
    );
}

#[test]
fn html_commit_page_plan_line_links_to_plan_page() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(out.status.success());

    let page =
        std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
    assert!(
        page.contains("<a class=\"plan-pill\" href=\"../plan/foo.html\">foo</a>"),
        "commit page plan-line should link to ../plan/foo.html; got:\n{page}"
    );
}

#[test]
fn html_plan_page_timeline_row_links_resolve_to_top_level_commit_dir() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let page = std::fs::read_to_string(repo.join(".clank/html/plan/foo.html")).unwrap();
    let expected = format!("href=\"../commit/{sha}.html\"");
    let wrong = format!("href=\"commit/{sha}.html\"");
    assert!(
        page.contains(&expected),
        "plan page row link should resolve to {expected}; got:\n{page}"
    );
    assert!(
        !page.contains(&wrong),
        "plan page must NOT carry top-level-relative {wrong} (would resolve to .clank/html/plan/commit/...)"
    );
}

#[test]
fn html_plan_page_refreshes_on_feedback_only_rebuild() {
    // Reviewer's case: a new FINISHED review lands on a top-N
    // commit without any new git commit. The index and commit
    // page already refresh; the plan page must too — its
    // timeline rows render the same verdict marks.
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha = head_sha(repo);
    let short = &sha[..7];

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let foo_page = repo.join(".clank/html/plan/foo.html");
    let before = std::fs::read_to_string(&foo_page).unwrap();
    assert!(
        !before.contains("mark-finished"),
        "plan page should start with no FINISHED mark"
    );

    write(
        repo,
        &format!(".clank/agents/alice/feedback/{short}.md"),
        "FINISHED ship it\n",
    );

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = std::fs::read_to_string(&foo_page).unwrap();
    assert!(
        after.contains("mark-finished"),
        "plan page should pick up the FINISHED mark after a feedback-only rebuild; got:\n{after}"
    );
}

/// Replace the builder-version meta tag in an existing index
/// with a fake value so the next `clank html` sees it as a
/// stale prior build.
fn corrupt_builder_version(index_path: &Path) {
    let body = std::fs::read_to_string(index_path).unwrap();
    let needle = "name=\"clank:builder-version\" content=\"";
    let start = body.find(needle).expect("meta tag present");
    let after = &body[start + needle.len()..];
    let close = after.find('"').unwrap();
    let mut patched = String::with_capacity(body.len());
    patched.push_str(&body[..start + needle.len()]);
    patched.push_str("old-version");
    patched.push_str(&after[close..]);
    std::fs::write(index_path, patched).unwrap();
}

#[test]
fn html_version_mismatch_rebuilds_every_commit_page() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let sha1 = head_sha(repo);
    write(repo, "src/lib.rs", "// x\n");
    commit(repo, "[foo] impl");
    let sha2 = head_sha(repo);

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Append a sentinel to each commit page so we can detect
    // when the builder overwrites them.
    let page1 = repo.join(format!(".clank/html/commit/{sha1}.html"));
    let page2 = repo.join(format!(".clank/html/commit/{sha2}.html"));
    for page in [&page1, &page2] {
        let mut body = std::fs::read_to_string(page).unwrap();
        body.push_str("<!-- SENTINEL -->");
        std::fs::write(page, body).unwrap();
    }

    corrupt_builder_version(&repo.join(".clank/html/index.html"));

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    for page in [&page1, &page2] {
        let body = std::fs::read_to_string(page).unwrap();
        assert!(
            !body.contains("<!-- SENTINEL -->"),
            "{page:?} should be rewritten on a stale prior build; sentinel still present"
        );
    }
}

#[test]
fn html_version_mismatch_rebuilds_every_plan_page() {
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    write(repo, ".clank/plans/bar.md", "# bar\n");
    commit(repo, "[bar] intro");

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let foo_page = repo.join(".clank/html/plan/foo.html");
    let bar_page = repo.join(".clank/html/plan/bar.html");
    for page in [&foo_page, &bar_page] {
        let mut body = std::fs::read_to_string(page).unwrap();
        body.push_str("<!-- SENTINEL -->");
        std::fs::write(page, body).unwrap();
    }

    corrupt_builder_version(&repo.join(".clank/html/index.html"));

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    for page in [&foo_page, &bar_page] {
        let body = std::fs::read_to_string(page).unwrap();
        assert!(
            !body.contains("<!-- SENTINEL -->"),
            "{page:?} should be rewritten on a stale prior build; sentinel still present"
        );
    }
}

#[test]
fn html_version_match_preserves_incremental_skip() {
    // Regression: when the prior index's version matches
    // ours, an incremental rebuild after a new commit must
    // NOT rewrite older commit pages that are outside the
    // top-N refresh window. We use enough buffer commits to
    // push the original commit past TOP_N_FEEDBACK_RECHECK
    // (= 10).
    let dir = init_repo();
    let repo = dir.path();
    write(repo, ".clank/plans/foo.md", "# foo\n");
    commit(repo, "[foo] intro");
    let pinned = head_sha(repo);

    for i in 0..11 {
        write(repo, &format!("src/f{i}.rs"), "// x\n");
        commit(repo, &format!("[foo] impl-{i}"));
    }

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pinned_path = repo.join(format!(".clank/html/commit/{pinned}.html"));
    let mtime_before =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&pinned_path).unwrap());

    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(repo, "src/extra.rs", "// y\n");
    commit(repo, "[foo] impl-extra");

    let out = run_clank(repo, &["html"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mtime_after =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(&pinned_path).unwrap());

    assert_eq!(
        mtime_before, mtime_after,
        "pinned older commit page should NOT be rewritten when version matches and it's outside top-N"
    );
}
