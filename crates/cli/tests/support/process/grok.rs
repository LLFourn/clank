//! A deterministic transport, not a model: exercise installed hooks and real
//! background waits while the Rust test drives delivery through JSON pipes.
use super::{emit, env};
use anyhow::{Context, Result, ensure};
use clank::proc_run::{self, Out, Spec, Stdin};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const LIMIT: Duration = Duration::from_secs(20);
const IDENTITIES: &[&str] = clank::agent_env::SESSION_IDENTITY_VARS;

fn audit(root: &Path, value: Value) -> Result<()> {
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(root.join("audit.jsonl"))?;
    serde_json::to_writer(&mut out, &value)?;
    writeln!(out)?;
    Ok(())
}

fn summary(root: &Path, cwd: &str, sid: &str) -> PathBuf {
    let encoded: String = cwd
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect();
    root.join("sessions")
        .join(encoded)
        .join(sid)
        .join("summary.json")
}

fn save(root: &Path, cwd: &str, sid: &str, parent: Option<&str>) -> Result<()> {
    let path = summary(root, cwd, sid);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        path,
        serde_json::to_vec(&json!({"info":{"id":sid,"cwd":cwd},"parent_session_id":parent}))?,
    )?;
    Ok(())
}

fn receive(expected: &str) -> Result<Value> {
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let value: Value = serde_json::from_str(&line)?;
    ensure!(value["action"] == expected, "expected {expected}: {value}");
    Ok(value)
}

fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .with_context(|| format!("missing {name}: {value}"))
}

fn rpc(root: &Path) -> Result<()> {
    let mode = std::env::var("FIXTURE_RPC_MODE").unwrap_or_else(|_| "good".into());
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        audit(root, json!({"kind":"rpc","request":request}))?;
        match mode.as_str() {
            "hang" => loop {
                std::thread::park();
            },
            "flood" => {
                std::io::stdout().write_all(&vec![b'x'; 1_100_000])?;
                std::io::stdout().flush()?;
                loop {
                    std::thread::park();
                }
            }
            "stderr" => {
                std::io::stderr().write_all(&vec![b'e'; 150_000])?;
                std::io::stderr().flush()?;
            }
            "malformed" => {
                println!("not JSON");
                continue;
            }
            "unsupported" => {
                emit(
                    &json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32601,"message":"method not found"}}),
                )?;
                continue;
            }
            _ => {}
        }
        let result = if request["method"] == "initialize" {
            json!({"protocolVersion":if mode == "version" {2} else {1},"_meta":{"agentVersion":"1.0.46"},"agentCapabilities":{"_meta":{"x.ai/hooks":{"blockingEvents":["pre_tool_use","stop"]}}}})
        } else if request["method"] == "fixture/echo" {
            request["params"].clone()
        } else {
            ensure!(request["method"] == "_x.ai/session/fork");
            let p = &request["params"];
            let original: Value = serde_json::from_slice(&std::fs::read(summary(
                root,
                text(p, "sourceCwd")?,
                text(p, "sourceSessionId")?,
            ))?)?;
            ensure!(original["info"]["cwd"] == p["sourceCwd"]);
            save(
                root,
                text(p, "newCwd")?,
                text(p, "newSessionId")?,
                Some(text(p, "sourceSessionId")?),
            )?;
            json!({"newSessionId":p["newSessionId"],"newCwd":p["newCwd"],"parentSessionId":p["sourceSessionId"]})
        };
        let id = if mode == "wrong_id" {
            json!(request["id"].as_u64().unwrap() + 1)
        } else {
            request["id"].clone()
        };
        emit(&json!({"jsonrpc":"2.0","id":id,"result":result}))?;
    }
    Ok(())
}

struct Native {
    root: PathBuf,
    cwd: String,
    sid: String,
    hooks: Value,
}

impl Native {
    fn command(&self, args: &[&str]) -> Spec {
        Spec::new("clank")
            .args(args)
            .env("GROK_SESSION_ID", &self.sid)
            .env("GROK_AGENT", "1")
    }

    async fn hook(&self, event: &str, turn: &str, extra: Value) -> Result<Value> {
        let native = match event {
            "SessionStart" => "session_start",
            "SessionEnd" => "session_end",
            "Stop" => "stop",
            _ => unreachable!(),
        };
        let matches = self.hooks[event]
            .as_array()
            .context("missing hook event")?
            .iter()
            .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
            .filter(|entry| entry["command"] == "clank stop-hook --tool grok")
            .count();
        ensure!(matches == 1, "installed hook is missing or duplicated");
        let mut payload = json!({"hookEventName":native,"sessionId":self.sid,"cwd":self.cwd,"promptId":turn,"reason":"end_turn"});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let spec = self
            .command(&["stop-hook", "--tool", "grok"])
            .env("GROK_HOOK_EVENT", native)
            .stdin(Stdin::Piped)
            .stdout(Out::Piped)
            .stderr(Out::Piped);
        let mut child = proc_run::start(spec)?;
        let mut input = child.take_stdin_async().unwrap();
        let mut output = child.take_stdout_async().unwrap();
        let mut error = child.take_stderr_async().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let status = tokio::time::timeout(LIMIT, async {
            let write = async {
                input.write_all(&serde_json::to_vec(&payload)?).await?;
                drop(input);
                Ok::<_, anyhow::Error>(())
            };
            let (_, _, _, status) = tokio::try_join!(
                write,
                async { Ok::<_, anyhow::Error>(output.read_to_end(&mut out).await?) },
                async { Ok::<_, anyhow::Error>(error.read_to_end(&mut err).await?) },
                async { Ok::<_, anyhow::Error>(child.wait_async().await?) }
            )?;
            Ok::<_, anyhow::Error>(status)
        })
        .await??;
        ensure!(
            status.success() && err.is_empty(),
            "hook failed: {}",
            String::from_utf8_lossy(&err)
        );
        let value = if out.iter().all(u8::is_ascii_whitespace) {
            json!({})
        } else {
            serde_json::from_slice(&out)?
        };
        audit(
            &self.root,
            json!({"kind":"hook","event":event,"turn":turn,"output":value}),
        )?;
        Ok(value)
    }

    async fn interactive(&self, args: &[String]) -> Result<()> {
        self.hook("SessionStart", "0", json!({})).await?;
        let first = self
            .hook("Stop", "0", json!({"backgroundTasks":[]}))
            .await?;
        ensure!(first["decision"] == "block", "{first}");
        emit(&json!({"step":"bound","session":self.sid,"cwd":self.cwd,"argv":args,"stop":first}))?;
        let total = receive("begin")?["cycles"]
            .as_u64()
            .context("missing cycles")?;
        for cycle in 0..total {
            let mut args = vec!["wait"];
            if cycle % 2 == 0 {
                args.extend(["--repo", &self.cwd]);
            }
            let command = shell_words::join(std::iter::once("clank").chain(args.iter().copied()));
            let spec = self
                .command(&args)
                .env("RUST_LOG", "clank::cli::wait=debug")
                .stdout(Out::Piped)
                .stderr(Out::Piped);
            // Managed drop kills and reaps on any assertion or deadline failure.
            let mut worker = proc_run::start(spec)?;
            let mut stdout = worker.take_stdout_async().unwrap();
            let mut stderr = BufReader::new(worker.take_stderr_async().unwrap());
            emit(&json!({"step":"started","cycle":cycle,"command":command,"pid":worker.pid()}))?;
            let turn = cycle.to_string();
            let stopped = self.hook("Stop", &turn, json!({"stopHookActive":true,"backgroundTasks":[{"id":format!("task-{turn}"),"type":"shell","status":"running","command":command}]})).await?;
            ensure!(stopped["decision"] != "block", "{stopped}");
            let mut error = String::new();
            if cycle > 0 {
                tokio::time::timeout(LIMIT, stderr.read_line(&mut error)).await??;
                ensure!(error.contains("monitor subscription registered"), "{error}");
            }
            emit(
                &json!({"step":"waiting","cycle":cycle,"stop":stopped,"worker_running":worker.try_wait()?.is_none()}),
            )?;
            let mut output = String::new();
            let status = tokio::time::timeout(LIMIT, async {
                let (_, _, status) = tokio::try_join!(
                    stdout.read_to_string(&mut output),
                    stderr.read_to_string(&mut error),
                    worker.wait_async()
                )?;
                Ok::<_, std::io::Error>(status)
            })
            .await??;
            ensure!(status.success(), "{error}");
            emit(&json!({"step":"completed","cycle":cycle,"output":output,"error":error}))?;
            let ack = receive("react")?;
            let result = proc_run::run(
                self.command(&["events", "ack", text(&ack, "selector")?])
                    .stderr(Out::Capture { cap: 65536 }),
                LIMIT,
            )
            .await?;
            ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            emit(&json!({"step":"reacted","cycle":cycle}))?;
        }
        self.hook("SessionEnd", &total.to_string(), json!({}))
            .await?;
        emit(&json!({"step":"ended","cycles":total}))
    }
}

pub fn run(args: &[String]) -> Result<()> {
    let root = PathBuf::from(env("GROK_HOME")?);
    std::fs::create_dir_all(&root)?;
    let cwd = std::fs::canonicalize(std::env::current_dir()?)?
        .to_string_lossy()
        .into_owned();
    let identity: serde_json::Map<String, Value> = IDENTITIES
        .iter()
        .map(|key| ((*key).into(), json!(std::env::var(key).ok())))
        .collect();
    audit(
        &root,
        json!({"kind":"start","argv":args,"cwd":cwd,"pid":std::process::id(),"identity":identity,"updater":std::env::var("GROK_DISABLE_AUTOUPDATER").ok()}),
    )?;
    if args.ends_with(&["agent".into(), "--no-leader".into(), "stdio".into()]) {
        return rpc(&root);
    }
    ensure!(args.first().map(String::as_str) == Some("--fixture-profile"));
    let option = |key: &str| args.windows(2).find(|p| p[0] == key).map(|p| p[1].clone());
    ensure!(option("--cwd").as_deref() == Some(cwd.as_str()));
    let sid = option("--resume")
        .or_else(|| option("--session-id"))
        .unwrap_or_else(|| format!("00000000-0000-4000-8000-{:012x}", std::process::id()));
    ensure!(env("CLANK_AGENT")? == "grok");
    for key in IDENTITIES {
        if !["CLANK_AGENT", "GROK_SESSION_ID"].contains(key) {
            ensure!(
                std::env::var(key).unwrap_or_default().is_empty(),
                "foreign identity {key}"
            );
        }
    }
    ensure!(
        std::env::var("GROK_SESSION_ID")
            .ok()
            .is_none_or(|s| s == sid)
    );
    if option("--resume").is_some() {
        let previous: Value = serde_json::from_slice(&std::fs::read(summary(&root, &cwd, &sid))?)?;
        ensure!(previous["info"]["cwd"] == cwd);
    } else {
        save(&root, &cwd, &sid, None)?;
    }
    let hooks: Value = serde_json::from_slice(&std::fs::read(root.join("hooks/clank.json"))?)?;
    let native = Native {
        root,
        cwd,
        sid,
        hooks: hooks["hooks"].clone(),
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(native.interactive(args))
}
