//! Shim regression tests: cross-session calls are NOT refused, and the
//! shim's per-tool label cache fills in `label` / `author_label` when the
//! caller omits them.
//!
//! These tests drive `trinity mcp` over its real stdio JSON-RPC interface
//! against an auto-spawned daemon. Each test gets its own temporary
//! `~/.trinity` and TCP port so it doesn't fight other tests for state.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use common::run_git;

struct Shim {
    child: Child,
    stdin: ChildStdin,
    lines_rx: mpsc::Receiver<String>,
    next_id: i64,
    _tmp: TempDir,
}

impl Shim {
    /// Spawn `trinity mcp` against a unique loopback port + temp DB. Lets
    /// the shim auto-spawn the daemon on first probe.
    fn spawn() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("trinity.sqlite");
        // Pick an ephemeral port: bind, capture, drop, hand to the daemon.
        // The daemon binds the same port a beat later. Acceptable race
        // because nothing else on the box is racing for these ports.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let daemon_url = format!("http://127.0.0.1:{port}");

        let bin = env!("CARGO_BIN_EXE_trinity");
        let mut child = Command::new(bin)
            .arg("mcp")
            .env("TRINITY_DAEMON", &daemon_url)
            .env("TRINITY_DB", db.to_string_lossy().to_string())
            .env("TRINITY_BIND", format!("127.0.0.1:{port}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn trinity mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        let mut shim = Self {
            child,
            stdin,
            lines_rx: rx,
            next_id: 1,
            _tmp: tmp,
        };
        // Handshake.
        let init = shim.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "trinity");
        shim.notification("notifications/initialized", json!({}));
        shim
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", frame).unwrap();
        self.stdin.flush().unwrap();
        // Read until we see the matching id.
        loop {
            let line = self
                .lines_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("shim did not respond within 20s");
            let v: Value = serde_json::from_str(line.trim()).expect("non-JSON line on stdout");
            if v.get("id").and_then(|x| x.as_i64()) == Some(id) {
                return v;
            }
            // Drop unrelated messages (notifications etc.).
        }
    }

    fn notification(&mut self, method: &str, params: Value) {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", frame).unwrap();
        self.stdin.flush().unwrap();
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Drain stderr to surface any panic for debugging.
        if let Some(mut e) = self.child.stderr.take() {
            let mut buf = String::new();
            let _ = e.read_to_string(&mut buf);
            if !buf.is_empty() && std::env::var("TRINITY_SHIM_STDERR").is_ok() {
                eprintln!("shim stderr:\n{buf}");
            }
        }
    }
}

/// Set up a git repo + plan file in a tempdir, return the plan-file path.
fn make_repo(repo: &Path) -> PathBuf {
    std::fs::create_dir_all(repo).unwrap();
    run_git(repo, &["init", "-q"]);
    run_git(repo, &["config", "user.email", "test@trinity"]);
    run_git(repo, &["config", "user.name", "trinity-test"]);
    std::fs::write(repo.join(".gitkeep"), b"").unwrap();
    run_git(repo, &["add", ".gitkeep"]);
    run_git(repo, &["commit", "-q", "-m", "init"]);
    let plan = repo.join("plan.md");
    std::fs::write(&plan, "# body\n").unwrap();
    plan
}

fn structured_result(call: &Value) -> Value {
    call.get("result")
        .and_then(|r| r.get("structuredContent"))
        .cloned()
        .or_else(|| call.get("result").cloned())
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn cross_session_calls_are_not_refused() {
    let mut shim = Shim::spawn();
    let repo = shim._tmp.path().join("repo");
    let plan = make_repo(&repo);
    // The shim's cwd is the test process's cwd, not the repo. The daemon
    // resolves repo_root from `cwd`, so set absolute plan path + use the
    // repo as cwd via the shim's TRINITY_CWD if we had one. Since we
    // don't, we register from the test process's cwd which is the trinity
    // crate root — that IS a git repo. Use the trinity crate root.
    let _ = (repo, plan);
    let crate_root = std::env::current_dir().unwrap();
    let plan_path = crate_root.join("Cargo.toml"); // any file in the repo
    // We only care that `register_plan_file` succeeds binding label "A"
    // into the shim's cache.

    let r = shim.call_tool(
        "register_plan_file",
        json!({
            "session_id": "session-a",
            "path": plan_path.to_string_lossy().to_string(),
            "label": "A",
        }),
    );
    let res = structured_result(&r);
    assert_eq!(res["session_id"], "session-a");

    // Now call get_current_feedback for a DIFFERENT session. The old shim
    // would refuse; the new shim must forward (daemon will report
    // not-found, which is fine — what matters is no shim-level refusal).
    let r2 = shim.call_tool("get_current_feedback", json!({ "session_id": "session-b" }));
    let text_block = r2["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(
        !text_block.contains("this shell is bound") && !text_block.contains("refusing to forward"),
        "shim refused cross-session call: {text_block}"
    );
}

#[tokio::test]
async fn cached_label_fills_missing_author_label() {
    let mut shim = Shim::spawn();
    let crate_root = std::env::current_dir().unwrap();

    // We'll use the trinity repo as the git-backed working tree the shim
    // sees. Make a plan file inside it (in a tempdir under crate root to
    // avoid clobbering anything).
    let tmp_plan = shim._tmp.path().join("plan-for-cache.md");
    std::fs::write(&tmp_plan, "# v1\n").unwrap();

    let r = shim.call_tool(
        "register_plan_file",
        json!({
            "session_id": "cache-session",
            "path": tmp_plan.to_string_lossy().to_string(),
            "label": "rev-a",
        }),
    );
    let res = structured_result(&r);
    let rev_id = res["revision_id"].as_i64().expect("revision_id");
    let _ = crate_root;

    // Omit author_label entirely on put_feedback — shim should fill it
    // from the cached "rev-a".
    let r = shim.call_tool(
        "put_feedback",
        json!({
            "session_id": "cache-session",
            "target_kind": "plan_revision",
            "target_id": rev_id.to_string(),
            "body": "from cache fallback",
        }),
    );
    let res = structured_result(&r);
    let feedback_id = res
        .get("feedback_id")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(|| panic!("expected feedback_id in result: {res}"));

    // Read it back; the daemon round-trips body + author_label.
    let view = shim.call_tool(
        "get_current_feedback",
        json!({ "session_id": "cache-session" }),
    );
    let res = structured_result(&view);
    let items = res["feedback"].as_array().expect("feedback array");
    let mine = items
        .iter()
        .find(|i| i["feedback_id"].as_i64() == Some(feedback_id))
        .expect("our feedback row in current view");
    assert_eq!(mine["author_label"], "rev-a");
    assert_eq!(mine["body"], "from cache fallback");
}

#[tokio::test]
async fn caller_supplied_author_label_wins_over_cache() {
    let mut shim = Shim::spawn();
    let tmp_plan = shim._tmp.path().join("plan.md");
    std::fs::write(&tmp_plan, "# v1\n").unwrap();

    let r = shim.call_tool(
        "register_plan_file",
        json!({
            "session_id": "override-session",
            "path": tmp_plan.to_string_lossy().to_string(),
            "label": "rev-a",
        }),
    );
    let res = structured_result(&r);
    let rev_id = res["revision_id"].as_i64().unwrap();

    // Caller supplies an explicit different author_label. Cache must not override.
    let r = shim.call_tool(
        "put_feedback",
        json!({
            "session_id": "override-session",
            "target_kind": "plan_revision",
            "target_id": rev_id.to_string(),
            "body": "from explicit author",
            "author_label": "rev-b",
        }),
    );
    let res = structured_result(&r);
    let feedback_id = res["feedback_id"].as_i64().unwrap();

    let view = shim.call_tool(
        "get_current_feedback",
        json!({ "session_id": "override-session" }),
    );
    let res = structured_result(&view);
    let items = res["feedback"].as_array().unwrap();
    let mine = items
        .iter()
        .find(|i| i["feedback_id"].as_i64() == Some(feedback_id))
        .expect("our feedback row in current view");
    assert_eq!(mine["author_label"], "rev-b");
}
