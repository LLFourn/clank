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
    let page = std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
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
    let page = std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
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
    let page = std::fs::read_to_string(repo.join(format!(".clank/html/commit/{sha}.html"))).unwrap();
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
