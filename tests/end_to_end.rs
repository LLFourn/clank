//! End-to-end integration test for the filesystem-truth daemon.
//! Boots the HTTP server, hits the homepage and the internal MCP
//! endpoint, asserts the new core drives correct responses.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use trinity::server;
use trinity_core::api::PlanDetailResponse;

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

/// Fetch the HTTP `/api/plan/<basename>/<slug>.md` response as the
/// typed `PlanDetailResponse`. Useful in tests that need fields the
/// MCP `work_context` coordination view intentionally drops
/// (`pr_hint`, `latest_plan_revision`,
/// `latest_implementation_revision`, `commits[]`, etc.).
async fn fetch_plan_detail(
    client: &reqwest::Client,
    url: &str,
    dir: &tempfile::TempDir,
    slug: &str,
) -> PlanDetailResponse {
    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    client
        .get(format!("{url}/api/plan/{basename}/{slug}.md"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
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
        frontend_dist: None,
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
        body.contains("commit_needs_review"),
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
async fn mcp_work_context_via_internal_tool_call() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "work_context",
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
    assert_eq!(result["waiting_on"]["reason"], "commit_needs_review");
}

#[tokio::test]
async fn mcp_work_context_for_uncommitted_returns_plan_not_committed() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    // Don't commit.

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let req = json!({
        "cwd": dir.path(),
        "tool": "work_context",
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
    let body = fetch_plan_detail(&client, &url, &dir, "foo").await;
    handle.abort();
    let pr_hint = body.pr_hint.expect("pr_hint should be present");
    assert!(!pr_hint.plan_intro.is_empty());
    let kinds: Vec<_> = pr_hint.options.iter().map(|o| o.kind).collect();
    assert_eq!(kinds.len(), 2);
    use trinity_core::vocab::PrHintOptionKind::*;
    assert!(kinds.contains(&KeepPlanInPr));
    assert!(kinds.contains(&ExcludePlanFromPr));
}

#[tokio::test]
async fn plan_revision_route_renders_blob() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# plan body v1\n");
    commit(dir.path(), "Add foo plan");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();
    let plan = fetch_plan_detail(&client, &url, &dir, "foo").await;
    let sha = plan
        .latest_plan_revision
        .expect("plan should have an intro revision")
        .commit_sha;

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
    let plan = fetch_plan_detail(&client, &url, &dir, "foo").await;
    let impl_sha = plan
        .latest_implementation_revision
        .expect("plan should have an implementation commit")
        .commit_sha;

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
    let plan = fetch_plan_detail(&client, &url, &dir, "foo").await;
    let impl_sha = plan
        .latest_implementation_revision
        .expect("plan should have an implementation commit")
        .commit_sha;

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
async fn plan_detail_carries_body_and_timeline_subject() {
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

    // Phase 3 of wasm-markdown-rendering: the wire ships raw
    // markdown; the wasm frontend renders. `plan_body_html` /
    // `plan_body_truncated` are gone.
    let plan_body = body["plan_body"].as_str().unwrap();
    assert!(plan_body.contains("# Foo Plan"), "got: {plan_body}");
    assert!(plan_body.contains("First paragraph."), "got: {plan_body}");
    assert!(
        body["plan_body_html"].is_null(),
        "plan_body_html must NOT cross the wire: {body}"
    );
    assert!(
        body["plan_body_truncated"].is_null(),
        "plan_body_truncated must NOT cross the wire: {body}"
    );

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
    assert_eq!(
        payload["scope"], "repo",
        "repo-level events carry scope: repo; got {payload}"
    );
    assert!(
        payload["ts"].is_number(),
        "ts should be a unix-seconds number"
    );
    assert!(
        payload["plan_id"].is_null(),
        "repo-level events have no plan_id; got {payload}"
    );
}

#[tokio::test]
async fn start_plan_persists_repo_to_registry() {
    let registry_parent = tempfile::tempdir().unwrap();
    let registry_path = registry_parent.path().join("repos");

    let dir = init_repo();
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

    assert!(registry_path.exists(), "registry file should be created");
    let body = std::fs::read_to_string(&registry_path).unwrap();
    assert!(
        body.lines()
            .any(|l| std::path::Path::new(l.trim()) == dir.path()
                || std::path::Path::new(l.trim()) == dir.path().canonicalize().unwrap()),
        "registry should contain the repo, got: {body}"
    );
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

    let plan = fetch_plan_detail(&client, &url, &dir, "foo").await;
    let intro = plan
        .latest_plan_revision
        .as_ref()
        .expect("plan should have an intro revision")
        .commit_sha
        .clone();

    // Drop a feedback file at the canonical path.
    let feedback_rel = format!(".trinity/feedback/foo/{}/alice.md", intro);
    write_file(dir.path(), &feedback_rel, "APPROVE\n\nlgtm\n");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let plan2 = fetch_plan_detail(&client, &url, &dir, "foo").await;
    let has_alice_approve = plan2.commits.iter().any(|c| {
        c.feedback.iter().any(|f| {
            f.author.as_str() == "alice"
                && matches!(f.verdict, trinity_core::vocab::Verdict::Approve)
        })
    });
    assert!(has_alice_approve, "commits[] should carry alice's APPROVE");
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
        body.contains("\"body\""),
        "plan detail should carry raw feedback body; got: {body}"
    );
    assert!(
        !body.contains("\"body_html\""),
        "Phase 2 of wasm-markdown-rendering: body_html must NOT cross the wire on feedback; got: {body}"
    );
}

#[tokio::test]
async fn plan_id_url_stable_across_finish_flip() {
    // The plan_id / URL doesn't move when a plan becomes finished
    // because the file doesn't move — finalize is event-log truth, not
    // a filesystem rename. `state` flips to `finished`,
    // `current_path` stays put.
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
    assert_eq!(before["lifecycle"], "active");
    assert_eq!(before["current_path"], ".trinity/plans/foo.md");

    // Commit a finalize snapshot — plan file stays at the same path.
    write_file(
        dir.path(),
        ".trinity/finished/foo/alice.md",
        "APPROVE\n\nlgtm\n",
    );
    commit(dir.path(), "Finalize foo");
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
        after["lifecycle"], "finished",
        "lifecycle should flip to finished; full response: {after}"
    );
    assert_eq!(
        after["current_path"], ".trinity/plans/foo.md",
        "current_path should not move — plan file stays at the active path"
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
async fn watch_repo_registers_fresh_repo_and_is_idempotent() {
    let dir = init_repo();
    // Don't add the repo via spawn_daemon_with_repos so watch_repo
    // is the first registration.
    let (url, handle) = spawn_daemon_with_repos(&[]).await;
    let client = reqwest::Client::new();

    let first: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "watch_repo",
            "arguments": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["result"]["status"], "registered");
    assert!(
        first["result"]["basename"].as_str().is_some(),
        "basename should be present: {first}"
    );

    // Second call is the idempotent no-op.
    let second: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "watch_repo",
            "arguments": {}
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(second["result"]["status"], "already_watching");
    assert_eq!(first["result"]["repo"], second["result"]["repo"]);
}

/// `wait_for_work` is the ambiguity-prone surface in practice. Two
/// active plans + a `set_active_work` selection → WFW resolves to
/// the selected plan instead of returning `ambiguous_plan`. Without
/// the selection, the same call raises ambiguous.
#[tokio::test]
async fn set_active_work_routes_wait_for_work_through_selection() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");
    write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
    commit(dir.path(), "Add bar");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();

    // Baseline: WFW without plan_id is ambiguous.
    let ambiguous: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "wait_for_work",
            "arguments": {
                "role": "reviewers",
                "author_label": "alice",
                "timeout_secs": 1
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ambiguous["result"]["error"], "ambiguous_plan");

    // Set selection → WFW now resolves to foo and returns work for it.
    client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "set_active_work",
            "arguments": {
                "plan_id": plan_id_for(&dir, "foo"),
                "author_label": "alice"
            }
        }))
        .send()
        .await
        .unwrap();

    let resolved: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "wait_for_work",
            "arguments": {
                "role": "reviewers",
                "author_label": "alice",
                "timeout_secs": 1
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(
        resolved["result"]["plan_id"].as_str().unwrap(),
        plan_id_for(&dir, "foo"),
        "WFW should route to the selected plan; got: {resolved}"
    );
}

/// Stale-selection drop: select a plan, delete its file from the
/// worktree, then resolve. Resolver must fall through to the other
/// active plan, AND the selection must be cleared from memory so
/// subsequent calls don't keep re-validating a doomed entry.
#[tokio::test]
async fn stale_active_work_selection_drops_and_falls_through() {
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");
    write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
    commit(dir.path(), "Add bar");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();

    // Select foo.
    client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "set_active_work",
            "arguments": {
                "plan_id": plan_id_for(&dir, "foo"),
                "author_label": "alice"
            }
        }))
        .send()
        .await
        .unwrap();

    // Render foo invisible by deleting its worktree file.
    std::fs::remove_file(dir.path().join(".trinity/plans/foo.md")).unwrap();

    // work_context without plan_id should fall through to bar (the
    // only active+visible plan).
    let resolved: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "work_context",
            "arguments": { "author_label": "alice" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resolved["result"]["plan_id"].as_str().unwrap(),
        plan_id_for(&dir, "bar"),
        "resolver should drop stale selection and route to bar; got: {resolved}"
    );

    // Restore foo's file. The selection should be gone (dropped on
    // the previous call), so resolution is ambiguous again, not
    // sticky-on-foo.
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    let after_restore: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "work_context",
            "arguments": { "author_label": "alice" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();
    assert_eq!(
        after_restore["result"]["error"], "ambiguous_plan",
        "stale selection must have been cleared; got: {after_restore}"
    );
}

#[tokio::test]
async fn set_active_work_lets_wfw_resolve_amid_multiple_active_plans() {
    // Two active plans in one repo. Without a selection, work_context
    // (which calls resolve_plan_id under the hood) raises
    // `ambiguous_plan`. After set_active_work, the resolver routes to
    // the selected plan instead.
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");
    write_file(dir.path(), ".trinity/plans/bar.md", "# bar\n");
    commit(dir.path(), "Add bar");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();

    // Baseline: omitting plan_id raises ambiguous_plan.
    let baseline: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "work_context",
            "arguments": { "author_label": "alice" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(baseline["result"]["error"], "ambiguous_plan");

    // Select foo.
    let set_resp: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "set_active_work",
            "arguments": {
                "plan_id": plan_id_for(&dir, "foo"),
                "author_label": "alice"
            }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(set_resp["result"]["ok"], true);

    // Now work_context without plan_id should resolve foo.
    let resolved: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "work_context",
            "arguments": { "author_label": "alice" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        resolved["result"]["plan_id"].as_str().unwrap(),
        plan_id_for(&dir, "foo")
    );

    // Clear and confirm ambiguity returns.
    let clear_resp: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "clear_active_work",
            "arguments": { "author_label": "alice" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(clear_resp["result"]["ok"], true);

    let after_clear: serde_json::Value = client
        .post(format!("{}/internal/tool_call", url))
        .json(&json!({
            "cwd": dir.path(),
            "tool": "work_context",
            "arguments": { "author_label": "alice" }
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after_clear["result"]["error"], "ambiguous_plan");

    handle.abort();
}

#[tokio::test]
async fn start_plan_rejects_repo_basename_collision() {
    // Two distinct canonical repos with the same `file_name`: the second
    // one's `start_plan` must fail with a basename-collision error
    // (`repo_basename_taken`) before any disk mutation.
    let registry_parent = tempfile::tempdir().unwrap();
    let registry_path = registry_parent.path().join("repos");

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

    let (url, handle) = spawn_daemon_with_repos_file(&registry_path, &[]).await;
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
    // all (the first repo's path is fine, that's the whole point). The
    // daemon stores canonical paths (`/private/var/...` on macOS), so
    // canonicalize before comparing — a byte-level compare against the
    // non-canonical form is vacuous.
    let registry = std::fs::read_to_string(&registry_path).unwrap_or_default();
    let dir_b_canonical = dir_b.canonicalize().unwrap().to_string_lossy().to_string();
    assert!(
        !registry.lines().any(|l| l.trim() == dir_b_canonical),
        "registry must not record dir_b on collision; registry: {registry}"
    );
}

#[tokio::test]
async fn start_plan_concurrent_basename_twins_loser_does_no_disk_mutation() {
    // Round 3 regression: two `start_plan`s from basename-twin repos
    // fired concurrently must result in exactly one repo with a written
    // `.gitignore` + registry entry — the loser must not leak disk
    // mutations between the precheck and the atomic register.
    let registry_parent = tempfile::tempdir().unwrap();
    let registry_path = registry_parent.path().join("repos");

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

    let (url, handle) = spawn_daemon_with_repos_file(&registry_path, &[]).await;
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
    let registry = std::fs::read_to_string(&registry_path).unwrap_or_default();
    let loser_canonical = loser_dir
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        !registry.lines().any(|l| l.trim() == loser_canonical),
        "registry must not record the loser twin; registry: {registry}"
    );
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
    let registry_parent = tempfile::tempdir().unwrap();
    let registry_path = registry_parent.path().join("repos");

    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("orphan");
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&dir, &["config", "user.email", "test@test"]);
    run_git(&dir, &["config", "user.name", "test"]);
    run_git(&dir, &["config", "commit.gpgsign", "false"]);

    let (url, handle) = spawn_daemon_with_repos_file(&registry_path, &[]).await;
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
    let registry = std::fs::read_to_string(&registry_path).unwrap_or_default();
    let dir_canonical = dir.canonicalize().unwrap().to_string_lossy().to_string();
    assert!(
        !registry.lines().any(|l| l.trim() == dir_canonical),
        "registry must not contain unwatched repo; registry: {registry}"
    );
}

#[tokio::test]
async fn delete_repo_removes_non_canonical_registry_lines() {
    // Codex regression: `remove_repo_from_registry` used to compare
    // each registry line byte-for-byte against the canonical repo
    // root. A non-canonical-but-equivalent line (path with `..`,
    // trailing slash, or a symlinked prefix) was kept, so DELETE
    // returned ok:true and the repo resurrected on next daemon
    // restart. The fix canonicalizes each line before comparing.
    let registry_parent = tempfile::tempdir().unwrap();
    let registry_path = registry_parent.path().join("repos");

    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("noncanon");
    std::fs::create_dir_all(&dir).unwrap();
    run_git(&dir, &["init", "--quiet", "--initial-branch=main"]);
    run_git(&dir, &["config", "user.email", "test@test"]);
    run_git(&dir, &["config", "user.name", "test"]);
    run_git(&dir, &["config", "commit.gpgsign", "false"]);
    write_file(&dir, ".trinity/plans/foo.md", "# foo\n");
    commit(&dir, "add foo");

    // Seed the registry with the canonical path so the daemon loads it.
    let canonical = dir.canonicalize().unwrap();
    let (url, handle) =
        spawn_daemon_with_repos_file(&registry_path, std::slice::from_ref(&canonical)).await;
    // Now rewrite the registry with a NON-canonical equivalent path
    // that canonicalizes back to the same root but is NOT
    // `Path::eq`-equal — `parent/dirname/../dirname`. The `..` form
    // survives `Path::eq` because Path::components doesn't resolve
    // parent traversals (only filesystem canonicalization does), so a
    // naive byte/component compare misses it.
    let dir_name = canonical.file_name().unwrap().to_string_lossy();
    let parent_canonical = canonical.parent().unwrap();
    let non_canonical_line = format!("{}/{dir_name}/../{dir_name}", parent_canonical.display());
    // Sanity check: this string is NOT Path::eq to canonical
    // (otherwise the regression would be untestable).
    assert_ne!(
        std::path::Path::new(&non_canonical_line),
        canonical.as_path(),
        "test setup: non_canonical_line must differ from canonical under Path::eq",
    );
    // …but it DOES canonicalize to the same root.
    assert_eq!(
        std::fs::canonicalize(&non_canonical_line).unwrap(),
        canonical,
        "test setup: non_canonical_line must resolve to the canonical root",
    );
    std::fs::write(&registry_path, format!("{non_canonical_line}\n")).unwrap();

    let resp = reqwest::Client::new()
        .delete(format!("{url}/api/repos/noncanon"))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();
    handle.abort();

    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["ok"], true, "delete should succeed; body: {body}");

    let after = std::fs::read_to_string(&registry_path).unwrap_or_default();
    assert!(
        !after.lines().any(|l| l.trim() == non_canonical_line),
        "non-canonical registry entry must be removed; got: {after}"
    );
    assert!(
        after.trim().is_empty(),
        "registry should be empty after removing the only entry; got: {after}"
    );
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
    // remove_repo_from_registry fails with EACCES. Wrap the chmod in
    // a Drop guard so if any subsequent `.unwrap()` panics, the
    // tempdir can still be cleaned up rather than leaving a
    // read-only dir behind.
    struct ChmodGuard {
        path: std::path::PathBuf,
        original_mode: u32,
    }
    impl Drop for ChmodGuard {
        fn drop(&mut self) {
            if let Ok(meta) = std::fs::metadata(&self.path) {
                let mut perms = meta.permissions();
                perms.set_mode(self.original_mode);
                let _ = std::fs::set_permissions(&self.path, perms);
            }
        }
    }
    let original_mode = std::fs::metadata(registry_parent.path())
        .unwrap()
        .permissions()
        .mode();
    let mut perms = std::fs::metadata(registry_parent.path())
        .unwrap()
        .permissions();
    perms.set_mode(0o555);
    std::fs::set_permissions(registry_parent.path(), perms).unwrap();
    let _chmod_guard = ChmodGuard {
        path: registry_parent.path().to_path_buf(),
        original_mode,
    };

    let resp = client
        .delete(format!("{url}/api/repos/locked"))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();

    // Guard restores permissions on Drop; explicit restore before
    // tempdir teardown keeps the test readable.
    drop(_chmod_guard);
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
    // DELETE handler must write to THIS path.
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

    let custom_body = std::fs::read_to_string(&registry_path).unwrap_or_default();
    // start_plan persists whatever `git rev-parse --show-toplevel`
    // returned, which on macOS is the canonical /private/var/... form.
    let dir_canonical = dir.canonicalize().unwrap().to_string_lossy().to_string();
    assert!(
        custom_body.lines().any(|l| l.trim() == dir_canonical),
        "custom registry should contain repo after start_plan; \
         dir_canonical={dir_canonical}; got: {custom_body}"
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

#[tokio::test]
async fn finalize_commit_endpoint_returns_full_snapshot_not_just_diff() {
    // Codex's regression scenario: snapshot has TWO approval files
    // (alice + bob) but the freeze commit only changes ONE of them
    // (bob's file flips from REQUEST_CHANGES → APPROVE). The
    // /api/plan/.../commit/{sha} endpoint must return BOTH approval
    // files for the finalize event, not just bob's diff.
    let dir = init_repo();
    write_file(dir.path(), ".trinity/plans/foo.md", "# foo\n");
    commit(dir.path(), "Add foo");
    // c2: alice approves, bob requests changes — does not freeze.
    write_file(
        dir.path(),
        ".trinity/finished/foo/alice.md",
        "APPROVE\n\nLooks good to me.\n",
    );
    write_file(
        dir.path(),
        ".trinity/finished/foo/bob.md",
        "REQUEST_CHANGES\n\nNeeds more work.\n",
    );
    commit(dir.path(), "First reviewer round");
    // c3: bob flips to APPROVE — THIS is the freeze commit. The
    // commit only changes bob.md; alice.md is unchanged.
    write_file(
        dir.path(),
        ".trinity/finished/foo/bob.md",
        "APPROVE\n\nAddressed. Ship it.\n",
    );
    commit(dir.path(), "Bob's approval lands");

    let (url, handle) = spawn_daemon(dir.path()).await;
    let client = reqwest::Client::new();

    let plan = fetch_plan_detail(&client, &url, &dir, "foo").await;
    let finalize_sha = plan
        .timeline
        .iter()
        .find_map(|e| match e {
            trinity_core::api::TimelineEvent::CommitFinalize { sha, .. } => Some(sha.clone()),
            _ => None,
        })
        .expect("timeline must carry a commit_finalize event");

    let basename = dir.path().file_name().unwrap().to_str().unwrap();
    let body: serde_json::Value = client
        .get(format!(
            "{url}/api/plan/{basename}/foo.md/commit/{finalize_sha}"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    handle.abort();

    // Phase 5b: CommitDetailResponse uses `#[serde(flatten)]` over
    // the tagged `CommitDetail` enum, so a Finalize commit's wire
    // shape has `"kind": "finalize"` at the top level and the
    // variant's `snapshot: [...]` field next to it. The old
    // `finalize_snapshot` field name is gone.
    assert_eq!(body["kind"], "finalize");
    let snapshot = body["snapshot"]
        .as_array()
        .unwrap_or_else(|| panic!("finalize commit body must carry snapshot; got {body:?}"));
    assert_eq!(
        snapshot.len(),
        2,
        "snapshot must include BOTH approval files (alice + bob), \
         not just the file touched by this freeze commit. got: {snapshot:?}"
    );
    let authors: Vec<&str> = snapshot
        .iter()
        .map(|e| e["author"].as_str().unwrap())
        .collect();
    assert!(authors.contains(&"alice"), "alice must be in snapshot");
    assert!(authors.contains(&"bob"), "bob must be in snapshot");
    let alice = snapshot.iter().find(|e| e["author"] == "alice").unwrap();
    // Phase 3 of wasm-markdown-rendering: FinalizeApproval ships
    // raw markdown via `body`; the frontend renders.
    let alice_body = alice["body"].as_str().unwrap();
    assert!(
        alice_body.contains("Looks good"),
        "alice's raw body must carry her approval text; got: {alice_body}"
    );
    assert!(
        alice["body_html"].is_null(),
        "body_html must NOT cross the wire on FinalizeApproval"
    );
    // Finalize events are not gated — the tagged enum doesn't even
    // carry a `feedback` field for the Finalize variant.
    assert!(
        body.get("feedback").is_none(),
        "finalize variant must not carry `feedback` field; got: {body:?}"
    );
}
