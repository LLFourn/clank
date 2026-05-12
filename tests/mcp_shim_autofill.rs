//! Shim regression tests: per-tool label cache fills in the missing label
//! argument when the caller omits it. With the watcher-coordinator model
//! the catalog is only three tools — the only ones taking a label
//! argument are `register_plan_file` (`label`) and `get_context`
//! (`author_label`).

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

struct Shim {
    child: Child,
    stdin: ChildStdin,
    lines_rx: mpsc::Receiver<String>,
    next_id: i64,
    _tmp: TempDir,
}

impl Shim {
    fn spawn() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("trinity.sqlite");
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
        writeln!(self.stdin, "{frame}").unwrap();
        self.stdin.flush().unwrap();
        loop {
            let line = self
                .lines_rx
                .recv_timeout(Duration::from_secs(20))
                .expect("shim did not respond within 20s");
            let v: Value = serde_json::from_str(line.trim()).expect("non-JSON line on stdout");
            if v.get("id").and_then(|x| x.as_i64()) == Some(id) {
                return v;
            }
        }
    }

    fn notification(&mut self, method: &str, params: Value) {
        let frame = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{frame}").unwrap();
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
        if let Some(mut e) = self.child.stderr.take() {
            let mut buf = String::new();
            let _ = e.read_to_string(&mut buf);
            if !buf.is_empty() && std::env::var("TRINITY_SHIM_STDERR").is_ok() {
                eprintln!("shim stderr:\n{buf}");
            }
        }
    }
}

fn structured_result(call: &Value) -> Value {
    call.get("result")
        .and_then(|r| r.get("structuredContent"))
        .cloned()
        .or_else(|| call.get("result").cloned())
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn cached_label_fills_missing_author_label_on_get_context() {
    let mut shim = Shim::spawn();
    let tmp_plan = shim._tmp.path().join("plan.md");
    std::fs::write(&tmp_plan, "# v1\n").unwrap();

    // register_plan_file seeds the cache with label "rev-a".
    let r = shim.call_tool(
        "register_plan_file",
        json!({
            "session_id": "cache-session",
            "path": tmp_plan.to_string_lossy().to_string(),
            "label": "rev-a",
        }),
    );
    let res = structured_result(&r);
    assert_eq!(res["session_id"], "cache-session");

    // Call get_context without author_label — shim must fill it from cache.
    let r = shim.call_tool("get_context", json!({ "session_id": "cache-session" }));
    let res = structured_result(&r);
    assert_eq!(res["session_id"], "cache-session");

    // `write_feedback` is non-null iff author_label was supplied. The
    // cached "rev-a" should have flowed through.
    let write = res
        .get("write_feedback")
        .expect("write_feedback key absent");
    assert!(
        !write.is_null(),
        "shim should have filled cached author_label: {res}"
    );
}

#[tokio::test]
async fn caller_supplied_author_label_wins_over_cache() {
    let mut shim = Shim::spawn();
    let tmp_plan = shim._tmp.path().join("plan.md");
    std::fs::write(&tmp_plan, "# v1\n").unwrap();

    shim.call_tool(
        "register_plan_file",
        json!({
            "session_id": "override-session",
            "path": tmp_plan.to_string_lossy().to_string(),
            "label": "rev-a",
        }),
    );

    // Caller supplies a different author_label explicitly. The
    // write_feedback path returned by get_context must reflect that,
    // not the cached "rev-a". Under the split-feedback layout the path
    // is `<...>/plan/rev-b.md` during the planning phase.
    let r = shim.call_tool(
        "get_context",
        json!({ "session_id": "override-session", "author_label": "rev-b" }),
    );
    let res = structured_result(&r);
    let feedback_path = res["write_feedback"]["path"].as_str().unwrap_or("");
    assert!(
        feedback_path.ends_with("/plan/rev-b.md"),
        "explicit author_label must win + sit under plan/: got path {feedback_path}"
    );
}
