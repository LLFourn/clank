//! Opt-in diagnostic: a native command's completion does not continue the
//! model within the observation window, and a Stop hook's block does.
//! Uses the question protocol fixture's isolated server and scripted model.
use super::*;
use tokio::net::UnixDatagram;

const OBSERVE: Duration = Duration::from_secs(2);

struct Trace {
    name: String,
    start: std::time::Instant,
    rows: Vec<Value>,
}

impl Drop for Trace {
    fn drop(&mut self) {
        if let Some(dir) = std::env::var_os("CLANK_CODEX_PROBE_TRACE_DIR") {
            let save = || -> Result<()> {
                let dir = PathBuf::from(dir);
                std::fs::create_dir_all(&dir)?;
                std::fs::write(
                    dir.join(format!("{}.json", self.name)),
                    serde_json::to_vec_pretty(&self.rows)?,
                )?;
                Ok(())
            };
            if let Err(error) = save() {
                eprintln!("could not save diagnostic trace: {error:#}");
            }
        }
    }
}

impl Trace {
    fn record(&mut self, event: Value) {
        self.rows
            .push(json!({"ms":self.start.elapsed().as_millis(),"event":event}));
    }
}

async fn marker(socket: &UnixDatagram, kind: &str, trace: &mut Trace) -> Result<()> {
    let mut bytes = [0u8; 16384];
    let size = tokio::time::timeout(TEST_WAIT, socket.recv(&mut bytes))
        .await
        .with_context(|| format!("waiting for {kind} socket marker"))??;
    let value: Value = serde_json::from_slice(&bytes[..size])?;
    trace.record(value.clone());
    ensure!(value["kind"] == kind, "expected {kind}, got {value}");
    Ok(())
}

async fn record_until(
    client: &mut Client,
    trace: &mut Trace,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value> {
    tokio::time::timeout(TEST_WAIT, async {
        loop {
            let event = match client.backlog.pop_front() {
                Some(event) => event,
                None => client.receive().await?,
            };
            trace.record(event.clone());
            if predicate(&event) {
                return Ok(event);
            }
        }
    })
    .await
    .context("waiting for diagnostic event")?
}

async fn observe(client: &mut Client, trace: &mut Trace) -> Result<()> {
    let deadline = tokio::time::Instant::now() + OBSERVE;
    loop {
        let event = if let Some(event) = client.backlog.pop_front() {
            event
        } else {
            match tokio::time::timeout_at(deadline, client.receive()).await {
                Ok(event) => event?,
                Err(_) => break,
            }
        };
        trace.record(event);
        ensure!(trace.rows.len() < 10000, "unexpected notification flood");
    }
    Ok(())
}

async fn scenario(phase: &str, exit: i32, repeat: usize, wrapped: bool) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let result = scenario_in(&home, phase, exit, repeat, wrapped).await;
    if result.is_err() {
        eprintln!(
            "isolated server stderr: {}",
            std::fs::read_to_string(home.join("server.stderr")).unwrap_or_default()
        );
    }
    result
}

async fn scenario_in(
    home: &Path,
    phase: &str,
    exit: i32,
    repeat: usize,
    wrapped: bool,
) -> Result<()> {
    let gate_path = home.join("gate");
    let gate = UnixDatagram::bind(&gate_path)?;
    let fixture = env!("CARGO_BIN_EXE_clank-test-fixture");
    let command = shell_words::join([
        fixture,
        "codex-completion",
        "command",
        gate_path.to_str().context("gate path")?,
        &exit.to_string(),
    ]);
    let mode = if wrapped { "code-mode" } else { "native" };
    let mut trace = Trace {
        name: format!("{mode}-{phase}-exit{exit}-{repeat}"),
        start: std::time::Instant::now(),
        rows: Vec::new(),
    };
    let pause = Arc::new(tokio::sync::Notify::new());
    let (requests, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let script = Arc::new(move |count| {
        // A deterministic model makes exactly one tool call, then a final
        // answer. Subsequent requests are direct evidence of continuation.
        requests.send(count).expect("diagnostic receiver alive");
        if count == 1 {
            let args = json!({"cmd":command,"yield_time_ms":250,"max_output_tokens":1000});
            if wrapped {
                json!({"type":"custom_tool_call","id":"fc-command","call_id":"command-1",
                    "name":"exec","input":format!("text(await tools.exec_command({args}));")})
            } else {
                json!({"type":"function_call","id":"fc-command","call_id":"command-1",
                    "name":"exec_command","arguments":args.to_string()})
            }
        } else {
            json!({"type":"message","id":format!("final-{count}"),"role":"assistant","phase":"final_answer",
                "content":[{"type":"output_text","text":"Fixture turn finished.","annotations":[]}]})
        }
    });
    let model = Model::scripted(script, Some(pause.clone())).await?;
    if phase != "idle" {
        let hook = shell_words::join([
            fixture,
            "codex-completion",
            "hook",
            gate_path.to_str().unwrap(),
            "0",
        ]);
        std::fs::write(
            home.join("hooks.json"),
            serde_json::to_vec(&json!({"hooks":{"Stop":[{"hooks":[{
                "type":"command","command":hook,"timeout":20
            }]}]}}))?,
        )?;
    }
    let features = if wrapped {
        "code_mode = true\ncode_mode_only = true\ncode_mode_host = true"
    } else {
        "code_mode = false\ncode_mode_only = false"
    };
    let (server, socket) = start_codex_with_features(home, &model, features).await?;
    let mut client = Client::connect(&socket).await?;
    if phase != "idle" {
        let inventory = client.call("hooks/list", json!({"cwds":[home]})).await?;
        trace.record(inventory.clone());
        let hooks = inventory["data"][0]["hooks"]
            .as_array()
            .context("hook inventory")?;
        ensure!(
            hooks.len() == 1
                && hooks[0]["sourcePath"] == home.join("hooks.json").to_str().unwrap()
                && hooks[0]["eventName"] == "stop",
            "only the generated fixture hook may be trusted"
        );
        let key = hooks[0]["key"].as_str().context("hook key")?;
        client
            .call(
                "config/batchWrite",
                json!({"edits":[{"keyPath":"hooks.state",
            "value":{key:{"trusted_hash":hooks[0]["currentHash"]}},"mergeStrategy":"upsert"}],
            "reloadUserConfig":true}),
            )
            .await?;
    }
    let started = client
        .call(
            "thread/start",
            json!({"model":"mock-model","cwd":home,
        "approvalPolicy":"never","sandbox":"danger-full-access"}),
        )
        .await?;
    let thread = started["thread"]["id"].as_str().context("thread id")?;
    trace.record(
        client
            .call("hooks/list", json!({"threadId":thread}))
            .await?,
    );
    let turn = client
        .call(
            "turn/start",
            json!({"threadId":thread,
        "input":[{"type":"text","text":"Run the diagnostic command and finish."}]}),
        )
        .await?;
    trace.record(
        json!({"mode":mode,"phase":phase,"exit":exit,"repeat":repeat,"thread":thread,"turn":turn}),
    );
    marker(&gate, "command", &mut trace).await?;
    // The second request proves exec_command yielded while its process lives.
    ensure!(tokio::time::timeout(TEST_WAIT, request_rx.recv()).await? == Some(1));
    ensure!(tokio::time::timeout(TEST_WAIT, request_rx.recv()).await? == Some(2));
    trace.record(json!({"kind":"second_model_request"}));
    {
        let inputs = model.inputs.lock().unwrap();
        let result = inputs
            .last()
            .and_then(|r| r["input"].as_array())
            .and_then(|items| {
                items.iter().find(|item| {
                    item["call_id"] == "command-1"
                        && matches!(
                            item["type"].as_str(),
                            Some("function_call_output" | "custom_tool_call_output")
                        )
                })
            })
            .context("model-visible command result")?;
        let running = if wrapped {
            result["output"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["text"]
                        .as_str()
                        .and_then(|s| serde_json::from_str::<Value>(s).ok())
                        .is_some_and(|v| v["session_id"].is_number() && v["exit_code"].is_null())
                })
            })
        } else {
            result["output"]
                .as_str()
                .is_some_and(|s| s.contains("Process running"))
        };
        ensure!(running, "command did not yield: {result}");
        trace.record(json!({"kind":"yielded_tool_result","result":result}));
    }
    let command_end = |v: &Value| {
        v["method"] == "item/completed" && v["params"]["item"]["type"] == "commandExecution"
    };
    if phase == "active" {
        gate.send_to(b"exit", home.join("gate.command")).await?;
        trace.record(json!({"kind":"release_command"}));
        let end = record_until(&mut client, &mut trace, command_end).await?;
        ensure!(end["params"]["item"]["exitCode"] == exit);
    }
    pause.notify_one();
    if phase == "boundary" {
        record_until(&mut client, &mut trace, |v| {
            v["method"] == "item/completed" && v["params"]["item"]["phase"] == "final_answer"
        })
        .await?;
        gate.send_to(b"exit", home.join("gate.command")).await?;
        trace.record(json!({"kind":"release_command_at_final_answer"}));
    }
    if phase == "idle" {
        record_until(&mut client, &mut trace, |v| v["method"] == "turn/completed").await?;
    } else {
        record_until(&mut client, &mut trace, |v| v["method"] == "hook/started").await?;
        marker(&gate, "hook", &mut trace).await?;
    }
    if phase != "active" {
        if phase != "boundary" {
            gate.send_to(b"exit", home.join("gate.command")).await?;
            trace.record(json!({"kind":"release_command"}));
        }
        // At the hook-entry boundary the command may end before the hook's
        // start event. Do not discard that already-observed completion.
        let observed = trace
            .rows
            .iter()
            .find_map(|row| command_end(&row["event"]).then(|| row["event"].clone()));
        let end = match observed {
            Some(event) => event,
            None => record_until(&mut client, &mut trace, command_end).await?,
        };
        ensure!(end["params"]["item"]["exitCode"] == exit);
    }
    observe(&mut client, &mut trace).await?;
    let calls = model.inputs.lock().unwrap().len();
    trace.record(json!({"kind":"observation_complete","model_requests":calls,"window_ms":OBSERVE.as_millis()}));
    ensure!(
        calls == 2,
        "{}: native completion continued the model: {calls} requests, expected the initial tool call and the final answer",
        trace.name
    );
    if phase != "idle" {
        // Positive control: prove that the hook's documented continuation
        // response is effective, separately from native process completion.
        gate.send_to(
            br#"{"decision":"block","reason":"Diagnostic positive control: continue once."}"#,
            home.join("gate.hook"),
        )
        .await?;
        trace.record(json!({"kind":"release_hook_with_block"}));
        ensure!(tokio::time::timeout(TEST_WAIT, request_rx.recv()).await? == Some(3));
        marker(&gate, "hook", &mut trace).await?;
        gate.send_to(b"{}", home.join("gate.hook")).await?;
        record_until(&mut client, &mut trace, |v| v["method"] == "turn/completed").await?;
    }
    client.close().await?;
    drop(server); // Kill/reap owned group before removing its temporary profile.
    Ok(())
}

#[tokio::test]
#[ignore = "manual diagnostic against an installed Codex; isolated scripted localhost model"]
async fn installed_codex_completion_diagnostic() -> Result<()> {
    for wrapped in [false, true] {
        for phase in ["parked", "boundary", "active", "idle"] {
            for exit in [0, 7] {
                for repeat in 0..2 {
                    scenario(phase, exit, repeat, wrapped)
                        .await
                        .with_context(|| format!("{phase} exit {exit} repeat {repeat}"))?;
                }
            }
        }
    }
    Ok(())
}
