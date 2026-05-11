//! Regression guardrail: the MCP stdio shim must reserve stdout for
//! JSON-RPC frames only. If `tracing_subscriber` ever defaults back to
//! stdout, the first line a client sees will be an ANSI-coloured INFO
//! log instead of a JSON object, and strict clients (e.g. Codex's
//! rmcp_client) will reject the connection.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[test]
fn mcp_stdout_starts_with_jsonrpc_frame() {
    // Point at an unroutable, non-loopback host so the shim does NOT try
    // to auto-spawn a daemon. The shim's `initialize` response comes
    // from its static catalog, so a missing daemon is irrelevant here.
    // 192.0.2.0/24 is TEST-NET-1, guaranteed-unallocated per RFC 5737.
    let bin = env!("CARGO_BIN_EXE_trinity");
    let mut child = Command::new(bin)
        .arg("mcp")
        .env("TRINITY_DAEMON", "http://192.0.2.1:7777")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn `trinity mcp`");

    // Send a single `initialize` request. Keep stdin open afterwards so
    // the shim doesn't tear down before we read its reply.
    let mut stdin = child.stdin.take().expect("stdin pipe");
    let req = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}
"#;
    stdin.write_all(req).expect("write initialize");
    stdin.flush().expect("flush stdin");

    // Read the first line of stdout in a worker thread; use a channel
    // for timeout so a hung shim can't hang the test forever.
    let stdout = child.stdout.take().expect("stdout pipe");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let _ = reader.read_until(b'\n', &mut line);
        let _ = tx.send(line);
    });

    let first_line = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("shim did not write a stdout line within 10s");

    // Tear down before asserting so a panic message doesn't strand the child.
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    let first_byte = first_line
        .first()
        .copied()
        .expect("shim wrote an empty stdout line");
    assert_eq!(
        first_byte,
        b'{',
        "first stdout byte must be `{{` (JSON-RPC frame); got 0x{first_byte:02x}, line = {:?}",
        String::from_utf8_lossy(&first_line)
    );

    let line_str = std::str::from_utf8(&first_line)
        .expect("stdout was not utf-8")
        .trim_end_matches(['\r', '\n']);
    let parsed: serde_json::Value =
        serde_json::from_str(line_str).expect("first stdout line must parse as JSON");
    assert_eq!(parsed["jsonrpc"], "2.0");
    assert_eq!(parsed["id"], 1);
    assert!(
        parsed["result"]["protocolVersion"].is_string(),
        "expected initialize result with protocolVersion, got: {parsed}"
    );
}
