//! End-to-end integration test for the filesystem-truth daemon.
//! Boots the HTTP server, hits the homepage and the internal MCP
//! endpoint, asserts the new core drives correct responses.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use tokio::sync::Mutex;
use trinity::server;

/// Tests that mutate `$HOME` (so the daemon's `~/.trinity/repos`
/// registry writes land in a tempdir rather than the user's real home)
/// must hold this mutex for the duration of the test. Otherwise they
/// race each other and one test's persist call lands in another test's
/// fake home. `tokio::sync::Mutex` because the guard is held across
/// `.await` points; `std::sync::Mutex` would trip `await_holding_lock`.
static HOME_LOCK: Mutex<()> = Mutex::const_new(());

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

fn plan_id_for(dir: &tempfile::TempDir, slug: &str) -> String {
    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    format!("{basename}/{slug}.md")
}

async fn spawn_daemon(repo_root: &Path) -> (String, tokio::task::JoinHandle<()>) {
    spawn_daemon_with_repos(&[repo_root.to_path_buf()]).await
}

async fn spawn_daemon_with_repos(
    repos: &[std::path::PathBuf],
) -> (String, tokio::task::JoinHandle<()>) {
    let repos_file = tempfile::NamedTempFile::new().unwrap();
    let path = repos_file.path().to_path_buf();
    drop(repos_file);
    spawn_daemon_with_repos_file(&path, repos).await
}

async fn spawn_daemon_with_repos_file(
    repos_path: &std::path::Path,
    repos: &[std::path::PathBuf],
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    if let Some(parent) = repos_path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let body = repos
        .iter()
        .map(|p| format!("{}\n", p.display()))
        .collect::<String>();
    std::fs::write(repos_path, body).unwrap();

    let lock_file = tempfile::NamedTempFile::new().unwrap();
    // Remove the empty file so the daemon writes its own PID into it
    // without "another daemon running" being triggered by leftover bytes.
    let lock_path = lock_file.path().to_string_lossy().into_owned();
    drop(lock_file);
    let _ = std::fs::remove_file(&lock_path);

    let args = server::ServeArgs {
        bind: addr,
        repos: repos_path.to_string_lossy().into_owned(),
        lock: lock_path,
        frontend_dist: std::path::PathBuf::from("frontend/dist"),
    };

    let url = format!("http://{}", addr);
    let handle = tokio::spawn(async move {
        let _ = server::serve(args).await;
    });

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if reqwest::get(format!("{}/healthz", url)).await.is_ok() {
            return (url, handle);
        }
    }
    panic!("daemon did not come up");
}

#[tokio::test]
async fn home_renders_plan_list() {
    // `/` serves the SPA shell; the homepage's data comes from
    // `/api/plans`.
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let body = reqwest::get(format!("{url}/api/plans"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    handle.abort();
    assert!(body.contains("\"foo\""), "plan list should include 'foo'");
    assert!(body.contains("\"planning\""), "phase should be 'planning'");
    assert!(
        body.contains("plan_needs_initial_review"),
        "should describe waiting on reviewers; got: {}",
        body
    );
}

#[tokio::test]
async fn plan_detail_renders() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let body = reqwest::get(format!("{url}/api/plan/{basename}/foo.md"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    handle.abort();
    assert!(body.contains("\"foo\""), "plan detail should include slug");
    assert!(
        body.contains("\"timeline\""),
        "plan detail should carry timeline"
    );
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
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
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
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
    });
    let resp = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap();
    handle.abort();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["result"]["error"], "plan_not_committed");
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
        "tool": "list_plans",
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
    let plans = body["result"]["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2);
}

#[tokio::test]
async fn pr_hint_present_in_implementing_phase() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo plan");
    write_file(dir.path(), "src/lib.rs", "fn main() {}\n");
    commit(dir.path(), "Implement foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
    });
    let resp = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap();
    handle.abort();
    let body: serde_json::Value = resp.json().await.unwrap();
    let pr_hint = &body["result"]["pr_hint"];
    assert!(
        pr_hint.is_object(),
        "pr_hint should be an object: {pr_hint}"
    );
    assert!(pr_hint["plan_intro"].is_string());
    assert!(pr_hint["plan_intro_parent"].is_null() || pr_hint["plan_intro_parent"].is_string());
    let options = pr_hint["options"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    let names: Vec<&str> = options
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"keep_plan_in_pr"));
    assert!(names.contains(&"exclude_plan_from_pr"));
}

#[tokio::test]
async fn plan_revision_route_renders_blob() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# plan body v1\n");
    commit(dir.path(), "Add foo plan");

    let (url, handle) = spawn_daemon(dir.path()).await;
    // Get the latest plan revision SHA via list_sessions / get_context.
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
    });
    let ctx: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sha = ctx["result"]["latest_plan_revision"]["commit_sha"]
        .as_str()
        .unwrap()
        .to_string();

    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let body = client
        .get(format!("{url}/api/plan/{basename}/foo.md/revision/{sha}"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    handle.abort();
    assert!(body.contains("plan body v1"), "got: {body}");
}

#[tokio::test]
async fn commit_diff_route_renders_patch() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo plan");
    write_file(dir.path(), "src/lib.rs", "fn marker_in_commit() {}\n");
    commit(dir.path(), "Implement foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
    });
    let ctx: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let impl_sha = ctx["result"]["latest_implementation_revision"]["commit_sha"]
        .as_str()
        .unwrap()
        .to_string();

    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let body = client
        .get(format!(
            "{url}/api/plan/{basename}/foo.md/commit/{impl_sha}"
        ))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    handle.abort();
    assert!(body.contains("marker_in_commit"), "got: {body}");
}

#[tokio::test]
async fn commit_diff_carries_subject_and_message_body_without_patch_leak() {
    // The plan asserts that `git_io::commit_message` uses
    // `git show -s --format=%s%x00%b` so the response's `message_body`
    // never contains the patch text. Regression test: commit with a
    // long message body, confirm subject + body parse separately and
    // body has no `diff --git` substring.
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo plan");
    write_file(dir.path(), "src/lib.rs", "fn impl_marker() {}\n");
    run_git(dir.path(), &["add", "-A"]);
    run_git(
        dir.path(),
        &[
            "commit",
            "--quiet",
            "-m",
            "Implement foo: subject line",
            "-m",
            "Extended body paragraph one.\n\nExtended body paragraph two.",
        ],
    );

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
    });
    let ctx: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let impl_sha = ctx["result"]["latest_implementation_revision"]["commit_sha"]
        .as_str()
        .unwrap()
        .to_string();

    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let body: serde_json::Value = client
        .get(format!(
            "{url}/api/plan/{basename}/foo.md/commit/{impl_sha}"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();

    assert_eq!(body["subject"], "Implement foo: subject line");
    let message_body = body["message_body"].as_str().unwrap();
    assert!(
        message_body.contains("Extended body paragraph one."),
        "message_body should carry the extended body; got: {message_body}"
    );
    assert!(
        !message_body.contains("diff --git"),
        "message_body must NOT contain patch text; got: {message_body}"
    );
}

#[tokio::test]
async fn plan_detail_carries_body_html_and_timeline_subject() {
    let dir = init_repo();
    write_file(
        dir.path(),
        ".trinity/plans/foo.md",
        "# Foo Plan\n\nFirst paragraph.\n",
    );
    run_git(dir.path(), &["add", "-A"]);
    run_git(
        dir.path(),
        &["commit", "--quiet", "-m", "Add foo plan with body"],
    );

    let (url, handle) = spawn_daemon(dir.path()).await;
    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let body: serde_json::Value = reqwest::get(format!("{url}/api/plan/{basename}/foo.md"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();

    let body_html = body["plan_body_html"].as_str().unwrap();
    assert!(body_html.contains("<h1>Foo Plan</h1>"), "got: {body_html}");
    assert!(body_html.contains("First paragraph."), "got: {body_html}");
    assert!(body["plan_body_truncated"].is_boolean());

    let first = &body["timeline"][0];
    assert_eq!(first["kind"], "commit_plan");
    assert_eq!(first["subject"], "Add foo plan with body");
}

#[tokio::test]
async fn api_plans_sorts_across_repos_by_last_activity_ts() {
    // Regression for the cross-repo sort bug: plans_index sorts within
    // a single repo's response, but api_plans extends across repos and
    // must re-sort the combined list.
    //
    // Iteration order under `Trinity.repos: BTreeMap<PathBuf, _>` is
    // lex-by-canonical-path. To make this test fail deterministically
    // without the cross-repo sort, **pin the dir names** so the
    // alphabetically-first repo also happens to have the older commit.
    // Without the sort, that older plan would come first in the
    // response; with the sort, the newer (beta) plan wins.
    let parent = tempfile::tempdir().unwrap();
    let dir_a = parent.path().join("a_alpha");
    let dir_b = parent.path().join("z_beta");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    for d in [&dir_a, &dir_b] {
        run_git(d, &["init", "--quiet", "--initial-branch=main"]);
        run_git(d, &["config", "user.email", "test@test"]);
        run_git(d, &["config", "user.name", "test"]);
        run_git(d, &["config", "commit.gpgsign", "false"]);
    }
    write_file(&dir_a, ".trinity/plans/in-alpha.md", "# alpha\n");
    commit(&dir_a, "alpha intro");
    // Wait so beta's commit is strictly newer.
    std::thread::sleep(Duration::from_secs(1));
    write_file(&dir_b, ".trinity/plans/in-beta.md", "# beta\n");
    commit(&dir_b, "beta intro");

    let (url, handle) = spawn_daemon_with_repos(&[dir_a.clone(), dir_b.clone()]).await;
    let body: serde_json::Value = reqwest::get(format!("{url}/api/plans"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    let plans = body["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2);
    // beta committed second → its plan should appear first globally,
    // regardless of which repo `Trinity.repos` iterated first.
    assert_eq!(
        plans[0]["slug"], "in-beta",
        "cross-repo sort must put the newest plan first; got: {plans:?}"
    );
    assert_eq!(plans[1]["slug"], "in-alpha");
    assert!(plans[0]["last_activity_ts"].as_i64() >= plans[1]["last_activity_ts"].as_i64());
}

#[tokio::test]
async fn last_activity_ts_handles_off_first_parent_plan_intro() {
    // Plan introduced on a feature branch and merged with --no-ff:
    // plan_intro is off the first-parent chain so it's not in
    // `commit_meta` from the batched `git log --first-parent`. The
    // snapshot layer must backfill its author_ts so last_activity_ts
    // doesn't sink to 0 (which would bury the plan at the bottom of
    // /api/plans).
    let dir = init_repo();
    // Initial commit on main so we have a base.
    write_file(dir.path(), "README.md", "base\n");
    commit(dir.path(), "init");

    // Branch, add plan, switch back, merge with --no-ff.
    run_git(dir.path(), &["checkout", "-q", "-b", "feature"]);
    write_file(dir.path(), ".trinity/plans/branchy.md", "# branchy\n");
    commit(dir.path(), "Add branchy on feature");
    run_git(dir.path(), &["checkout", "-q", "main"]);
    run_git(
        dir.path(),
        &["merge", "--no-ff", "-q", "-m", "Merge feature", "feature"],
    );

    let (url, handle) = spawn_daemon(dir.path()).await;
    let body: serde_json::Value = reqwest::get(format!("{url}/api/plans"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    let plans = body["plans"].as_array().unwrap();
    let branchy = plans
        .iter()
        .find(|p| p["slug"] == "branchy")
        .expect("branchy plan in response");
    let ts = branchy["last_activity_ts"].as_i64().unwrap();
    assert!(
        ts > 0,
        "last_activity_ts must be backfilled for off-first-parent plan_intro; got {ts}"
    );
}

#[tokio::test]
async fn api_plans_carries_last_activity_ts_and_sorts_desc() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/old.md", "# old\n");
    commit(dir.path(), "Add old");
    // Sleep so the newer commit's author_ts is strictly larger.
    std::thread::sleep(Duration::from_secs(1));
    write_file(dir.path(), ".trinity/plans/new.md", "# new\n");
    commit(dir.path(), "Add new");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let body: serde_json::Value = reqwest::get(format!("{url}/api/plans"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    let plans = body["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2);
    assert!(plans[0]["last_activity_ts"].is_number());
    let first_ts = plans[0]["last_activity_ts"].as_i64().unwrap();
    let second_ts = plans[1]["last_activity_ts"].as_i64().unwrap();
    assert!(
        first_ts >= second_ts,
        "plans should be sorted by last_activity_ts desc; got {first_ts} then {second_ts}"
    );
    assert_eq!(plans[0]["slug"], "new", "newest plan should be first");
}

#[tokio::test]
async fn sse_pushes_repo_rebuilt_on_head_change() {
    use futures::StreamExt;

    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    // Connect to the SSE stream and let the subscription settle.
    let resp = reqwest::get(format!("{}/events", url)).await.unwrap();
    let mut stream = resp.bytes_stream();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Trigger a HEAD change by committing again.
    write_file(dir.path(), "src/lib.rs", "fn x() {}\n");
    commit(dir.path(), "Add lib");

    // The fs watcher (notify-debouncer-full, 150ms window) picks up
    // .git/logs/HEAD and emits a `repo_rebuilt` SSE event.
    let mut payload: Option<serde_json::Value> = None;
    let timeout = tokio::time::sleep(Duration::from_secs(10));
    tokio::pin!(timeout);
    let mut buf = String::new();
    loop {
        tokio::select! {
            _ = &mut timeout => break,
            chunk = stream.next() => {
                let Some(Ok(bytes)) = chunk else { continue; };
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(end) = buf.find("\n\n") {
                    let event = buf[..end].to_string();
                    buf.drain(..end + 2);
                    for line in event.lines() {
                        if let Some(data) = line.strip_prefix("data: ")
                            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data)
                            && parsed["kind"] == "repo_rebuilt"
                        {
                            payload = Some(parsed);
                        }
                    }
                    if payload.is_some() {
                        break;
                    }
                }
                if payload.is_some() {
                    break;
                }
            }
        }
    }
    handle.abort();
    let payload = payload.expect("expected `repo_rebuilt` SSE event after HEAD change");
    assert_eq!(payload["kind"], "repo_rebuilt");
    assert!(
        payload["ts"].is_number(),
        "ts should be a unix-seconds number"
    );
    assert!(payload["repo"].is_string(), "repo should be a string");
    assert!(
        payload["plan_id"].is_null(),
        "repo-level events carry plan_id: null; got {payload}"
    );
    assert!(
        payload["state"].is_null(),
        "repo-level events carry state: null; got {payload}"
    );
}

#[tokio::test]
async fn start_plan_persists_repo_to_registry() {
    // Override $HOME so we don't pollute the user's real ~/.trinity.
    let _home_guard = HOME_LOCK.lock().await;
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    // SAFETY: serialized with other HOME-mutating tests via HOME_LOCK.
    unsafe { std::env::set_var("HOME", fake_home.path()) };

    let dir = init_repo();
    // Use the default registry path the daemon resolves from ~/.trinity/repos.
    let registry_path = fake_home.path().join(".trinity/repos");
    let (url, handle) = spawn_daemon_with_repos_file(&registry_path, &[]).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "start_plan",
        "arguments": { "slug": "foo", "label": "test-agent" }
    });
    let resp = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap();
    handle.abort();
    assert!(
        resp.status().is_success(),
        "start_plan failed: {}",
        resp.status()
    );

    // ~/.trinity/repos should now contain the repo path.
    let registry_path = fake_home.path().join(".trinity/repos");
    assert!(registry_path.exists(), "registry file should be created");
    let body = std::fs::read_to_string(&registry_path).unwrap();
    assert!(
        body.lines()
            .any(|l| std::path::Path::new(l.trim()) == dir.path()
                || std::path::Path::new(l.trim()) == dir.path().canonicalize().unwrap()),
        "registry should contain the repo, got: {body}"
    );

    // Restore $HOME before dropping the lock — another HOME-mutating
    // test may acquire next and we don't want it to observe our fake
    // value.
    match prev_home {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[tokio::test]
async fn branch_switch_rebuilds_repo_state() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();

    // Branch off from main, add a NEW plan there
    run_git(dir.path(), &["checkout", "-q", "-b", "feat"]);
    write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
    commit(dir.path(), "Add bar plan on feat branch");
    // Give the watcher time to fire HeadChanged + rebuild.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // list_plans should now show both foo and bar (we're on feat).
    let req = json!({"cwd": dir.path(), "tool": "list_plans", "arguments": {}});
    let v: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slugs: Vec<&str> = v["result"]["plans"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["slug"].as_str())
        .collect();
    assert!(slugs.contains(&"foo"), "got: {:?}", slugs);
    assert!(slugs.contains(&"bar"), "got: {:?}", slugs);

    // Switch back to main. bar's plan file doesn't exist there.
    run_git(dir.path(), &["checkout", "-q", "main"]);
    tokio::time::sleep(Duration::from_millis(500)).await;

    let v: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slugs: Vec<&str> = v["result"]["plans"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["slug"].as_str())
        .collect();
    handle.abort();
    assert!(slugs.contains(&"foo"));
    assert!(
        !slugs.contains(&"bar"),
        "bar should be gone after switching to main; got: {:?}",
        slugs
    );
}

#[tokio::test]
async fn feedback_renders_on_session_page() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();

    // Find the plan_intro sha via get_context.
    let req = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
    });
    let ctx: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let intro = ctx["result"]["latest_plan_revision"]["commit_sha"]
        .as_str()
        .unwrap()
        .to_string();

    // Drop a feedback file at the canonical path.
    let feedback_rel = format!(".trinity/feedback/foo/plan/{}/alice.md", intro);
    write_file(dir.path(), &feedback_rel, "APPROVE\n\nlgtm\n");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // First check via get_context that the feedback is in state.
    let req2 = json!({
        "cwd": dir.path(),
        "tool": "get_context",
        "arguments": { "plan_id": plan_id_for(&dir, "foo") }
    });
    let ctx2: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&req2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pf = ctx2["result"]["plan_feedback"].as_array().unwrap();
    assert!(
        !pf.is_empty(),
        "plan_feedback should be populated; ctx: {ctx2}"
    );
    // Then check that the UI plan endpoint exposes the feedback
    // including the rendered body HTML.
    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let body = client
        .get(format!("{url}/api/plan/{basename}/foo.md"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    handle.abort();
    assert!(
        body.contains("\"alice\"") && body.contains("\"approve\""),
        "plan detail should include alice's APPROVE; got: {body}"
    );
    assert!(
        body.contains("\"body_html\""),
        "plan detail should carry rendered feedback HTML; got: {body}"
    );
}

#[tokio::test]
async fn done_move_endpoint_moves_plan_file() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let resp = client
        .post(format!("{url}/api/plan/{basename}/foo.md/done"))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let response_body: serde_json::Value = resp.json().await.unwrap();
    handle.abort();
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(response_body["ok"], true);
    assert_eq!(
        response_body["new_plan_path"], ".trinity/plans/done/foo.md",
        "response should advertise the new path"
    );
    assert!(
        !dir.path().join(".trinity/plans/foo.md").exists(),
        "active path should be gone"
    );
    assert!(
        dir.path().join(".trinity/plans/done/foo.md").exists(),
        "done path should exist"
    );
}

#[tokio::test]
async fn plan_id_url_stable_across_done_flip() {
    // The headline plan-path-identity invariant: same plan_id resolves
    // before and after the active↔done move, and the response shape
    // flips `state` and `current_path` to track the file's new home.
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let basename = dir.path().file_name().unwrap().to_str().unwrap();

    let before: serde_json::Value = client
        .get(format!("{url}/api/plan/{basename}/foo.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["plan_id"], format!("{basename}/foo.md"));
    assert_eq!(before["state"], "active");
    assert_eq!(before["current_path"], ".trinity/plans/foo.md");

    // Move + commit the move so the watcher rebuilds against a real
    // post-move HEAD.
    let active = dir.path().join(".trinity/plans/foo.md");
    let done_dir = dir.path().join(".trinity/plans/done");
    std::fs::create_dir_all(&done_dir).unwrap();
    std::fs::rename(&active, done_dir.join("foo.md")).unwrap();
    commit(dir.path(), "Move foo to done");
    tokio::time::sleep(Duration::from_millis(800)).await;

    let after: serde_json::Value = client
        .get(format!("{url}/api/plan/{basename}/foo.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();

    assert_eq!(after["plan_id"], format!("{basename}/foo.md"));
    assert_eq!(
        after["state"], "done",
        "state should flip to done; full response: {after}"
    );
    assert_eq!(
        after["current_path"], ".trinity/plans/done/foo.md",
        "current_path should point at done variant"
    );
}

#[tokio::test]
async fn same_stem_different_repos_distinguishable() {
    // Two repos with the same plan stem must resolve to different
    // plan_ids via their basenames, and wait_for_work / list_plans must
    // route each call to the correct repo. This is the second headline
    // invariant of the plan-path-identity refactor.
    let repo_a_parent = tempfile::tempdir().unwrap();
    let repo_b_parent = tempfile::tempdir().unwrap();
    let dir_a = repo_a_parent.path().join("alpha");
    let dir_b = repo_b_parent.path().join("beta");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    for d in [&dir_a, &dir_b] {
        run_git(d, &["init", "--quiet", "--initial-branch=main"]);
        run_git(d, &["config", "user.email", "test@test"]);
        run_git(d, &["config", "user.name", "test"]);
        run_git(d, &["config", "commit.gpgsign", "false"]);
    }
    write_file(&dir_a, ".trinity/plans/shared.md", "# in alpha\n");
    commit(&dir_a, "alpha shared");
    write_file(&dir_b, ".trinity/plans/shared.md", "# in beta\n");
    commit(&dir_b, "beta shared");

    let (url, handle) = spawn_daemon_with_repos(&[dir_a.clone(), dir_b.clone()]).await;
    let plans: serde_json::Value = reqwest::get(format!("{url}/api/plans"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let plan_ids: Vec<&str> = plans["plans"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["plan_id"].as_str())
        .collect();
    assert!(
        plan_ids.contains(&"alpha/shared.md") && plan_ids.contains(&"beta/shared.md"),
        "both basenames should resolve their own copy of the stem; got: {plan_ids:?}"
    );

    let client = reqwest::Client::new();
    let alpha_resp: serde_json::Value = client
        .get(format!("{url}/api/plan/alpha/shared.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let beta_resp: serde_json::Value = client
        .get(format!("{url}/api/plan/beta/shared.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    let alpha_canonical = dir_a.canonicalize().unwrap().to_string_lossy().into_owned();
    let beta_canonical = dir_b.canonicalize().unwrap().to_string_lossy().into_owned();
    assert_eq!(alpha_resp["repo"].as_str().unwrap(), alpha_canonical);
    assert_eq!(beta_resp["repo"].as_str().unwrap(), beta_canonical);
    assert_ne!(alpha_resp["repo"], beta_resp["repo"]);
}

#[tokio::test]
async fn start_plan_rejects_repo_basename_collision() {
    // Two distinct canonical repos with the same `file_name`: the second
    // one's `start_plan` must fail with a basename-collision error
    // (`repo_basename_taken`) before any disk mutation.
    let _home_guard = HOME_LOCK.lock().await;
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    // SAFETY: serialized with other HOME-mutating tests via HOME_LOCK.
    unsafe { std::env::set_var("HOME", fake_home.path()) };

    let parent_a = tempfile::tempdir().unwrap();
    let parent_b = tempfile::tempdir().unwrap();
    let dir_a = parent_a.path().join("collide");
    let dir_b = parent_b.path().join("collide");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    for d in [&dir_a, &dir_b] {
        run_git(d, &["init", "--quiet", "--initial-branch=main"]);
        run_git(d, &["config", "user.email", "test@test"]);
        run_git(d, &["config", "user.name", "test"]);
        run_git(d, &["config", "commit.gpgsign", "false"]);
    }

    let (url, handle) = spawn_daemon_with_repos(&[]).await;
    let client = reqwest::Client::new();

    let req_a = json!({
        "cwd": dir_a.clone(),
        "tool": "start_plan",
        "arguments": { "slug": "first", "label": "agent" }
    });
    let resp_a = client
        .post(format!("{url}/internal/tool_call"))
        .json(&req_a)
        .send()
        .await
        .unwrap();
    assert!(
        resp_a.status().is_success(),
        "first start_plan should succeed"
    );

    let req_b = json!({
        "cwd": dir_b.clone(),
        "tool": "start_plan",
        "arguments": { "slug": "second", "label": "agent" }
    });
    let resp_b = client
        .post(format!("{url}/internal/tool_call"))
        .json(&req_b)
        .send()
        .await
        .unwrap();
    let status_b = resp_b.status();
    let body_b = resp_b.text().await.unwrap();
    handle.abort();

    assert_eq!(
        status_b,
        reqwest::StatusCode::FORBIDDEN,
        "collision should be a 403; got status={status_b} body={body_b}"
    );
    assert!(
        body_b.contains("basename") || body_b.contains("collide"),
        "error should mention basename collision; got: {body_b}"
    );
    // The second repo must NOT have leaked partial state to its
    // .gitignore — we rejected before any disk mutation.
    assert!(
        !dir_b.join(".gitignore").exists(),
        ".gitignore must not be written when start_plan rejects on collision"
    );
    // Same for the user-level registry: dir_b must not be persisted at
    // all (the first repo's path is fine, that's the whole point).
    let registry =
        std::fs::read_to_string(fake_home.path().join(".trinity/repos")).unwrap_or_default();
    let dir_b_str = dir_b.to_string_lossy();
    assert!(
        !registry.lines().any(|l| l.trim() == dir_b_str),
        "registry must not record dir_b on collision; registry: {registry}"
    );

    // Restore HOME before dropping the lock — another HOME-mutating test
    // may acquire next and we don't want it to observe our fake value.
    match prev_home {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[tokio::test]
async fn start_plan_concurrent_basename_twins_loser_does_no_disk_mutation() {
    // Round 3 regression: two `start_plan`s from basename-twin repos
    // fired concurrently must result in exactly one repo with a written
    // `.gitignore` + `~/.trinity/repos` entry — the loser must not leak
    // disk mutations between the precheck and the atomic register.
    let _home_guard = HOME_LOCK.lock().await;
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    // SAFETY: serialized with other HOME-mutating tests via HOME_LOCK.
    unsafe { std::env::set_var("HOME", fake_home.path()) };

    let parent_a = tempfile::tempdir().unwrap();
    let parent_b = tempfile::tempdir().unwrap();
    let dir_a = parent_a.path().join("twin");
    let dir_b = parent_b.path().join("twin");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    for d in [&dir_a, &dir_b] {
        run_git(d, &["init", "--quiet", "--initial-branch=main"]);
        run_git(d, &["config", "user.email", "test@test"]);
        run_git(d, &["config", "user.name", "test"]);
        run_git(d, &["config", "commit.gpgsign", "false"]);
    }

    let (url, handle) = spawn_daemon_with_repos(&[]).await;
    let client = reqwest::Client::new();
    let url_a = url.clone();
    let url_b = url.clone();
    let client_a = client.clone();
    let client_b = client.clone();
    let dir_a_clone = dir_a.clone();
    let dir_b_clone = dir_b.clone();

    let (resp_a, resp_b) = tokio::join!(
        async move {
            let req = json!({
                "cwd": dir_a_clone,
                "tool": "start_plan",
                "arguments": { "slug": "first", "label": "agent" }
            });
            client_a
                .post(format!("{url_a}/internal/tool_call"))
                .json(&req)
                .send()
                .await
                .unwrap()
        },
        async move {
            let req = json!({
                "cwd": dir_b_clone,
                "tool": "start_plan",
                "arguments": { "slug": "second", "label": "agent" }
            });
            client_b
                .post(format!("{url_b}/internal/tool_call"))
                .json(&req)
                .send()
                .await
                .unwrap()
        }
    );
    let status_a = resp_a.status();
    let status_b = resp_b.status();
    handle.abort();

    let (winner_dir, loser_dir) = match (status_a.is_success(), status_b.is_success()) {
        (true, false) => (&dir_a, &dir_b),
        (false, true) => (&dir_b, &dir_a),
        other => panic!(
            "exactly one twin should succeed; got status_a={status_a} status_b={status_b}, ok={other:?}"
        ),
    };

    assert!(
        winner_dir.join(".gitignore").exists(),
        "winning twin should have .gitignore written"
    );
    assert!(
        !loser_dir.join(".gitignore").exists(),
        "loser twin must not have .gitignore written (TOCTOU leak)"
    );
    let registry =
        std::fs::read_to_string(fake_home.path().join(".trinity/repos")).unwrap_or_default();
    let loser_str = loser_dir.to_string_lossy();
    assert!(
        !registry.lines().any(|l| l.trim() == loser_str),
        "registry must not record the loser twin; registry: {registry}"
    );

    match prev_home {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[tokio::test]
async fn api_repos_lists_watched_with_basename_and_plan_count() {
    let repo_a_parent = tempfile::tempdir().unwrap();
    let repo_b_parent = tempfile::tempdir().unwrap();
    let dir_a = repo_a_parent.path().join("alpha");
    let dir_b = repo_b_parent.path().join("beta");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    for d in [&dir_a, &dir_b] {
        run_git(d, &["init", "--quiet", "--initial-branch=main"]);
        run_git(d, &["config", "user.email", "test@test"]);
        run_git(d, &["config", "user.name", "test"]);
        run_git(d, &["config", "commit.gpgsign", "false"]);
    }
    write_file(&dir_a, ".trinity/plans/p1.md", "# alpha plan\n");
    commit(&dir_a, "alpha plan");
    write_file(&dir_b, ".trinity/plans/q1.md", "# beta plan one\n");
    commit(&dir_b, "beta one");
    write_file(&dir_b, ".trinity/plans/q2.md", "# beta plan two\n");
    commit(&dir_b, "beta two");

    let (url, handle) = spawn_daemon_with_repos(&[dir_a.clone(), dir_b.clone()]).await;
    let body: serde_json::Value = reqwest::get(format!("{url}/api/repos"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();

    let repos = body["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 2);
    let by_basename: std::collections::HashMap<&str, &serde_json::Value> = repos
        .iter()
        .filter_map(|r| r["basename"].as_str().map(|b| (b, r)))
        .collect();
    assert_eq!(by_basename["alpha"]["plan_count"], 1);
    assert_eq!(by_basename["beta"]["plan_count"], 2);
    assert!(by_basename["alpha"]["last_activity_ts"].is_number());
    assert!(by_basename["beta"]["last_activity_ts"].is_number());
}

#[tokio::test]
async fn delete_repo_removes_from_state_and_registry() {
    let _home_guard = HOME_LOCK.lock().await;
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    // SAFETY: serialized with other HOME-mutating tests via HOME_LOCK.
    unsafe { std::env::set_var("HOME", fake_home.path()) };

    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("orphan");
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&dir, &["config", "user.email", "test@test"]);
    run_git(&dir, &["config", "user.name", "test"]);
    run_git(&dir, &["config", "commit.gpgsign", "false"]);

    let (url, handle) = spawn_daemon_with_repos(&[]).await;
    let client = reqwest::Client::new();

    // Register via start_plan, then delete.
    let req = json!({
        "cwd": dir,
        "tool": "start_plan",
        "arguments": { "slug": "p", "label": "agent" }
    });
    let resp = client
        .post(format!("{url}/internal/tool_call"))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "start_plan should succeed");

    // Now /api/repos should include `orphan`.
    let before: serde_json::Value = reqwest::get(format!("{url}/api/repos"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        before["repos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["basename"] == "orphan"),
        "orphan should be listed"
    );

    let resp = client
        .delete(format!("{url}/api/repos/orphan"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    let after: serde_json::Value = reqwest::get(format!("{url}/api/repos"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    assert!(
        !after["repos"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["basename"] == "orphan"),
        "orphan should be gone after DELETE; got: {after}"
    );

    // Registry file should not contain the path.
    let registry =
        std::fs::read_to_string(fake_home.path().join(".trinity/repos")).unwrap_or_default();
    let dir_str = dir.to_string_lossy();
    assert!(
        !registry.lines().any(|l| l.trim() == dir_str),
        "registry must not contain unwatched repo; registry: {registry}"
    );

    match prev_home {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[tokio::test]
async fn delete_repo_surfaces_registry_write_error() {
    // Wire-shape regression: when the registry rewrite fails after a
    // successful in-memory deregistration, the response must carry
    // `ok: false` and `registry_write_error: Some(_)` so the frontend
    // can show a warning to the operator. Without this, the repo is
    // gone from `Trinity.repos` but lingers in the file, and resurrects
    // on next daemon restart with no signal.
    //
    // Trigger the write failure by chmod'ing the registry's parent
    // directory read-only AFTER `start_plan` has already populated it.
    // `remove_repo_from_registry` writes a `.repos.tmp` sibling and
    // then renames; the tmp-write hits EACCES on a read-only dir.
    use std::os::unix::fs::PermissionsExt;

    let _home_guard = HOME_LOCK.lock().await;
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    // SAFETY: serialized via HOME_LOCK with the other HOME-mutating tests.
    unsafe { std::env::set_var("HOME", fake_home.path()) };

    let registry_parent = tempfile::tempdir().unwrap();
    let registry_path = registry_parent.path().join("repos");

    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("locked");
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&dir, &["config", "user.email", "test@test"]);
    run_git(&dir, &["config", "user.name", "test"]);
    run_git(&dir, &["config", "commit.gpgsign", "false"]);

    let (url, handle) = spawn_daemon_with_repos_file(&registry_path, &[]).await;
    let client = reqwest::Client::new();

    // Register first (writable parent).
    let req = json!({
        "cwd": dir,
        "tool": "start_plan",
        "arguments": { "slug": "p", "label": "agent" }
    });
    let resp = client
        .post(format!("{url}/internal/tool_call"))
        .json(&req)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "start_plan should succeed before chmod"
    );

    // Lock the registry's parent dir so the tmp-write inside
    // remove_repo_from_registry fails with EACCES.
    let mut perms = std::fs::metadata(registry_parent.path())
        .unwrap()
        .permissions();
    let original_mode = perms.mode();
    perms.set_mode(0o555);
    std::fs::set_permissions(registry_parent.path(), perms).unwrap();

    let resp = client
        .delete(format!("{url}/api/repos/locked"))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();

    // Restore so tempdir cleanup can drop the path on Drop.
    let mut restore = std::fs::metadata(registry_parent.path())
        .unwrap()
        .permissions();
    restore.set_mode(original_mode);
    std::fs::set_permissions(registry_parent.path(), restore).unwrap();
    handle.abort();

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["ok"], false,
        "registry write failure should flip ok to false; got: {body}"
    );
    let warning = body["registry_write_error"]
        .as_str()
        .unwrap_or_else(|| panic!("registry_write_error missing or not a string; body: {body}"));
    assert!(
        warning.contains(&*registry_path.to_string_lossy())
            || warning.to_lowercase().contains("registry"),
        "warning should mention the registry; got: {warning}"
    );

    match prev_home {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[tokio::test]
async fn delete_repo_404_on_unknown_basename() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "add foo");
    let (url, handle) = spawn_daemon(dir.path()).await;
    let resp = reqwest::Client::new()
        .delete(format!("{url}/api/repos/does-not-exist"))
        .send()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn registry_path_threads_through_start_plan_and_delete() {
    // Use a non-default --repos path. Both start_plan (persist) and the
    // new DELETE handler must write to THIS path, not ~/.trinity/repos.
    let _home_guard = HOME_LOCK.lock().await;
    let fake_home = tempfile::tempdir().unwrap();
    let prev_home = std::env::var_os("HOME");
    // SAFETY: serialized with other HOME-mutating tests via HOME_LOCK.
    unsafe { std::env::set_var("HOME", fake_home.path()) };

    let registry_dir = tempfile::tempdir().unwrap();
    let registry_path = registry_dir.path().join("custom-registry");

    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("custom");
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&dir, &["config", "user.email", "test@test"]);
    run_git(&dir, &["config", "user.name", "test"]);
    run_git(&dir, &["config", "commit.gpgsign", "false"]);

    let (url, handle) = spawn_daemon_with_repos_file(&registry_path, &[]).await;
    let client = reqwest::Client::new();

    let req = json!({
        "cwd": dir,
        "tool": "start_plan",
        "arguments": { "slug": "p", "label": "agent" }
    });
    client
        .post(format!("{url}/internal/tool_call"))
        .json(&req)
        .send()
        .await
        .unwrap();

    // Custom registry has the path, default location does NOT.
    let custom_body = std::fs::read_to_string(&registry_path).unwrap_or_default();
    let default_body =
        std::fs::read_to_string(fake_home.path().join(".trinity/repos")).unwrap_or_default();
    // start_plan persists whatever `git rev-parse --show-toplevel`
    // returned, which on macOS is the canonical /private/var/... form.
    let dir_canonical = dir.canonicalize().unwrap().to_string_lossy().to_string();
    assert!(
        custom_body.lines().any(|l| l.trim() == dir_canonical),
        "custom registry should contain repo after start_plan; \
         dir_canonical={dir_canonical}; got: {custom_body}"
    );
    assert!(
        !default_body.lines().any(|l| l.trim() == dir_canonical),
        "default registry must NOT be touched when --repos is custom; got: {default_body}"
    );

    // DELETE: should clean from custom registry.
    client
        .delete(format!("{url}/api/repos/custom"))
        .send()
        .await
        .unwrap();
    handle.abort();
    let after = std::fs::read_to_string(&registry_path).unwrap_or_default();
    assert!(
        !after.lines().any(|l| l.trim() == dir_canonical),
        "DELETE should remove from custom registry; got: {after}"
    );

    match prev_home {
        Some(v) => unsafe { std::env::set_var("HOME", v) },
        None => unsafe { std::env::remove_var("HOME") },
    }
}

#[tokio::test]
async fn api_plans_filter_by_basename() {
    let repo_a_parent = tempfile::tempdir().unwrap();
    let repo_b_parent = tempfile::tempdir().unwrap();
    let dir_a = repo_a_parent.path().join("alpha");
    let dir_b = repo_b_parent.path().join("beta");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    for d in [&dir_a, &dir_b] {
        run_git(d, &["init", "--quiet", "--initial-branch=main"]);
        run_git(d, &["config", "user.email", "test@test"]);
        run_git(d, &["config", "user.name", "test"]);
        run_git(d, &["config", "commit.gpgsign", "false"]);
    }
    write_file(&dir_a, ".trinity/plans/in-alpha.md", "# alpha plan\n");
    commit(&dir_a, "alpha plan");
    write_file(&dir_b, ".trinity/plans/in-beta.md", "# beta plan\n");
    commit(&dir_b, "beta plan");

    let (url, handle) = spawn_daemon_with_repos(&[dir_a.clone(), dir_b.clone()]).await;

    let alpha_only: serde_json::Value = reqwest::get(format!("{url}/api/plans?repo=alpha"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slugs: Vec<&str> = alpha_only["plans"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["slug"].as_str())
        .collect();
    assert_eq!(
        slugs,
        vec!["in-alpha"],
        "?repo=alpha should narrow to alpha"
    );

    let unknown = reqwest::get(format!("{url}/api/plans?repo=does-not-exist"))
        .await
        .unwrap();
    let unknown_status = unknown.status();
    assert_eq!(
        unknown_status,
        reqwest::StatusCode::NOT_FOUND,
        "unknown ?repo filter must 404 to match MCP error semantics"
    );

    // Path filter with `..` traversal: the route must canonicalize before
    // matching against `Trinity.repos` (keyed on canonical paths). Without
    // `dunce::canonicalize` in `repos_to_render`, this 404s even though
    // the absolute path resolves to a watched repo.
    let dir_b_canonical = dir_b.canonicalize().unwrap();
    let beta_basename = dir_b_canonical.file_name().unwrap().to_string_lossy();
    let parent_canonical = dir_b_canonical.parent().unwrap();
    let parent_basename = parent_canonical.file_name().unwrap().to_string_lossy();
    let parent_of_parent = parent_canonical.parent().unwrap().display();
    let traversal_path =
        format!("{parent_of_parent}/{parent_basename}/../{parent_basename}/{beta_basename}");
    let resp = reqwest::Client::new()
        .get(format!("{url}/api/plans"))
        .query(&[("repo", traversal_path.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "path filter with `..` must canonicalize before lookup; full path: {traversal_path}"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    let slugs: Vec<&str> = body["plans"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["slug"].as_str())
        .collect();
    handle.abort();
    assert_eq!(
        slugs,
        vec!["in-beta"],
        "traversal path should resolve to beta"
    );
}
