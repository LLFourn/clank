//! The exec boundary needs a subprocess: in-process composers cannot prove
//! the native hooks and ordinary wait command work in the launched env.
//! Use the existing CLI binary, one integration harness, empty temporary homes,
//! and a deterministic provider transport. No terminals, models or credentials.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use clank::cli::teams_config::{AgentDescription, RosterRole};
use clank_core::agent_config::LaunchConfig;
use clank_core::ids::{AgentLabel, SessionId};
use clank_core::wait::WaitItem;
use clank_core::{AutoMode, Tool};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::common::TestEnv;

const SOURCE: &str = "11111111-1111-4111-8111-111111111111";
const RESUME: &str = "22222222-2222-4222-8222-222222222222";
const LIMIT: Duration = Duration::from_secs(40);

fn label() -> AgentLabel {
    AgentLabel::parse("grok").unwrap()
}

struct Fixture {
    env: TestEnv,
    root: PathBuf,
    home: PathBuf,
    grok: PathBuf,
    path: String,
    source_binding: Option<Vec<u8>>,
    source_summary: Option<Vec<u8>>,
}

impl Fixture {
    fn new() -> Self {
        let env = TestEnv::init();
        let root = env.repo().canonicalize().unwrap();
        let home = env.home().canonicalize().unwrap();
        let grok = home.join("configured Grok home");
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_clank"), bin.join("clank")).unwrap();
        let script = home.join("provider.py");
        std::fs::write(&script, include_str!("fixtures/grok_provider.py")).unwrap();
        let mut profile_env = BTreeMap::from([
            ("HOME".into(), home.to_string_lossy().into_owned()),
            ("GROK_HOME".into(), grok.to_string_lossy().into_owned()),
        ]);
        for key in clank::agent_env::SESSION_IDENTITY_VARS {
            profile_env.insert((*key).into(), "foreign-profile".into());
        }
        clank::cli::agent::add_repo_roster_agent(
            &root,
            &label(),
            AgentDescription {
                tool: Tool::Grok,
                initial_prompt: None,
                launch: Some(LaunchConfig {
                    command: Some("/usr/bin/python3".into()),
                    args: vec![
                        script.to_string_lossy().into_owned(),
                        "--fixture-profile".into(),
                    ],
                    env: profile_env,
                }),
            },
            RosterRole::Master,
        )
        .unwrap();
        clank::agent_store::set_auto_mode(&root, &label(), AutoMode::On).unwrap();
        let result = std::process::Command::new("git")
            .current_dir(&root)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "fixture",
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let path = format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin.display());
        Self {
            env,
            root,
            home,
            grok,
            path,
            source_binding: None,
            source_summary: None,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_clank"));
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("TERM", "dumb")
            .current_dir(&self.root)
            .kill_on_drop(true);
        for key in clank::agent_env::SESSION_IDENTITY_VARS {
            command.env(key, "foreign-parent");
        }
        command
    }
    async fn run(&self, args: &[&str]) -> std::process::Output {
        let out = tokio::time::timeout(LIMIT, self.command().args(args).output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    }
    fn summary_path(&self, repo: &Path, sid: &str) -> PathBuf {
        // Native URI encoding is part of the provider fixture's durable store.
        let encoded = repo
            .to_string_lossy()
            .bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>();
        self.grok
            .join("sessions")
            .join(encoded)
            .join(sid)
            .join("summary.json")
    }
    fn bind(&self, sid: &str) {
        clank::agent_store::bind_session_to_agent(
            &self.root,
            &label(),
            Tool::Grok,
            &SessionId::parse(sid).unwrap(),
        )
        .unwrap();
        let summary = self.summary_path(&self.root, sid);
        std::fs::create_dir_all(summary.parent().unwrap()).unwrap();
        std::fs::write(
            summary,
            serde_json::to_vec(&json!({"info":{"id":sid,"cwd":self.root}})).unwrap(),
        )
        .unwrap();
    }
    async fn setup(&self) {
        let out = self
            .run(&[
                "setup",
                "--grok-agent",
                "grok",
                "--repo",
                self.root.to_str().unwrap(),
            ])
            .await;
        assert!(String::from_utf8_lossy(&out.stdout).contains("advertised Stop=true"));
        for role in ["clank-master", "clank-reviewer", "clank-standby"] {
            let body =
                std::fs::read_to_string(self.grok.join("skills").join(role).join("SKILL.md"))
                    .unwrap();
            assert!(body.contains("clank wait"));
            assert!(body.contains("block_until_ms: 0"));
        }
        assert!(
            !self.home.join(".grok/hooks/clank.json").exists(),
            "custom setup must not install a misleading default adapter"
        );
    }
    fn rows(&self) -> Vec<Value> {
        std::fs::read_to_string(self.grok.join("audit.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    async fn preview_is_read_only(&self) {
        let audit = std::fs::read(self.grok.join("audit.jsonl")).ok();
        let cfg =
            std::fs::read(clank::agent_store::agent_config_path(&self.root, &label())).unwrap();
        let intent = std::fs::read(clank::cli::fork::fork_spec_path(&self.root, &label())).ok();
        self.run(&[
            "agent",
            "start",
            "grok",
            "--repo",
            self.root.to_str().unwrap(),
            "--print",
        ])
        .await;
        assert_eq!(std::fs::read(self.grok.join("audit.jsonl")).ok(), audit);
        assert_eq!(
            std::fs::read(clank::agent_store::agent_config_path(&self.root, &label())).unwrap(),
            cfg
        );
        assert_eq!(
            std::fs::read(clank::cli::fork::fork_spec_path(&self.root, &label())).ok(),
            intent
        );
    }
    fn deliver(&self, cycle: usize) -> String {
        clank::inbox::deliver(
            &self.root,
            &label(),
            None,
            "2026-10-03T00:00:00Z",
            &format!("fixture cycle {cycle}"),
        )
        .unwrap()
    }
    fn pending(&self, handle: &str) -> Option<String> {
        clank::inbox::pending(&self.root, &label())
            .into_iter()
            .find_map(|item| match item {
                WaitItem::Message {
                    handle: id,
                    selector,
                    ..
                } if id == handle => selector,
                _ => None,
            })
    }
    async fn actor(&self) -> Actor {
        let error = self.home.join("interactive.stderr");
        let log = std::fs::File::create(&error).unwrap();
        let mut child = self
            .command()
            .args([
                "agent",
                "start",
                "grok",
                "--repo",
                self.root.to_str().unwrap(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .process_group(0)
            .spawn()
            .unwrap();
        let group = clank::signal::Pgid::new(child.id().unwrap() as i32).unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Actor {
            child,
            input,
            output,
            error,
            group: Some(group),
        }
    }
    fn monitor_descriptor(&self) -> Option<Value> {
        let runtime = PathBuf::from(format!("/tmp/clank-run-{}", unsafe { libc::getuid() }));
        let tag = blake3::hash(self.root.as_os_str().as_encoded_bytes()).to_hex()[..32].to_owned();
        let value: Value =
            serde_json::from_slice(&std::fs::read(runtime.join(format!("{tag}.json"))).ok()?)
                .ok()?;
        assert_eq!(value["repo"].as_str(), self.root.to_str());
        Some(value)
    }
    fn assert_source_unchanged(&self) {
        if let Some(binding) = &self.source_binding {
            assert_eq!(
                &std::fs::read(clank::agent_store::agent_config_path(
                    self.env.repo(),
                    &label()
                ))
                .unwrap(),
                binding
            );
            assert_eq!(
                &std::fs::read(self.summary_path(&self.env.repo().canonicalize().unwrap(), SOURCE))
                    .unwrap(),
                self.source_summary.as_ref().unwrap()
            );
        }
    }
}

struct Actor {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    error: PathBuf,
    group: Option<clank::signal::Pgid>,
}
impl Actor {
    async fn next(&mut self, step: &str) -> Value {
        let mut line = String::new();
        let n = tokio::time::timeout(LIMIT, self.output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert!(
            n > 0,
            "provider ended before {step}: {}",
            std::fs::read_to_string(&self.error).unwrap()
        );
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["step"], step, "{value}");
        value
    }
    async fn send(&mut self, value: Value) {
        self.input
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        self.input.flush().await.unwrap();
    }
    async fn end(mut self) {
        let status = tokio::time::timeout(LIMIT, self.child.wait())
            .await
            .unwrap()
            .unwrap();
        self.group = None;
        assert!(
            status.success(),
            "{}",
            std::fs::read_to_string(&self.error).unwrap()
        );
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        if let Some(group) = self.group.take() {
            clank::signal::signal_group(group, libc::SIGKILL);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Only this fixture repo's descriptor and slot. No process listing or
        // global socket sweep; the monitor uses its own process group.
        let runtime = PathBuf::from(format!("/tmp/clank-run-{}", unsafe { libc::getuid() }));
        let tag = blake3::hash(self.root.as_os_str().as_encoded_bytes()).to_hex()[..32].to_owned();
        let descriptor = runtime.join(format!("{tag}.json"));
        if let Ok(bytes) = std::fs::read(&descriptor) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                if value["repo"].as_str() == self.root.to_str() {
                    if let Some(pid) = value["pid"]
                        .as_i64()
                        .and_then(|p| i32::try_from(p).ok())
                        .and_then(clank::signal::Pid::new)
                    {
                        clank::signal::signal_process(pid, libc::SIGTERM);
                        if let Some(socket) = value["socket"].as_str() {
                            let socket = Path::new(socket);
                            if socket.file_name().and_then(|n| n.to_str())
                                == Some(format!("{pid}.sock").as_str())
                            {
                                let _ = std::fs::remove_file(socket);
                            }
                        }
                    }
                }
            }
        }
        // These inodes belong to a discarded fixture path, which is never
        // relaunched. Production lock inodes remain untouched.
        for extension in ["json", "lock", "repair"] {
            let _ = std::fs::remove_file(runtime.join(format!("{tag}.{extension}")));
        }
    }
}

async fn cycles(f: &Fixture, expected: Option<&str>) {
    f.setup().await;
    cycles_after_setup(f, expected, 2).await;
}

async fn cycles_after_setup(f: &Fixture, expected: Option<&str>, expected_probes: usize) {
    f.preview_is_read_only().await;
    let mut pending = f.deliver(0);
    let mut actor = f.actor().await;
    let bound = actor.next("bound").await;
    assert_eq!(bound["cwd"], f.root.to_string_lossy().as_ref());
    let sid = bound["session"].as_str().unwrap();
    if let Some(expected) = expected {
        assert_eq!(sid, expected);
    }
    let cfg = clank::agent_store::load_agent_config(&f.root, &label())
        .unwrap()
        .unwrap();
    assert_eq!(cfg.session.unwrap().id.as_str(), sid);
    let summary: Value =
        serde_json::from_slice(&std::fs::read(f.summary_path(&f.root, sid)).unwrap()).unwrap();
    assert_eq!(summary["info"]["cwd"], f.root.to_string_lossy().as_ref());
    assert!(
        clank::cli::fork::load_fork_spec(&f.root, &label())
            .unwrap()
            .is_none()
    );
    actor.send(json!({"action":"begin","cycles":10})).await;
    let mut monitor = None;
    for cycle in 0..10 {
        let started = actor.next("started").await;
        assert_eq!(started["cycle"], cycle);
        let words = shell_words::split(started["command"].as_str().unwrap()).unwrap();
        assert_eq!(&words[..2], &["clank", "wait"]);
        if cycle % 2 == 0 {
            assert_eq!(&words[2..], &["--repo", f.root.to_str().unwrap()]);
        } else {
            assert_eq!(words.len(), 2);
        }
        // A pending event remains unhandled, even while a process was armed.
        if cycle == 0 {
            assert!(f.pending(&pending).is_some());
        }
        let registered = actor.next("waiting").await;
        assert_ne!(registered["stop"]["decision"], "block");
        if cycle > 0 {
            assert_eq!(
                registered["worker_running"], true,
                "idle worker must remain running with no pending work"
            );
            let current = f
                .monitor_descriptor()
                .expect("accepted idle subscription must have a published monitor");
            if let Some(previous) = &monitor {
                assert_eq!(&current, previous, "re-arming must reuse one monitor");
            } else {
                monitor = Some(current.clone());
            }
            if cycle == 4 {
                // This descriptor is checked against the fixture's canonical
                // repository. Kill only its monitor while the wait is parked:
                // the same process must reconnect and catch the persisted event.
                let pid = current["pid"]
                    .as_i64()
                    .and_then(|p| i32::try_from(p).ok())
                    .and_then(clank::signal::Pid::new)
                    .unwrap();
                clank::signal::signal_process(pid, libc::SIGTERM);
                monitor = None;
            }
            pending = f.deliver(cycle);
        }
        let completed = actor.next("completed").await;
        assert!(
            completed["output"].as_str().unwrap().contains(&pending),
            "{completed}"
        );
        let selector = f
            .pending(&pending)
            .expect("delivery must not acknowledge work");
        let listed = f
            .run(&["events", "list", "--author", "grok", "--json"])
            .await;
        let metadata: Vec<Value> = serde_json::from_slice(&listed.stdout).unwrap();
        let delivered = metadata
            .iter()
            .find(|row| row["item"]["handle"] == pending)
            .expect("the delivered event must remain listed");
        assert!(delivered["woken_at"].is_number(), "{delivered}");
        assert_eq!(delivered["acked"], false);
        actor
            .send(json!({"action":"react","selector":selector}))
            .await;
        actor.next("reacted").await;
        assert!(f.pending(&pending).is_none());
        f.assert_source_unchanged();
    }
    actor.next("ended").await;
    actor.end().await;
    let rows = f.rows();
    assert!(
        rows.iter()
            .filter(|r| r["kind"] == "hook")
            .all(|r| matches!(
                r["event"].as_str(),
                Some("SessionStart" | "Stop" | "SessionEnd")
            ))
    );
    for start in rows.iter().filter(|r| {
        r["kind"] == "start"
            && r["argv"].as_array().unwrap().last().and_then(Value::as_str) == Some("stdio")
    }) {
        assert!(
            start["identity"]
                .as_object()
                .unwrap()
                .values()
                .all(Value::is_null)
        );
        let pid = i32::try_from(start["pid"].as_i64().unwrap()).unwrap();
        assert_eq!(
            clank::signal::probe_process(pid),
            clank::signal::Probe::Gone,
            "no ACP helper may outlive launch"
        );
    }
    let calls: Vec<_> = rows
        .iter()
        .filter(|r| r["kind"] == "rpc")
        .map(|r| r["request"]["method"].as_str().unwrap())
        .collect();
    assert!(
        calls
            .iter()
            .all(|m| matches!(*m, "initialize" | "_x.ai/session/fork")),
        "{calls:?}"
    );
    let copied = calls.contains(&"_x.ai/session/fork");
    assert_eq!(
        calls.iter().filter(|m| **m == "initialize").count(),
        expected_probes + usize::from(copied),
        "capabilities are probed only during setup/launch, never per idle turn"
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["kind"] == "start"
                && r["argv"].as_array().unwrap().last().and_then(Value::as_str) != Some("stdio"))
            .count(),
        1
    );
}

#[tokio::test]
async fn fresh_launch_delivers_pending_work_and_ten_background_completion_turns() {
    let f = Fixture::new();
    cycles(&f, None).await;
    assert!(
        !f.rows()
            .iter()
            .any(|r| r["request"]["method"] == "_x.ai/session/fork")
    );
}

#[tokio::test]
async fn missing_setup_is_repaired_before_native_grok_launch_and_wait() {
    let f = Fixture::new();
    assert!(!f.grok.join("hooks/clank.json").exists());
    cycles_after_setup(&f, None, 1).await;
    assert!(f.grok.join("hooks/clank.json").exists());
    assert!(
        !f.home.join(".codex/hooks.json").exists(),
        "only the selected provider is installed"
    );
}

#[tokio::test]
async fn resume_launch_binds_same_session_and_rearms_after_each_completion() {
    let f = Fixture::new();
    f.bind(RESUME);
    cycles(&f, Some(RESUME)).await;
    assert!(
        !f.rows()
            .iter()
            .any(|r| r["request"]["method"] == "_x.ai/session/fork")
    );
}

#[tokio::test]
async fn fork_launch_delivers_only_in_child_repo_and_never_rebinds_source() {
    let mut f = Fixture::new();
    f.bind(SOURCE);
    f.source_binding =
        Some(std::fs::read(clank::agent_store::agent_config_path(&f.root, &label())).unwrap());
    f.source_summary = Some(std::fs::read(f.summary_path(&f.root, SOURCE)).unwrap());
    let dest = clank::cli::fork::run_fork(
        &clank::cli::ForkCreateArgs {
            name: Some("native-child".into()),
            fresh: false,
            source: Some(f.root.clone()),
            pr: None,
            branch: None,
            path: None,
            clone: false,
            team: None,
            drafts: vec![],
            prompt: None,
            no_open: true,
        },
        Some(&f.home),
    )
    .await
    .unwrap();
    f.root = dest.canonicalize().unwrap();
    let child = clank::cli::fork::load_fork_spec(&f.root, &label())
        .unwrap()
        .unwrap()
        .grok
        .unwrap()
        .child_id;
    assert_ne!(child, SOURCE);
    cycles(&f, Some(&child)).await;
    assert_eq!(
        f.rows()
            .iter()
            .filter(|r| r["kind"] == "rpc" && r["request"]["method"] == "_x.ai/session/fork")
            .count(),
        1
    );
    let summary: Value =
        serde_json::from_slice(&std::fs::read(f.summary_path(&f.root, &child)).unwrap()).unwrap();
    assert_eq!(summary["parent_session_id"], SOURCE);
    f.assert_source_unchanged();
}
