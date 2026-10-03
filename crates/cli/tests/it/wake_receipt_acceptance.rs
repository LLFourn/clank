//! Real CLI producer -> WAL -> wait -> hook -> callback contracts. These
//! need the subprocess boundary; no providers, terminals or live accounts.
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clank::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::agent_config::{AgentConfig, CommandSource, WaitEventSource};
use clank_core::ids::{AgentLabel, SessionId};
use clank_core::{AutoMode, Tool};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixDatagram;
use tokio::process::{Child, Command};

use crate::common::TestEnv;
const LIMIT: Duration = Duration::from_secs(20);
const SESSION: &str = "receipt-acceptance-a1";
const ID: &str = "utt/a/雪\n\"$()";

struct Fixture {
    _env: TestEnv,
    repo: PathBuf,
    home: PathBuf,
    hook_cwd: PathBuf,
    host_cwd: PathBuf,
    gate: std::fs::File,
    ready: UnixDatagram,
    output: PathBuf,
    source: CommandSource,
    author: AgentLabel,
    tool: Tool,
    producer: Option<(i32, clank::proc_identity::ProcToken)>,
}
impl Fixture {
    fn new(tool: Tool, receipts: bool) -> Self {
        let env = TestEnv::init();
        let repo = dunce::canonicalize(env.repo()).unwrap();
        let home = dunce::canonicalize(env.home()).unwrap();
        let author = AgentLabel::parse("codex").unwrap();
        clank::cli::agent::add_repo_roster_agent(
            &repo,
            &author,
            AgentDescription {
                tool,
                launch: None,
                initial_prompt: None,
            },
            RosterRole::Commit,
        )
        .unwrap();
        clank::cli::agent::set_repo_master(&repo, &author).unwrap();
        std::fs::write(repo.join("seed"), "seed").unwrap();
        for args in [
            vec!["add", "seed"],
            vec!["commit", "--quiet", "-m", "fixture"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let hook_cwd = repo.join("hook-cwd");
        let host_cwd = repo.join("monitor-cwd");
        std::fs::create_dir(&hook_cwd).unwrap();
        std::fs::create_dir(&host_cwd).unwrap();
        let gate_path = repo.join("producer-gate");
        let name = std::ffi::CString::new(gate_path.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        // RDWR keeps fixture readiness cancellation safe and writing finite;
        // the producer reads one line, so no EOF dependency is introduced.
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&gate_path)
            .unwrap();
        let ready_path = repo.join("producer-ready.sock");
        let ready = UnixDatagram::bind(&ready_path).unwrap();
        let output = repo.join("receipt.json");
        let source = CommandSource { interrupt: false,
            name: "voice/a".into(),
            command: vec!["/usr/bin/python3".into(), "-c".into(),
                "import json,os,socket,sys,time; s=socket.socket(socket.AF_UNIX,socket.SOCK_DGRAM); s.sendto(json.dumps({'pid':os.getpid(),'parent':os.getppid(),'cwd':os.getcwd(),'state':os.environ['CLANK_SOURCE_STATE_DIR'],'argv':sys.argv[1:3]}).encode(),sys.argv[3]); g=open(sys.argv[4]); data=json.loads(g.readline()); print(json.dumps(data),flush=True); time.sleep(300)".into(),
                "events".into(), "--flag".into(), ready_path.to_str().unwrap().into(), gate_path.to_str().unwrap().into()],
            reply_command: Some(vec!["/bin/false".into()]), receipt_command: None, glyph: None,
        };
        let cfg = AgentConfig {
            auto_mode: Some(AutoMode::On),
            wait_events: vec![WaitEventSource::Command(source.clone())],
            ..AgentConfig::default()
        };
        clank::agent_store::save_agent_config(&repo, &author, &cfg).unwrap();
        clank::agent_store::bind_session_to_agent(
            &repo,
            &author,
            tool,
            &SessionId::parse(SESSION).unwrap(),
        )
        .unwrap();
        let mut fixture = Self {
            _env: env,
            repo,
            home,
            hook_cwd,
            host_cwd,
            gate,
            ready,
            output,
            source,
            author,
            tool,
            producer: None,
        };
        if receipts {
            fixture.enable_receipts();
        }
        fixture
    }
    fn enable_receipts(&mut self) {
        self.source.receipt_command = Some(vec!["/usr/bin/python3".into(), "-c".into(),
            "import json,os,pathlib,sys; data={'receipt':json.load(sys.stdin),'cwd':os.getcwd(),'state':os.environ['CLANK_SOURCE_STATE_DIR'],'argv':sys.argv[1:3]}; pathlib.Path(sys.argv[3]).write_text(json.dumps(data)); print('callback stdout'); print('callback stderr',file=sys.stderr)".into(),
            "receipt".into(), "--flag".into(), self.output.to_str().unwrap().into()]);
        let mut cfg = clank::agent_store::load_agent_config(&self.repo, &self.author)
            .unwrap()
            .unwrap();
        cfg.wait_events = vec![WaitEventSource::Command(self.source.clone())];
        clank::agent_store::save_agent_config(&self.repo, &self.author, &cfg).unwrap();
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_clank"));
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("TERM", "dumb")
            .env("CLANK_AGENT", self.author.as_str())
            .env(
                match self.tool {
                    Tool::Claude => "CLAUDE_CODE_SESSION_ID",
                    _ => "CODEX_THREAD_ID",
                },
                SESSION,
            )
            .current_dir(&self.hook_cwd)
            .kill_on_drop(true);
        command
    }
    fn hook(&self, session_start: bool) -> Command {
        let mut command = self.command();
        command
            .args(["stop-hook", "--tool", self.tool.as_str(), "--repo"])
            .arg(&self.repo);
        if session_start {
            command.arg("--session-start");
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
    async fn hook_child(&self, session_start: bool) -> Child {
        let mut child = self.hook(session_start).spawn().unwrap();
        let input = json!({"session_id":SESSION,"cwd":self.hook_cwd,"stop_hook_active":false,"source":"resume"});
        child
            .stdin
            .take()
            .unwrap()
            .write_all(&serde_json::to_vec(&input).unwrap())
            .await
            .unwrap();
        child
    }
    async fn producer_ready(&mut self) -> Value {
        let mut buffer = [0u8; 4096];
        let got = tokio::time::timeout(LIMIT, self.ready.recv(&mut buffer))
            .await
            .expect("producer did not start")
            .unwrap();
        let ready: Value = serde_json::from_slice(&buffer[..got]).unwrap();
        let pid = ready["pid"].as_i64().unwrap() as i32;
        self.producer = Some((pid, clank::proc_identity::token_for(pid).unwrap()));
        assert_eq!(ready["argv"], json!(["events", "--flag"]));
        ready
    }
    fn emit_event(&mut self) {
        serde_json::to_writer(
            &mut self.gate,
            &json!({"id":ID,"kind":"voice","content":"do the task","requires_reply":true}),
        )
        .unwrap();
        self.gate.write_all(b"\n").unwrap();
        self.gate.flush().unwrap();
    }
    fn receipt(&self) -> Value {
        serde_json::from_slice(&std::fs::read(&self.output).unwrap()).unwrap()
    }
    async fn run(&self, args: &[&str]) -> std::process::Output {
        tokio::time::timeout(LIMIT, self.command().args(args).output())
            .await
            .expect("CLI deadline")
            .unwrap()
    }
    async fn assert_owed(&self) -> String {
        let output = self
            .run(&[
                "events",
                "list",
                "--repo",
                self.repo.to_str().unwrap(),
                "--json",
            ])
            .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        let row = value
            .as_array()
            .expect("event list array")
            .iter()
            .find(|row| row["item"]["id"] == ID)
            .expect("original producer row");
        assert_eq!(row["acked"], false);
        assert_eq!(row["item"]["requires_reply"], true);
        assert!(row.get("reply").is_none());
        row["id"].as_str().expect("event selector").into()
    }
    fn monitor_lease(&self) -> std::fs::File {
        let dir = runtime_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.lock", repo_tag(&self.repo)));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        file
    }
    fn assert_receipt(&self, producer: &Value) {
        let value = self.receipt();
        assert_eq!(
            value["receipt"],
            json!({"version":1,"kind":"wake","source":self.source.name,"author":self.author.as_str(),"ids":[ID]})
        );
        assert_eq!(value["cwd"], producer["cwd"]);
        assert_eq!(value["state"], producer["state"]);
        let expected_state = clank::agent_store::agents_root(&self.repo)
            .join(self.author.as_str())
            .join("sources")
            .join(self.source.source_key());
        assert_eq!(value["state"], expected_state.to_str().unwrap());
        assert_eq!(value["argv"], json!(["receipt", "--flag"]));
    }
}
fn runtime_dir() -> PathBuf {
    PathBuf::from(format!("/tmp/clank-run-{}", unsafe { libc::getuid() }))
}
fn repo_tag(repo: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    blake3::hash(repo.as_os_str().as_bytes()).to_hex()[..32].into()
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some((pid, token)) = &self.producer {
            if token.still_holds(*pid) {
                clank::signal::signal_group(clank::signal::Pgid::new(*pid).unwrap(), libc::SIGKILL);
            }
        }
        let prefix = format!("{}.", repo_tag(&self.repo));
        if let Ok(entries) = std::fs::read_dir(runtime_dir()) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_fallback_hook_receives_interrupting_producer_without_zellij_and_keeps_reply_owed() {
    let mut fixture = Fixture::new(Tool::Codex, true);
    fixture.source.interrupt = true;
    fixture.enable_receipts(); // Persist the updated source policy alongside its receipt callback.
    let _lease = fixture.monitor_lease(); // No published monitor: attachment must fall back.
    let hook = fixture.hook_child(false).await;
    let ready = fixture.producer_ready().await;
    assert_eq!(ready["cwd"], fixture.hook_cwd.to_str().unwrap());
    fixture.emit_event();
    let output = tokio::time::timeout(LIMIT, hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(payload["decision"], "block");
    assert!(payload["reason"].as_str().unwrap().contains("do the task"));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fixture.assert_receipt(&ready);
    let selector = fixture.assert_owed().await;
    let shown = fixture
        .run(&[
            "events",
            "show",
            &selector,
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--json",
        ])
        .await;
    assert!(shown.status.success());
    let row: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(row["interrupt"], true);
    assert!(
        row.get("interrupt_attempt").is_none_or(Value::is_null),
        "an already-armed local hook needs no detached interrupt helper"
    );
    let ack = fixture
        .run(&[
            "events",
            "ack",
            &selector,
            "--repo",
            fixture.repo.to_str().unwrap(),
        ])
        .await;
    assert!(
        !ack.status.success(),
        "wake receipt must not satisfy required reply"
    );
    assert!(
        String::from_utf8_lossy(&ack.stderr).contains("requires a reply"),
        "{}",
        String::from_utf8_lossy(&ack.stderr)
    );
    fixture.assert_owed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_start_notifies_legacy_backlog_only_after_actual_context_emission() {
    let mut fixture = Fixture::new(Tool::Claude, false);
    let _lease = fixture.monitor_lease();
    let wait = fixture
        .command()
        .args([
            "wait",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--author",
            fixture.author.as_str(),
            "--json",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let ready = fixture.producer_ready().await;
    fixture.emit_event();
    let output = tokio::time::timeout(LIMIT, wait.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.output.exists());
    fixture.enable_receipts();
    let peek = fixture
        .run(&[
            "wait",
            "--peek",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--author",
            fixture.author.as_str(),
            "--json",
        ])
        .await;
    assert!(
        peek.status.success(),
        "peek {}: {}",
        peek.status,
        String::from_utf8_lossy(&peek.stderr)
    );
    let selector = fixture.assert_owed().await;
    let show = fixture
        .run(&[
            "events",
            "show",
            &selector,
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--json",
        ])
        .await;
    assert!(show.status.success());
    assert!(
        !fixture.output.exists(),
        "peek/list/show cannot emit receipts"
    );
    let mut unexpected_ready = [0u8; 4096];
    assert_eq!(
        fixture
            .ready
            .try_recv(&mut unexpected_ready)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::WouldBlock,
        "inspection must not start another producer"
    );
    let hook = fixture.hook_child(true).await;
    let output = tokio::time::timeout(LIMIT, hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        payload["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("do the task")
    );
    assert!(output.stderr.is_empty());
    fixture.assert_receipt(&ready);
    fixture.assert_owed().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_monitor_recurring_timer_watches_config_and_ack_then_rearms_native_deadline() {
    let fixture = Fixture::new(Tool::Codex, false);
    clank::agent_store::update_agent_config(&fixture.repo, &fixture.author, |cfg| {
        cfg.wait_events.clear();
    })
    .unwrap();
    let mut monitor = fixture
        .command()
        .args(["monitor", "start", "--repo", fixture.repo.to_str().unwrap()])
        .env("RUST_LOG", "clank::cli::monitor=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(monitor.stdout.take().unwrap()).lines();
    assert!(
        tokio::time::timeout(LIMIT, stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .contains("listening on")
    );
    let mut stderr = BufReader::new(monitor.stderr.take().unwrap()).lines();
    let first_hook = fixture.hook_child(false).await;
    async fn admitted(lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStderr>>) {
        tokio::time::timeout(LIMIT, async {
            while let Some(line) = lines.next_line().await.unwrap() {
                if line.contains("monitor agent wait admitted") {
                    return;
                }
            }
            panic!("monitor closed before admission");
        })
        .await
        .expect("waiter must attach to shared monitor");
    }
    admitted(&mut stderr).await;
    let anchor = time::OffsetDateTime::now_utc() - time::Duration::seconds(100);
    let anchor = anchor.replace_nanosecond(0).unwrap();
    clank::agent_store::update_agent_config(&fixture.repo, &fixture.author, |cfg| {
        cfg.wait_events = vec![WaitEventSource::Timer(
            clank_core::agent_config::TimerSource {
                interrupt: false,
                name: "recurring-monitor".into(),
                due_at: anchor
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
                repeat_every: Some("10s".into()),
                content: Some("recurring work".into()),
            },
        )];
    })
    .unwrap();
    let first = tokio::time::timeout(LIMIT, first_hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert!(first["reason"].as_str().unwrap().contains("recurring work"));
    let list = fixture
        .run(&[
            "events",
            "list",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--json",
        ])
        .await;
    assert!(list.status.success());
    let rows: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1, "offline slots coalesce");
    let row = &rows[0];
    let previous_last = row["item"]["recurrence"]["last_slot"].as_u64().unwrap();
    assert!(row["item"]["recurrence"]["count"].as_u64().unwrap() >= 11);
    let selector = row["id"].as_str().unwrap();
    let key = row["source"].as_str().unwrap();
    let wal = clank::agent_store::agents_root(&fixture.repo)
        .join("codex/events")
        .join(format!("{key}.jsonl"));
    let accepted_bytes = std::fs::read(&wal).unwrap();
    let replay = fixture.hook_child(false).await;
    let replay = tokio::time::timeout(LIMIT, replay.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(replay.status.success());
    assert_eq!(
        std::fs::read(&wal).unwrap(),
        accepted_bytes,
        "pending replay appends nothing"
    );
    let ack = fixture
        .run(&[
            "events",
            "ack",
            "--repo",
            fixture.repo.to_str().unwrap(),
            selector,
        ])
        .await;
    assert!(
        ack.status.success(),
        "{}",
        String::from_utf8_lossy(&ack.stderr)
    );
    let second_hook = fixture.hook_child(false).await;
    admitted(&mut stderr).await;
    let second = tokio::time::timeout(LIMIT, second_hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert!(
        second["reason"]
            .as_str()
            .unwrap()
            .contains("recurring work")
    );
    let list = fixture
        .run(&[
            "events",
            "list",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--json",
        ])
        .await;
    let rows: Value = serde_json::from_slice(&list.stdout).unwrap();
    let pending: Vec<_> = rows
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["acked"] == false)
        .collect();
    assert_eq!(pending.len(), 1);
    assert!(
        pending[0]["item"]["recurrence"]["first_slot"]
            .as_u64()
            .unwrap()
            > previous_last
    );
    assert_ne!(pending[0]["id"].as_str().unwrap(), selector);
    assert!(
        !fixture.output.exists(),
        "timer delivery has no command receipt effect"
    );
    let descriptor: Value = serde_json::from_slice(
        &std::fs::read(runtime_dir().join(format!("{}.json", repo_tag(&fixture.repo)))).unwrap(),
    )
    .unwrap();
    let mut socket = tokio::net::UnixStream::connect(descriptor["socket"].as_str().unwrap())
        .await
        .unwrap();
    let greeting = json!({"proto":u32::MAX,"repo":fixture.repo,"who":{"as":"observer"}});
    socket
        .write_all(format!("{greeting}\n").as_bytes())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(LIMIT, monitor.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_monitor_delivery_preserves_monitor_cwd_and_does_not_own_receipts() {
    let mut fixture = Fixture::new(Tool::Codex, true);
    let mut monitor = fixture
        .command()
        .args(["monitor", "start", "--repo", fixture.repo.to_str().unwrap()])
        .current_dir(&fixture.host_cwd)
        .env("RUST_LOG", "clank::cli::monitor=debug")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(monitor.stdout.take().unwrap()).lines();
    let listening = tokio::time::timeout(LIMIT, stdout.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(listening.contains("listening on"));
    let ready = fixture.producer_ready().await;
    assert_eq!(ready["parent"].as_u64(), monitor.id().map(u64::from));
    assert_eq!(ready["cwd"], fixture.host_cwd.to_str().unwrap());
    let hook = fixture.hook_child(false).await;
    let mut stderr = BufReader::new(monitor.stderr.take().unwrap()).lines();
    tokio::time::timeout(LIMIT, async {
        loop {
            let line = stderr
                .next_line()
                .await
                .unwrap()
                .expect("monitor closed before admission");
            if line.contains("monitor agent wait admitted") {
                break;
            }
        }
    })
    .await
    .expect("hook did not attach to the monitor");
    assert!(
        !fixture.output.exists(),
        "monitor admission is not a receipt"
    );
    fixture.emit_event();
    let output = tokio::time::timeout(LIMIT, hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(payload["reason"].as_str().unwrap().contains("do the task"));
    fixture.assert_receipt(&ready);
    fixture.assert_owed().await;
    // A newer protocol greeting exercises normal owned-source retirement.
    let descriptor: Value = serde_json::from_slice(
        &std::fs::read(runtime_dir().join(format!("{}.json", repo_tag(&fixture.repo)))).unwrap(),
    )
    .unwrap();
    let mut socket = tokio::net::UnixStream::connect(descriptor["socket"].as_str().unwrap())
        .await
        .unwrap();
    let greeting = json!({"proto":u32::MAX,"repo":fixture.repo,"who":{"as":"observer"}});
    socket
        .write_all(format!("{greeting}\n").as_bytes())
        .await
        .unwrap();
    let exit = tokio::time::timeout(LIMIT, monitor.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(exit.success());
    assert_eq!(
        clank::signal::probe_process(ready["pid"].as_i64().unwrap() as i32),
        clank::signal::Probe::Gone
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn receipt_does_not_retire_work_or_redirect_a_successor_masters_subscription() {
    let mut fixture = Fixture::new(Tool::Codex, true);
    let _lease = fixture.monitor_lease();
    let hook = fixture.hook_child(false).await;
    let first = fixture.producer_ready().await;
    fixture.emit_event();
    let output = tokio::time::timeout(LIMIT, hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    fixture.assert_receipt(&first);
    fixture.assert_owed().await;
    let previous_author = fixture.author.clone();
    let previous_state = first["state"].clone();
    let successor = AgentLabel::parse("successor").unwrap();
    clank::cli::agent::add_repo_roster_agent(
        &fixture.repo,
        &successor,
        AgentDescription {
            tool: Tool::Codex,
            launch: None,
            initial_prompt: None,
        },
        RosterRole::Commit,
    )
    .unwrap();
    clank::cli::agent::set_repo_master(&fixture.repo, &successor).unwrap();
    let cfg = AgentConfig {
        auto_mode: Some(AutoMode::On),
        wait_events: vec![WaitEventSource::Command(fixture.source.clone())],
        ..AgentConfig::default()
    };
    clank::agent_store::save_agent_config(&fixture.repo, &successor, &cfg).unwrap();
    clank::agent_store::bind_session_to_agent(
        &fixture.repo,
        &successor,
        Tool::Codex,
        &SessionId::parse(SESSION).unwrap(),
    )
    .unwrap();
    fixture.author = successor;
    let hook = fixture.hook_child(false).await;
    let second = fixture.producer_ready().await;
    assert_ne!(second["state"], previous_state);
    fixture.emit_event(); // Backend still owes and replays the original producer ID.
    let output = tokio::time::timeout(LIMIT, hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    fixture.assert_receipt(&second);
    fixture.assert_owed().await;
    let previous = fixture
        .run(&[
            "events",
            "list",
            "--author",
            previous_author.as_str(),
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--json",
        ])
        .await;
    assert!(previous.status.success());
    let rows: Value = serde_json::from_slice(&previous.stdout).unwrap();
    assert_eq!(rows[0]["item"]["id"], ID);
    assert_eq!(rows[0]["acked"], false);
    assert!(rows[0].get("reply").is_none());
}

#[tokio::test]
async fn shared_monitor_claims_due_interrupt_without_waiter_and_does_not_probe_on_beats() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new(Tool::Codex, false);
    let source = clank_core::agent_config::TimerSource {
        interrupt: true,
        name: "interrupt-without-waiter".into(),
        due_at: "1970-01-01T00:01:40Z".into(),
        repeat_every: None,
        content: Some("durable task".into()),
    };
    clank::agent_store::update_agent_config(&fixture.repo, &fixture.author, |cfg| {
        cfg.wait_events = vec![WaitEventSource::Timer(source.clone())];
    })
    .unwrap();
    let bin = fixture.home.join("probe-bin");
    std::fs::create_dir(&bin).unwrap();
    let shim = bin.join("zellij");
    std::fs::write(&shim, "#!/bin/sh\nprintf 'query\\n' >> \"$CLANK_TEST_QUERY_COUNT\"\nprintf 'No active zellij sessions found.\\n' >&2\nexit 1\n").unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let count = fixture.home.join("queries");
    let mut monitor = fixture
        .command()
        .args(["monitor", "start", "--repo", fixture.repo.to_str().unwrap()])
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display()),
        )
        .env("CLANK_TEST_QUERY_COUNT", &count)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(monitor.stdout.take().unwrap()).lines();
    assert!(
        tokio::time::timeout(LIMIT, stdout.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .contains("listening on")
    );
    let mut stderr = BufReader::new(monitor.stderr.take().unwrap()).lines();
    tokio::time::timeout(LIMIT, async {
        while let Some(line) = stderr.next_line().await.unwrap() {
            if line.contains("Some(Unavailable) for 1 event(s)") {
                return;
            }
        }
        panic!("monitor ended before due interruption was attempted");
    })
    .await
    .expect("due work must initiate interruption without a waiter");
    let listed = fixture
        .run(&[
            "events",
            "list",
            "--repo",
            fixture.repo.to_str().unwrap(),
            "--json",
        ])
        .await;
    assert!(listed.status.success());
    let rows: Value = serde_json::from_slice(&listed.stdout).unwrap();
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["item"]["name"] == source.name)
        .unwrap();
    assert_eq!(row["interrupt"], true);
    assert_eq!(row["acked"], false);
    assert_eq!(row["interrupt_attempt"]["outcome"], "unavailable");
    assert!(
        !fixture.output.exists(),
        "attempts cannot fabricate receipts"
    );
    // Cross a normal monitor heartbeat, including the claim's own FS invalidation.
    // This one-shot test delay measures cost; production has no retry timer.
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert_eq!(
        std::fs::read_to_string(&count).unwrap(),
        "query\n",
        "idle beats and claim invalidations cannot retry transport discovery"
    );
    monitor.start_kill().unwrap();
    monitor.wait().await.unwrap();
}
