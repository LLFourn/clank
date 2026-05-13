//! Shim regression tests: per-tool label cache fills in the missing label
//! argument when the caller omits it. With the watcher-coordinator model
//! the catalog is only three tools. The only tools taking a label
//! argument are `register_plan_file` (`label`) and `get_context`
//! (`author_label`).

mod common;

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Shim {
    proc: ShimProcess,
    serve: Child,
    tmp: TempDir,
}

impl Shim {
    fn spawn() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("trinity.sqlite");
        let port = free_port();
        let bind = format!("127.0.0.1:{port}");
        let daemon_url = format!("http://{bind}");

        let serve = start_serve(tmp.path(), &db, &bind);
        assert!(
            wait_for_health(&bind, Duration::from_secs(10)),
            "managed daemon did not become healthy"
        );

        let mut proc = ShimProcess::spawn(tmp.path(), &db, &bind, &daemon_url);
        proc.initialize();

        Self { proc, serve, tmp }
    }

    fn tmp_path(&self) -> &Path {
        self.tmp.path()
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.proc.call_tool(name, arguments)
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        self.proc.terminate();
        terminate_child(&mut self.serve);
    }
}

struct ShimProcess {
    child: Child,
    stdin: ChildStdin,
    lines_rx: mpsc::Receiver<String>,
    next_id: i64,
}

impl ShimProcess {
    fn spawn(home: &Path, db: &Path, bind: &str, daemon_url: &str) -> Self {
        let bin = env!("CARGO_BIN_EXE_trinity");
        let mut child = Command::new(bin)
            .arg("mcp")
            .env("HOME", home)
            .env("TRINITY_DAEMON", daemon_url)
            .env("TRINITY_DB", db)
            .env("TRINITY_BIND", bind)
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

        Self {
            child,
            stdin,
            lines_rx: rx,
            next_id: 1,
        }
    }

    fn initialize(&mut self) {
        let init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "trinity");
        self.notification("notifications/initialized", json!({}));
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

    fn terminate(&mut self) {
        terminate_child(&mut self.child);
        if let Some(mut e) = self.child.stderr.take() {
            let mut buf = String::new();
            let _ = e.read_to_string(&mut buf);
            if !buf.is_empty() && std::env::var("TRINITY_SHIM_STDERR").is_ok() {
                eprintln!("shim stderr:\n{buf}");
            }
        }
    }
}

impl Drop for ShimProcess {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct AutoSpawnedDaemon {
    home: PathBuf,
    bind: String,
    terminated: bool,
}

impl AutoSpawnedDaemon {
    fn new(home: &Path, bind: &str) -> Self {
        Self {
            home: home.to_path_buf(),
            bind: bind.to_string(),
            terminated: false,
        }
    }

    fn terminate(&mut self) -> bool {
        if self.terminated {
            return true;
        }
        self.terminated = true;

        let pid_path = self.home.join(".trinity/daemon.spawn.last_pid");
        let Ok(pid) = std::fs::read_to_string(&pid_path) else {
            return false;
        };
        let Ok(pid) = pid.trim().parse::<u32>() else {
            return false;
        };

        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        if wait_until_unhealthy(&self.bind, Duration::from_secs(5)) {
            return true;
        }

        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .status();
        wait_until_unhealthy(&self.bind, Duration::from_secs(5))
    }
}

impl Drop for AutoSpawnedDaemon {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

struct HeldSpawnLock {
    _file: File,
}

impl HeldSpawnLock {
    fn acquire(home: &Path) -> Self {
        let lock_dir = home.join(".trinity");
        std::fs::create_dir_all(&lock_dir).unwrap();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(lock_dir.join("daemon.spawn.lock"))
            .unwrap();
        try_flock_exclusive_nb(&file)
            .expect("flock spawn lock")
            .then_some(())
            .expect("test should acquire spawn lock");
        Self { _file: file }
    }
}

fn start_serve(home: &Path, db: &Path, bind: &str) -> Child {
    let bin = env!("CARGO_BIN_EXE_trinity");
    Command::new(bin)
        .arg("serve")
        .env("HOME", home)
        .env("TRINITY_DB", db)
        .env("TRINITY_BIND", bind)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn trinity serve")
}

fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn wait_for_health(bind: &str, max: Duration) -> bool {
    let addr: SocketAddr = bind.parse().expect("parse bind addr");
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        if health_check(addr) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_until_unhealthy(bind: &str, max: Duration) -> bool {
    let addr: SocketAddr = bind.parse().expect("parse bind addr");
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        if !health_check(addr) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn health_check(addr: SocketAddr) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(200)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    if stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }

    let mut buf = [0; 256];
    match stream.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).contains("200 OK"),
        Err(_) => false,
    }
}

fn try_flock_exclusive_nb(file: &File) -> io::Result<bool> {
    let rc = unsafe { test_flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }

    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err)
    }
}

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

unsafe extern "C" {
    #[link_name = "flock"]
    fn test_flock(fd: i32, operation: i32) -> i32;
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
    let tmp_plan = shim.tmp_path().join("plan.md");
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
    assert_eq!(res["session_id"], "cache-session");

    let r = shim.call_tool("get_context", json!({ "session_id": "cache-session" }));
    let res = structured_result(&r);
    assert_eq!(res["session_id"], "cache-session");

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
    let tmp_plan = shim.tmp_path().join("plan.md");
    std::fs::write(&tmp_plan, "# v1\n").unwrap();

    shim.call_tool(
        "register_plan_file",
        json!({
            "session_id": "override-session",
            "path": tmp_plan.to_string_lossy().to_string(),
            "label": "rev-a",
        }),
    );

    let r = shim.call_tool(
        "get_context",
        json!({ "session_id": "override-session", "author_label": "rev-b" }),
    );
    let res = structured_result(&r);
    let feedback_path = res["write_feedback"]["path"].as_str().unwrap_or("");
    assert!(
        feedback_path.ends_with("/plan/rev-b.md"),
        "explicit author_label must win and sit under plan/: got path {feedback_path}"
    );
}

#[test]
fn busy_spawn_lock_waiters_share_one_autospawned_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("trinity.sqlite");
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    let daemon_url = format!("http://{bind}");
    let mut daemon = AutoSpawnedDaemon::new(tmp.path(), &bind);

    let held_lock = HeldSpawnLock::acquire(tmp.path());
    let mut waiter_a = ShimProcess::spawn(tmp.path(), &db, &bind, &daemon_url);
    let mut waiter_b = ShimProcess::spawn(tmp.path(), &db, &bind, &daemon_url);
    thread::sleep(Duration::from_millis(300));

    assert!(
        !wait_for_health(&bind, Duration::from_millis(200)),
        "no daemon should spawn while the test holds the spawn lock"
    );
    assert!(
        !tmp.path().join(".trinity/daemon.spawn.last_pid").exists(),
        "busy-lock waiters must not write the autospawn pid sidecar"
    );

    drop(held_lock);
    let mut spawner = ShimProcess::spawn(tmp.path(), &db, &bind, &daemon_url);

    spawner.initialize();
    waiter_a.initialize();
    waiter_b.initialize();
    assert!(
        wait_for_health(&bind, Duration::from_secs(5)),
        "autospawned daemon should become healthy"
    );

    let pid_path = tmp.path().join(".trinity/daemon.spawn.last_pid");
    let pid = std::fs::read_to_string(&pid_path).expect("daemon pid sidecar");
    pid.trim().parse::<u32>().expect("daemon pid is numeric");

    let log_path = tmp.path().join(".trinity/daemon.log");
    let log_len = std::fs::metadata(&log_path)
        .unwrap_or_else(|e| panic!("daemon log missing at {}: {e}", log_path.display()))
        .len();
    assert!(log_len > 0, "daemon log should be written under temp HOME");

    drop(waiter_a);
    drop(waiter_b);
    drop(spawner);
    assert!(
        daemon.terminate(),
        "autospawned daemon should terminate via pid sidecar"
    );
}
