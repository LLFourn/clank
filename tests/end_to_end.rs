//! End-to-end integration test for the filesystem-truth daemon.
//! Boots the HTTP server, hits the homepage and the internal MCP
//! endpoint, asserts the new core drives correct responses.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use trinity::server;

fn run_git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {:?} failed", args);
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    run_git(path, &["init", "--quiet", "--initial-branch=main"]);
    run_git(path, &["config", "user.email", "test@test"]);
    run_git(path, &["config", "user.name", "test"]);
    run_git(path, &["config", "commit.gpgsign", "false"]);
    dir
}

fn write_file(repo: &Path, rel: &str, body: &str) {
    let abs = repo.join(rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(abs, body).unwrap();
}

fn commit(repo: &Path, msg: &str) {
    run_git(repo, &["add", "-A"]);
    run_git(repo, &["commit", "--quiet", "-m", msg]);
}

async fn spawn_daemon(repo_root: &Path) -> (String, tokio::task::JoinHandle<()>) {
    // Pick an ephemeral port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let repos_file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        repos_file.path(),
        format!("{}\n", repo_root.display()),
    )
    .unwrap();

    let args = server::ServeArgs {
        bind: addr,
        repos: repos_file.path().to_string_lossy().into_owned(),
    };

    let url = format!("http://{}", addr);
    let handle = tokio::spawn(async move {
        let _hold = repos_file; // keep the temp file alive
        let _ = server::serve(args).await;
    });

    // Wait for the server to come up.
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if reqwest::get(format!("{}/healthz", url)).await.is_ok() {
            return (url, handle);
        }
    }
    panic!("daemon did not come up");
}

#[tokio::test]
async fn home_renders_session_list() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let body = reqwest::get(format!("{}/", url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    handle.abort();
    assert!(body.contains("foo"), "home page should list 'foo'");
    assert!(body.contains("planning"), "phase should be 'planning'");
    assert!(
        body.contains("plan_needs_initial_review")
            || body.contains("Plan is committed and awaiting"),
        "should describe waiting on reviewers; got: {}",
        body
    );
}

#[tokio::test]
async fn session_detail_renders() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let body = reqwest::get(format!("{}/sessions/foo", url))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    handle.abort();
    assert!(body.contains("foo"), "session page should show id");
}

#[tokio::test]
async fn mcp_get_context_via_internal_tool_call() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "session_id": "foo" }
    });
    let resp = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap();
    handle.abort();
    assert!(resp.status().is_success(), "got {}", resp.status());
    let body: serde_json::Value = resp.json().await.unwrap();
    let result = &body["result"];
    assert_eq!(result["phase"], "planning");
    assert_eq!(result["plan_worktree_status"], "clean");
    assert_eq!(result["waiting_on"]["role"], "reviewers");
    assert_eq!(result["waiting_on"]["reason"], "plan_needs_initial_review");
}

#[tokio::test]
async fn mcp_get_context_for_uncommitted_returns_session_not_committed() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    // Don't commit.

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "session_id": "foo" }
    });
    let resp = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap();
    handle.abort();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["result"]["error"], "session_not_committed");
}

#[tokio::test]
async fn mcp_list_sessions() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");
    write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
    commit(dir.path(), "Add bar");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "list_sessions",
        "arguments": {}
    });
    let resp = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap();
    handle.abort();
    let body: serde_json::Value = resp.json().await.unwrap();
    let sessions = body["result"]["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
}

#[tokio::test]
async fn done_move_endpoint_moves_plan_file() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = client
        .post(format!("{}/sessions/foo/done", url))
        .form(&[("repo", dir.path().to_string_lossy().into_owned())])
        .send()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    assert!(
        !dir.path().join(".trinity/plans/foo.md").exists(),
        "active path should be gone"
    );
    assert!(
        dir.path().join(".trinity/plans/done/foo.md").exists(),
        "done path should exist"
    );
}
