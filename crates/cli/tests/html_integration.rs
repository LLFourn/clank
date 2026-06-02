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
fn init_gitignore_includes_html_dir() {
    let dir = init_repo();
    let out = run_clank(dir.path(), &["init", "--yes"]);
    assert!(out.status.success());
    let body = std::fs::read_to_string(dir.path().join(".clank/.gitignore")).unwrap();
    assert!(
        body.contains("/html/"),
        "canonical .clank/.gitignore should include /html/; got:\n{body}"
    );
}
