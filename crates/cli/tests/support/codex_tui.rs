//! Actual terminal-client experiments. All profiles, hooks and commands are owned.
use super::*;
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
use tokio::net::UnixDatagram;

struct Terminal {
    _child: OwnedCodex,
    reader: tokio::task::JoinHandle<()>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

fn terminal(home: &Path, remote: &Path) -> Result<Terminal> {
    let master =
        rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)?;
    rustix::fs::fcntl_setfl(
        &master,
        rustix::fs::fcntl_getfl(&master)? | rustix::fs::OFlags::NONBLOCK,
    )?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let path = rustix::pty::ptsname(&master, Vec::new())?;
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(std::ffi::OsStr::from_bytes(path.as_bytes()))?;
    rustix::termios::tcsetwinsize(
        &slave,
        rustix::termios::Winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )?;
    let binary = std::env::var_os("CLANK_CODEX_PROTOCOL_BINARY").context("installed Codex")?;
    let mut command = std::process::Command::new(binary);
    command
        .env_clear()
        .env("HOME", home)
        .env("CODEX_HOME", home)
        .env("CLANK_QUESTION_PROOF_KEY", "fixture-only")
        .env("TERM", "xterm-256color")
        .env("CLANK_TUI_SENTINEL", home.file_name().unwrap())
        .current_dir(home)
        .process_group(0)
        .stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave)
        .args([
            "--no-alt-screen",
            "--dangerously-bypass-approvals-and-sandbox",
            "--remote",
            &format!("unix://{}", remote.display()),
            "-C",
        ])
        .arg(home)
        .arg("Run the diagnostic command and finish.");
    let child = OwnedCodex {
        child: command.spawn()?,
    };
    let output = Arc::new(Mutex::new(Vec::new()));
    let captured = output.clone();
    let fd = tokio::io::unix::AsyncFd::new(master)?;
    let reader = tokio::spawn(async move {
        let mut bytes = [0u8; 32768];
        loop {
            let Ok(mut ready) = fd.readable().await else {
                break;
            };
            let count = match ready.try_io(|fd| Ok(rustix::io::read(fd.get_ref(), &mut bytes)?)) {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(count)) => count,
                Err(_) => continue,
            };
            captured.lock().unwrap().extend_from_slice(&bytes[..count]);
            // Terminal capability replies only; no user-input wake is injected.
            if bytes[..count].windows(4).any(|s| s == b"\x1b[6n") {
                let _ = rustix::io::write(fd.get_ref(), b"\x1b[1;1R");
            }
            if bytes[..count].windows(3).any(|s| s == b"\x1b[c") {
                let _ = rustix::io::write(fd.get_ref(), b"\x1b[?1;2c");
            }
        }
    });
    Ok(Terminal {
        _child: child,
        reader,
        output,
    })
}

async fn receive(socket: &UnixDatagram) -> Result<Value> {
    let mut bytes = vec![0; 65536];
    let count = tokio::time::timeout(TEST_WAIT, socket.recv(&mut bytes)).await??;
    Ok(serde_json::from_slice(&bytes[..count])?)
}

async fn trust_fixture_hooks(socket: &Path, home: &Path, cwd: &Path) -> Result<()> {
    let mut client = Client::connect(socket).await?;
    let inventory = client.call("hooks/list", json!({"cwds":[cwd]})).await?;
    let hooks = inventory["data"][0]["hooks"]
        .as_array()
        .context("fixture hooks")?;
    ensure!(!hooks.is_empty());
    let mut trusted = serde_json::Map::new();
    for hook in hooks {
        ensure!(hook["sourcePath"] == home.join("hooks.json").to_str().unwrap());
        trusted.insert(
            hook["key"].as_str().context("hook key")?.into(),
            json!({"trusted_hash":hook["currentHash"]}),
        );
    }
    client.call("config/batchWrite", json!({"edits":[{"keyPath":"hooks.state","value":trusted,"mergeStrategy":"upsert"}],"reloadUserConfig":true})).await?;
    client.close().await?;
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(home.join("config.toml"))?,
        "\n[projects.{}]\ntrust_level = \"trusted\"",
        serde_json::to_string(cwd.to_str().unwrap())?
    )?;
    Ok(())
}

fn save_extra(name: &str, evidence: &Value, model: &Model, terminal: &Terminal) -> Result<()> {
    if let Some(dir) = std::env::var_os("CLANK_CODEX_PROBE_TRACE_DIR") {
        let dir = PathBuf::from(dir).join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("evidence.json"),
            serde_json::to_vec_pretty(evidence)?,
        )?;
        std::fs::write(
            dir.join("model-inputs.json"),
            serde_json::to_vec_pretty(&*model.inputs.lock().unwrap())?,
        )?;
        std::fs::write(dir.join("tui.txt"), &*terminal.output.lock().unwrap())?;
    }
    Ok(())
}

fn final_item(count: usize) -> Value {
    json!({"type":"message","id":format!("final-{count}"),"role":"assistant","phase":"final_answer",
        "content":[{"type":"output_text","text":"Fixture finished.","annotations":[]}]})
}

#[tokio::test]
#[ignore = "actual installed Codex TUI; scripted subagent lifecycle, no external inference"]
async fn installed_codex_tui_subagent_probe() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let gate_path = home.join("gate");
    let gate = UnixDatagram::bind(&gate_path)?;
    let observed_path = home.join("observed");
    let observed = UnixDatagram::bind(&observed_path)?;
    let fixture = env!("CARGO_BIN_EXE_clank-test-fixture");
    let command = shell_words::join([
        fixture,
        "codex-completion",
        "command",
        gate_path.to_str().unwrap(),
        "0",
    ]);
    let counts = Mutex::new(std::collections::BTreeMap::<String, usize>::new());
    let parent = Mutex::new(None::<String>);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let model = Model::scripted_input(Arc::new(move |global, input| {
        let thread = input["client_metadata"]["thread_id"].as_str().unwrap().to_owned();
        let is_parent = parent.lock().unwrap().get_or_insert(thread.clone()) == &thread;
        let mut counts = counts.lock().unwrap();
        let count = counts.entry(thread.clone()).or_default();
        *count += 1;
        let _ = tx.send(json!({"thread":thread,"parent":is_parent,"count":*count}));
        if *count == 1 && is_parent {
            json!({"type":"function_call","id":"fc-spawn","call_id":"spawn-1","namespace":"multi_agent_v1","name":"spawn_agent",
                "arguments":json!({"message":"Run the local fixture command, then finish."}).to_string()})
        } else if *count == 1 {
            json!({"type":"function_call","id":"fc-child","call_id":"child-command","name":"exec_command",
                "arguments":json!({"cmd":command,"yield_time_ms":10000,"max_output_tokens":1000}).to_string()})
        } else { final_item(global) }
    }), None).await?;
    let stop = shell_words::join([
        fixture,
        "codex-completion",
        "hook",
        gate_path.to_str().unwrap(),
        "0",
    ]);
    let observe = shell_words::join([fixture, "codex-observe", observed_path.to_str().unwrap()]);
    std::fs::write(
        home.join("hooks.json"),
        serde_json::to_vec(&json!({"hooks":{
            "Stop":[{"hooks":[{"type":"command","command":stop,"timeout":20}]}],
            "SubagentStart":[{"hooks":[{"type":"command","command":observe,"timeout":10}]}],
            "SubagentStop":[{"hooks":[{"type":"command","command":observe,"timeout":10}]}]
        }}))?,
    )?;
    let (_server, socket) =
        start_codex_with_features(&home, &model, "code_mode = false\ncode_mode_only = false")
            .await?;
    trust_fixture_hooks(&socket, &home, &home).await?;
    let terminal = terminal(&home, &socket)?;
    let mut evidence = Vec::new();
    let result = async {
        let first = tokio::time::timeout(TEST_WAIT, rx.recv())
            .await?
            .context("parent request")?;
        let parent = first["thread"].as_str().unwrap().to_owned();
        evidence.push(first);
        let start = receive(&observed).await?;
        ensure!(start["hook_event_name"] == "SubagentStart");
        let child = start["agent_id"].as_str().context("child ID")?.to_owned();
        evidence.push(start);
        let mut observer = Client::connect(&socket).await?;
        observer.attach_loaded(&parent, &home).await?;
        observer.attach_loaded(&child, &home).await?;
        let read = observer
            .call(
                "thread/read",
                json!({"threadId":child,"includeTurns":false}),
            )
            .await?;
        ensure!(read["thread"]["parentThreadId"] == parent);
        evidence.push(read);
        let first = receive(&gate).await?;
        let second = receive(&gate).await?;
        let (parked, command) = if first["kind"] == "hook" {
            (first, second)
        } else {
            (second, first)
        };
        ensure!(command["kind"] == "command");
        ensure!(parked["input"]["session_id"] == parent);
        evidence.push(parked);
        while let Ok(request) = rx.try_recv() {
            evidence.push(request);
        }
        gate.send_to(b"exit", home.join("gate.command")).await?;
        let child_stop = receive(&observed).await?;
        ensure!(child_stop["hook_event_name"] == "SubagentStop" && child_stop["agent_id"] == child);
        evidence.push(child_stop);
        evidence.push(
            observer
                .wait(|v| v["method"] == "turn/completed" && v["params"]["threadId"] == child)
                .await?,
        );
        // Distinguish the child's completion request from a parent continuation.
        let native = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let request = rx.recv().await.context("model requests closed")?;
                if request["parent"] == true {
                    return Ok::<_, anyhow::Error>(request);
                }
            }
        })
        .await;
        evidence.push(
            json!({"native_parent_continuation":native.as_ref().ok().and_then(|v|v.as_ref().ok())}),
        );
        ensure!(
            native.is_err(),
            "the child's completion continued the parked parent: {native:?}"
        );
        gate.send_to(
            br#"{"decision":"block","reason":"Fixture observed subagent completion."}"#,
            home.join("gate.hook"),
        )
        .await?;
        let next = tokio::time::timeout(TEST_WAIT, rx.recv())
            .await?
            .context("parent continuation")?;
        ensure!(next["parent"] == true);
        evidence.push(next);
        ensure!(receive(&gate).await?["input"]["session_id"] == parent);
        gate.send_to(b"{}", home.join("gate.hook")).await?;
        evidence.push(
            observer
                .wait(|v| v["method"] == "turn/completed" && v["params"]["threadId"] == parent)
                .await?,
        );
        observer.close().await?;
        let mut reconnect = Client::connect(&socket).await?;
        reconnect.attach_loaded(&parent, &home).await?;
        evidence.push(
            reconnect
                .call(
                    "thread/read",
                    json!({"threadId":parent,"includeTurns":true}),
                )
                .await?,
        );
        evidence.push(
            reconnect
                .call("thread/read", json!({"threadId":child,"includeTurns":true}))
                .await?,
        );
        reconnect.close().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    save_extra(
        "subagent",
        &json!({"events":evidence,"result":format!("{result:?}")}),
        &model,
        &terminal,
    )?;
    result
}

#[derive(Clone, Copy, Debug)]
struct Case {
    mode: &'static str,
    phase: &'static str,
    exit: i32,
    repeat: usize,
}

impl Case {
    fn name(self) -> String {
        format!(
            "private-{}-{}-{}-{}",
            self.mode, self.phase, self.exit, self.repeat
        )
    }
}

async fn probe(case: Case) -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let gate_path = home.join("gate");
    let gate = UnixDatagram::bind(&gate_path)?;
    let observed_path = home.join("observed");
    let observed = UnixDatagram::bind(&observed_path)?;
    let fixture = env!("CARGO_BIN_EXE_clank-test-fixture");
    let command = shell_words::join([
        fixture,
        "codex-completion",
        "command",
        gate_path.to_str().unwrap(),
        &case.exit.to_string(),
    ]);
    let command2 = shell_words::join([
        fixture,
        "codex-completion",
        "command2",
        gate_path.to_str().unwrap(),
        &case.exit.to_string(),
    ]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let pause = Arc::new(tokio::sync::Notify::new());
    let model = Model::scripted(Arc::new(move |count| {
        let _ = tx.send(count);
        if count == 1 {
            let args=json!({"cmd":command,"yield_time_ms":if case.mode=="cell" {1000} else {250},"max_output_tokens":1000});
            if case.mode=="native" {
                json!({"type":"function_call","id":"fc-command","call_id":"command-1","name":"exec_command","arguments":args.to_string()})
            } else {
                let js=match case.mode {
                    "cell"=>format!("// @exec: {{\"yield_time_ms\":1}}\ntext(await tools.exec_command({args}));"),
                    "multi"=>format!("text(await Promise.all([tools.exec_command({args}),tools.exec_command({})]));",
                        json!({"cmd":command2,"yield_time_ms":250,"max_output_tokens":1000})),
                    _=>format!("text(await tools.exec_command({args}));"),
                };
                json!({"type":"custom_tool_call","id":"fc-command","call_id":"command-1","name":"exec","input":js})
            }
        } else if count == 3 && case.mode == "cell" {
            json!({"type":"function_call","id":"fc-wait-cell","call_id":"wait-cell","name":"wait",
                "arguments":json!({"cell_id":"1","yield_time_ms":1000,"max_tokens":1000}).to_string()})
        } else {
            json!({"type":"message","id":format!("final-{count}"),"role":"assistant","phase":"final_answer",
                "content":[{"type":"output_text","text":"Fixture finished.","annotations":[]}]})
        }
    }),Some(pause.clone())).await?;
    let hook = shell_words::join([
        fixture,
        "codex-completion",
        "hook",
        gate_path.to_str().unwrap(),
        "0",
    ]);
    let observe_hook =
        shell_words::join([fixture, "codex-observe", observed_path.to_str().unwrap()]);
    let mut hooks = json!({
        "PreToolUse":[{"hooks":[{"type":"command","command":observe_hook,"timeout":10}]}],
        "PostToolUse":[{"hooks":[{"type":"command","command":observe_hook,"timeout":10}]}]
    });
    if case.phase != "idle" {
        hooks["Stop"] = json!([{"hooks":[{"type":"command","command":hook,"timeout":20}]}]);
    }
    std::fs::write(
        home.join("hooks.json"),
        serde_json::to_vec(&json!({"hooks":hooks}))?,
    )?;
    let features = if case.mode == "native" {
        "code_mode = false\ncode_mode_only = false"
    } else {
        "code_mode = true\ncode_mode_only = true\ncode_mode_host = true"
    };
    let (_server, socket) = start_codex_with_features(&home, &model, features).await?;
    let mut client = Client::connect(&socket).await?;
    let inventory = client.call("hooks/list", json!({"cwds":[home]})).await?;
    let inventory_hooks = inventory["data"][0]["hooks"].as_array().context("hooks")?;
    ensure!(inventory_hooks.len() == if case.phase == "idle" { 2 } else { 3 });
    let mut trusted = serde_json::Map::new();
    for h in inventory_hooks {
        ensure!(h["sourcePath"] == home.join("hooks.json").to_str().unwrap());
        trusted.insert(
            h["key"].as_str().context("key")?.into(),
            json!({"trusted_hash":h["currentHash"]}),
        );
    }
    client.call("config/batchWrite",json!({"edits":[{"keyPath":"hooks.state","value":trusted,"mergeStrategy":"upsert"}],"reloadUserConfig":true})).await?;
    client.close().await?;
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(home.join("config.toml"))?,
        "\n[projects.{}]\ntrust_level = \"trusted\"",
        serde_json::to_string(home.to_str().unwrap())?
    )?;
    let terminal = terminal(&home, &socket)?;
    let mut evidence = Vec::new();
    let started_at = std::time::Instant::now();
    let mut record =
        |v: Value| evidence.push(json!({"ms":started_at.elapsed().as_millis(),"event":v}));
    let result=async {
        ensure!(tokio::time::timeout(TEST_WAIT,rx.recv()).await?==Some(1));
        let pre=receive(&observed).await?;
        ensure!(pre["hook_event_name"]=="PreToolUse");
        let thread=pre["session_id"].as_str().context("pre hook thread")?.to_owned();
        record(pre);
        let mut observer={
            let mut c=Client::connect(&socket).await?;
            ensure!(c.attach_loaded(&thread,&home.join("wrong")).await.is_err());
            c.attach_loaded(&thread,&home).await?;
            Some(c)
        };
        let tasks=if case.mode=="multi" {2} else {1};
        for _ in 0..tasks {
            let started=receive(&gate).await?;
            ensure!(started["kind"].as_str().is_some_and(|s|s.starts_with("command")));
            ensure!(started["cwd"]==home.to_str().unwrap());
            ensure!(started["sentinel"]==home.file_name().unwrap().to_str().unwrap());
            record(started);
        }
        ensure!(tokio::time::timeout(TEST_WAIT,rx.recv()).await?==Some(2));
        let yielded=model.inputs.lock().unwrap()[1]["input"].as_array().context("model input")?.iter()
            .find(|item|item["call_id"]=="command-1" && item.get("output").is_some()).context("tool result")?["output"].to_string();
        let marker=match case.mode {"cell"=>"Script running with cell ID","native"=>"Process running with session ID",_=>"session_id"};
        ensure!(yielded.contains(marker),"tool did not yield: {yielded}");
        record(json!({"kind":"agent_request","count":2}));
        let mut running_ids=std::collections::BTreeSet::new();
        if let Some(mut c)=observer.take() {
            let running=c.call("thread/backgroundTerminals/list",json!({"threadId":thread})).await?;
            record(json!({"kind":"inventory","value":running}));
            ensure!(running["data"].as_array().is_some_and(|items|items.len()==tasks || (case.mode=="cell" && items.is_empty())));
            for item in running["data"].as_array().unwrap() {
                running_ids.insert(item["itemId"].as_str().context("running item ID")?.to_owned());
            }
            // Reattach to the same already-loaded thread, not a copied rollout.
            c.close().await?;
            let mut reconnected=Client::connect(&socket).await?;
            reconnected.attach_loaded(&thread,&home).await?;
            observer=Some(reconnected);
        }
        let mut completed=std::collections::BTreeSet::new();
        if case.phase=="active" {
            ensure!(tasks==1 && running_ids.len()==1,"active case requires one identified command");
            gate.send_to(b"exit",home.join("gate.command")).await?;
            record(json!({"kind":"release_while_model_active"}));
            // Sending the release is not proof of exit. Keep the model's
            // final response parked until the identified command has ended.
            let c=observer.as_mut().context("active case requires native observer")?;
            let end=c.wait(|v|v["method"]=="item/completed" && v["params"]["item"]["type"]=="commandExecution").await?;
            ensure!(end["params"]["threadId"]==thread);
            ensure!(end["params"]["item"]["exitCode"]==case.exit);
            let item=end["params"]["item"]["id"].as_str().context("completed item ID")?;
            ensure!(running_ids.contains(item),"completion is not the identified active command");
            completed.insert(item.to_owned());
            record(end);
        }
        record(json!({"kind":"release_model_response","verified_completions":completed.len()}));
        pause.notify_one();
        if case.phase=="boundary" {
            let c=observer.as_mut().context("boundary requires native event observer")?;
            record(c.wait(|v|v["method"]=="item/completed" && v["params"]["item"]["phase"]=="final_answer").await?);
            gate.send_to(b"exit",home.join("gate.command")).await?;
            record(json!({"kind":"release_at_final_answer"}));
        }
        let stop=if case.phase=="idle" {
            let c=observer.as_mut().context("idle requires native event observer")?;
            record(c.wait(|v|v["method"]=="turn/completed").await?);
            None
        } else {
            let stop=receive(&gate).await?;
            ensure!(stop["kind"]=="hook");
            ensure!(stop["input"]["session_id"]==thread);
            record(stop.clone());
            Some(stop)
        };
        ensure!(rx.try_recv().is_err(),"agent continued before exit observation");
        if case.phase=="missed" {
            // Disconnect the monitor; a separate audit subscriber establishes
            // the exit barrier without passing its notification to the monitor.
            observer.take().context("private observer")?.close().await?;
            let mut audit=Client::connect(&socket).await?;
            audit.attach_loaded(&thread,&home).await?;
            observer=Some(audit);
        }
        if !["active","boundary"].contains(&case.phase) {
            gate.send_to(b"exit",home.join("gate.command")).await?;
            record(json!({"kind":"release_command"}));
        }
        if tasks==2 {gate.send_to(b"exit",home.join("gate.command2")).await?;}
        if let Some(c)=observer.as_mut() {
            // Active-model cases already consumed and verified their exit.
            for _ in completed.len()..tasks {
                let end=c.wait(|v|v["method"]=="item/completed" && v["params"]["item"]["type"]=="commandExecution").await?;
                ensure!(end["params"]["threadId"]==thread);
                ensure!(end["params"]["item"]["exitCode"]==case.exit);
                ensure!(completed.insert(end["params"]["item"]["id"].as_str().context("completed item ID")?.to_owned()),"duplicate completion");
                record(end);
            }
        }
        if case.phase=="missed" {
            observer.take().unwrap().close().await?;
            let mut recovered=Client::connect(&socket).await?;
            recovered.attach_loaded(&thread,&home).await?;
            let history=recovered.call("thread/read",json!({"threadId":thread,"includeTurns":true})).await?;
            ensure!(history["thread"]["turns"].as_array().context("turns")?.iter().any(|turn|
                turn["items"].as_array().is_some_and(|items|items.iter().any(|item|
                    item["type"]=="commandExecution" && item["exitCode"]==case.exit))),"missed exit was not recovered");
            record(json!({"kind":"missed_exit_recovered","value":history}));
            observer=Some(recovered);
        }
        if case.phase=="steer" {
            let turn=&stop.as_ref().unwrap()["input"]["turn_id"];
            let reply=observer.as_mut().unwrap().call("turn/steer",json!({"threadId":thread,"expectedTurnId":turn,
                "input":[{"type":"text","text":"Fixture observed completion; collect it."}]})).await?;
            record(json!({"kind":"steer_reply","value":reply}));
        }
        // The immediate bridge case has no timer between exit and hook release.
        let window=if case.phase=="bridge" {Duration::ZERO} else {Duration::from_secs(2)};
        let next=if case.phase=="bridge" {None} else {tokio::time::timeout(window,rx.recv()).await.ok().flatten()};
        record(json!({"kind":"native_observation","next_request":next,"window_ms":window.as_millis()}));
        ensure!(next.is_none(),"native completion continued the agent: request {next:?}");
        let mut hook_bytes=vec![0;65536];
        while let Ok(n)=observed.try_recv(&mut hook_bytes) {
            let hook:Value=serde_json::from_slice(&hook_bytes[..n])?;
            ensure!(hook["hook_event_name"]!="PostToolUse","a yielded or exited background command ran PostToolUse: {hook}");
            record(hook);
        }
        if case.phase=="interrupt" {
            let turn=&stop.as_ref().unwrap()["input"]["turn_id"];
            let c=observer.as_mut().unwrap();
            record(c.call("turn/interrupt",json!({"threadId":thread,"turnId":turn})).await?);
            let end=c.wait(|v|v["method"]=="turn/completed" && v["params"]["turn"]["id"]==*turn).await?;
            ensure!(end["params"]["turn"]["status"]=="interrupted");
            record(end);
            // SIGKILL cannot unlink the disposable hook fixture's socket.
            let _=std::fs::remove_file(home.join("gate.hook"));
            record(c.call("turn/start",json!({"threadId":thread,"input":[{"type":"text","text":"Collect the completed fixture command."}]})).await?);
            ensure!(tokio::time::timeout(TEST_WAIT,rx.recv()).await?==Some(3));
            let new_stop=receive(&gate).await?;
            ensure!(new_stop["input"]["turn_id"]!=*turn);
            record(new_stop);
            gate.send_to(b"{}",home.join("gate.hook")).await?;
            record(c.wait(|v|v["method"]=="turn/completed").await?);
            return Ok(());
        }
        if let Some(stop)=&stop {
            if let Some(path)=stop["input"]["transcript_path"].as_str() {
                let transcript=std::fs::read_to_string(path)?;
                record(json!({"kind":"transcript_snapshot","rows":transcript.lines().map(serde_json::from_str::<Value>).collect::<std::result::Result<Vec<_>,_>>()?}));
            }
            gate.send_to(br#"{"decision":"block","reason":"Fixture observed background completion."}"#,home.join("gate.hook")).await?;
            record(json!({"kind":"release_hook"}));
            ensure!(tokio::time::timeout(TEST_WAIT,rx.recv()).await?==Some(3));
            record(json!({"kind":"agent_request","count":3}));
            if case.mode=="cell" {
                ensure!(tokio::time::timeout(TEST_WAIT,rx.recv()).await?==Some(4));
                let output=model.output("wait-cell")?;
                ensure!(output.contains("Script completed"),"cell collection failed: {output}");
                record(json!({"kind":"collected_cell","output":output}));
            }
            ensure!(receive(&gate).await?["kind"]=="hook");
            gate.send_to(b"{}",home.join("gate.hook")).await?;
        }
        if let Some(mut c)=observer.take() {
            if stop.is_some(){record(c.wait(|v|v["method"]=="turn/completed").await?);}
            record(json!({"kind":"remaining_native_notifications","value":c.backlog}));
            c.close().await?;
            let mut reconnect=Client::connect(&socket).await?;
            reconnect.attach_loaded(&thread,&home).await?;
            record(json!({"kind":"after_exit_inventory","value":reconnect.call("thread/backgroundTerminals/list",json!({"threadId":thread})).await?}));
            record(json!({"kind":"after_exit_history","value":reconnect.call("thread/read",json!({"threadId":thread,"includeTurns":true})).await?}));
            reconnect.close().await?;
        }
        Ok::<_,anyhow::Error>(())
    }.await;
    if let Some(dir) = std::env::var_os("CLANK_CODEX_PROBE_TRACE_DIR") {
        let dir = PathBuf::from(dir).join(case.name());
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("evidence.json"),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
        std::fs::write(
            dir.join("model-inputs.json"),
            serde_json::to_vec_pretty(&*model.inputs.lock().unwrap())?,
        )?;
        std::fs::write(dir.join("tui.txt"), &*terminal.output.lock().unwrap())?;
    }
    println!("{}: {result:?}", case.name());
    result
}

#[tokio::test]
#[ignore = "actual installed Codex TUI; isolated local scripted model and PTY"]
async fn installed_codex_tui_background_probe() -> Result<()> {
    // Select a narrow development probe without changing the committed matrix.
    let quick = std::env::var("CLANK_CODEX_TUI_CASE").ok();
    if quick.as_deref() == Some("active") {
        for mode in ["native", "wrapped"] {
            for exit in [0, 7] {
                for repeat in 0..2 {
                    probe(Case {
                        mode,
                        phase: "active",
                        exit,
                        repeat,
                    })
                    .await?;
                }
            }
        }
        return Ok(());
    }
    if let Some(mode) = quick.as_deref() {
        let phase = if ["bridge", "missed", "steer", "interrupt"].contains(&mode) {
            mode
        } else {
            "parked"
        };
        let phase = match phase {
            "bridge" => "bridge",
            "missed" => "missed",
            "steer" => "steer",
            "interrupt" => "interrupt",
            _ => "parked",
        };
        let mode = match mode {
            "cell" => "cell",
            "multi" => "multi",
            _ => "wrapped",
        };
        return probe(Case {
            mode,
            phase,
            exit: 0,
            repeat: 0,
        })
        .await;
    }
    for mode in ["native", "wrapped"] {
        for phase in ["active", "boundary", "parked", "idle"] {
            for exit in [0, 7] {
                for repeat in 0..2 {
                    probe(Case {
                        mode,
                        phase,
                        exit,
                        repeat,
                    })
                    .await?;
                }
            }
        }
    }
    // Independent private endpoints run simultaneously with different sentinels.
    tokio::try_join!(
        probe(Case {
            mode: "multi",
            phase: "parked",
            exit: 0,
            repeat: 0
        }),
        probe(Case {
            mode: "multi",
            phase: "parked",
            exit: 7,
            repeat: 1
        }),
    )?;
    probe(Case {
        mode: "cell",
        phase: "parked",
        exit: 0,
        repeat: 0,
    })
    .await?;
    for phase in ["bridge", "missed", "steer", "interrupt"] {
        probe(Case {
            mode: "wrapped",
            phase,
            exit: 7,
            repeat: 0,
        })
        .await?;
    }
    Ok(())
}
