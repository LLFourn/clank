//! Installed Claude acceptance with a private PTY, profile and localhost model.
//! The model supplies one fixed question; it never calls a provider service.
use super::emit;
use anyhow::{Context, Result, ensure};
use clank::proc_run::{self, Group, Identity, Out, Spec, Stdin};
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::{Bytes, Incoming},
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use notify::Watcher;
use serde_json::{Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixStream},
    task::JoinSet,
};

const TOOL_ID: &str = "toolu_clank_question_proof";
const QUESTION_TEXT: &str = "Choose the fixture direction";
fn question() -> Value {
    json!({"questions":[{"question":QUESTION_TEXT,"header":"Direction","options":[{"label":"North","description":"First"},{"label":"South","description":"Second"}],"multiSelect":false}]})
}
fn load(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_slice(
        &std::fs::read(path.as_ref())
            .with_context(|| format!("read {}", path.as_ref().display()))?,
    )?)
}
fn save(path: impl AsRef<Path>, value: &Value) -> Result<()> {
    let path = path.as_ref();
    let mut staged = tempfile::NamedTempFile::new_in(path.parent().context("missing parent")?)?;
    staged.write_all(&serde_json::to_vec(value)?)?;
    staged.persist(path)?;
    Ok(())
}
fn results(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .flat_map(|m| m["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == TOOL_ID)
        .cloned()
        .collect()
}
fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}
fn recorded_results(profile: &Path) -> Vec<Value> {
    let mut found = Vec::new();
    for folder in std::fs::read_dir(profile.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for file in std::fs::read_dir(folder.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            if file.path().extension().is_some_and(|ext| ext == "jsonl") {
                found.extend(results(
                    &records(&file.path())
                        .into_iter()
                        // Newer Claude also stores an api-request-blob with a
                        // copy of this content. Only a native user message is
                        // a tool result; an outbound request is not a second
                        // answer. Keep distinct user results distinct.
                        .filter(|record| record["type"] == "user")
                        .map(|v| v["message"].clone())
                        .collect::<Vec<_>>(),
                ));
            }
        }
    }
    found
}
fn published(path: &Path, prompt: &Value) -> Value {
    let rows = records(path);
    json!({"prompt":rows.iter().any(|r| &r["promptId"] == prompt),
        "tool_use":rows.iter().any(|r|r["message"]["content"].as_array().is_some_and(|blocks|blocks.iter().any(|b|b["type"]=="tool_use" && b["id"]==TOOL_ID)))})
}

fn model_response(body: &Value) -> (String, &'static str) {
    let answered = !results(
        body["messages"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default(),
    )
    .is_empty();
    let content = if answered {
        json!([{"type":"text","text":"Fixture answer received."}])
    } else {
        json!([{"type":"tool_use","id":TOOL_ID,"name":"AskUserQuestion","input":question()}])
    };
    let reason = if answered { "end_turn" } else { "tool_use" };
    let message = json!({"id":"msg_fixture","type":"message","role":"assistant","model":body.get("model").cloned().unwrap_or(json!("claude-sonnet-4-6")),"content":content,"stop_reason":reason,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":10}});
    if body["stream"] != true {
        return (message.to_string(), "application/json");
    }
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    let mut events = vec![json!({"type":"message_start","message":start})];
    for (index, block) in content.as_array().unwrap().iter().enumerate() {
        let tool = block["type"] == "tool_use";
        let mut empty = block.clone();
        empty[if tool { "input" } else { "text" }] = if tool { json!({}) } else { json!("") };
        events.push(json!({"type":"content_block_start","index":index,"content_block":empty}));
        let delta = if tool {
            json!({"type":"input_json_delta","partial_json":block["input"].to_string()})
        } else {
            json!({"type":"text_delta","text":block["text"]})
        };
        events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.extend([json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":{"output_tokens":10}}),json!({"type":"message_stop"})]);
    (
        events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap()
                )
            })
            .collect(),
        "text/event-stream",
    )
}

async fn http(
    request: Request<Incoming>,
    requests: Arc<Mutex<Vec<Value>>>,
    changed: tokio::sync::mpsc::UnboundedSender<()>,
) -> Result<Response<Full<Bytes>>> {
    let (body, kind) = if request.method() == hyper::Method::POST {
        let value: Value =
            serde_json::from_slice(&request.into_body().collect().await?.to_bytes())?;
        requests.lock().unwrap().push(value.clone());
        let _ = changed.send(());
        model_response(&value)
    } else {
        ("{}".into(), "application/json")
    };
    Ok(Response::builder()
        .header("Content-Type", kind)
        .body(Full::new(Bytes::from(body)))?)
}

/// Executed in its own fixture process: map the inherited PTY onto all three
/// standard streams, then replace this process with the selected native CLI.
pub fn exec(args: &[String]) -> Result<()> {
    let tty = rustix::io::dup(std::io::stdin())?;
    rustix::stdio::dup2_stdout(&tty)?;
    rustix::stdio::dup2_stderr(&tty)?;
    let (binary, args) = args.split_first().context("missing Claude binary")?;
    Err(proc_run::exec(
        Spec::new(binary)
            .args(args)
            .stdin(Stdin::Inherit)
            .stdout(Out::Inherit)
            .stderr(Out::Inherit),
    )
    .into())
}

pub async fn hook(args: &[String]) -> Result<()> {
    let root = PathBuf::from(args.first().context("missing hook root")?);
    let mut raw = Vec::new();
    tokio::io::stdin().read_to_end(&mut raw).await?;
    let value: Value = serde_json::from_slice(&raw)?;
    let event = value["hook_event_name"].as_str().context("missing event")?;
    save(root.join(format!("{event}.json")), &value)?;
    std::fs::copy(
        value["transcript_path"]
            .as_str()
            .context("missing transcript")?,
        root.join(format!("{event}.transcript")),
    )?;
    if let Some(clank) = args.get(1) {
        if event == "PermissionRequest" {
            std::fs::write(root.join("hook-ready"), "clank")?;
        }
        let mut child = proc_run::start(
            Spec::new(clank)
                .arg("question-hook")
                .stdin(Stdin::Piped)
                .stdout(Out::Piped)
                .stderr(Out::Piped),
        )?;
        let mut input = child.take_stdin_async().unwrap();
        let mut output = child.take_stdout_async().unwrap();
        let mut error = child.take_stderr_async().unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let (_, _, _, status) = tokio::try_join!(
            async {
                input.write_all(&raw).await?;
                drop(input);
                Ok::<_, std::io::Error>(())
            },
            output.read_to_end(&mut out),
            error.read_to_end(&mut err),
            child.wait_async()
        )?;
        ensure!(
            status.success(),
            "question hook: {}",
            String::from_utf8_lossy(&err)
        );
        std::io::stdout().write_all(&out)?;
    } else if event == "PermissionRequest" {
        let listener = tokio::net::UnixListener::bind(root.join("answer.sock"))?;
        std::fs::write(root.join("hook-ready"), std::process::id().to_string())?;
        let (mut stream, _) = listener.accept().await?;
        let mut answer = [0; 1024];
        let n = stream.read(&mut answer).await?;
        ensure!(n > 0, "fixture answer channel closed");
        let mut updated = value["tool_input"].clone();
        updated["answers"] = json!({(QUESTION_TEXT):std::str::from_utf8(&answer[..n])?});
        let result = json!({"hookSpecificOutput":{"hookEventName":event,"decision":{"behavior":"allow","updatedInput":updated}}});
        save(root.join("hook-returned.json"), &result)?;
        emit(&result)?;
    }
    Ok(())
}

fn screen(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut chars = text.chars().peekable();
    let mut result = String::new();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

async fn pty_read(
    fd: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
    bytes: &mut [u8],
) -> std::io::Result<usize> {
    loop {
        let mut ready = fd.readable().await?;
        if let Ok(result) = ready.try_io(|fd| Ok(rustix::io::read(fd.get_ref(), &mut *bytes)?)) {
            return result;
        }
    }
}
fn key(fd: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>, bytes: &[u8]) -> Result<()> {
    ensure!(rustix::io::write(fd.get_ref(), bytes)? == bytes.len());
    Ok(())
}
async fn control(expected: &str) -> Result<()> {
    use tokio::io::AsyncBufReadExt;
    let mut line = String::new();
    tokio::io::BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    ensure!(
        line.trim() == expected,
        "expected controller {expected}, got {line:?}"
    );
    Ok(())
}

async fn scenario(binary: &str, name: &str, broker: Option<&[String]>) -> Result<()> {
    // Short paths leave room for the native Unix socket under either OS.
    let temp = tempfile::Builder::new().prefix("cq-").tempdir_in("/tmp")?;
    let root = temp.path().canonicalize()?;
    let home = root.join("home");
    let profile = home.join(".claude");
    std::fs::create_dir_all(&profile)?;
    let repo = if let Some(args) = broker {
        PathBuf::from(&args[0]).canonicalize()?
    } else {
        let p = root.join("repo");
        std::fs::create_dir(&p)?;
        p
    };
    let mut hook_args = vec![
        std::env::current_exe()?.to_string_lossy().into_owned(),
        "claude-hook".into(),
        root.to_string_lossy().into_owned(),
    ];
    if let Some(args) = broker {
        hook_args.push(args[1].clone());
    }
    let command = shell_words::join(&hook_args);
    let hooks: serde_json::Map<_,_> = ["PreToolUse","PermissionRequest","PostToolUse","PostToolUseFailure"].into_iter().map(|event|(event.into(),json!([{"matcher":"AskUserQuestion","hooks":[{"type":"command","command":command,"timeout":120}]}]))).collect();
    save(profile.join("settings.json"), &json!({"hooks":hooks}))?;
    save(
        profile.join(".claude.json"),
        &json!({"hasCompletedOnboarding":true,"lastOnboardingVersion":"2.1.280","theme":"dark","projects":{repo.to_string_lossy().as_ref():{"hasTrustDialogAccepted":true,"hasCompletedProjectOnboarding":true}}}),
    )?;
    let (changed, mut changes) = tokio::sync::mpsc::unbounded_channel();
    let notify = changed.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && !matches!(event.kind, notify::EventKind::Access(_))
        {
            let _ = notify.send(());
        }
    })?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut server = JoinSet::new();
    let observed = requests.clone();
    server.spawn(async move {
        let mut connections=JoinSet::new();
        loop {
            tokio::select! {
                connection=listener.accept()=> {
                    let (socket,_)=connection?;
                    let observed=observed.clone(); let changed=changed.clone();
                    connections.spawn(async move { hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(socket),service_fn(move |r|http(r,observed.clone(),changed.clone()))).await });
                }
                Some(_)=connections.join_next()=>{}
            }
        }
        #[allow(unreachable_code)] Ok::<(),std::io::Error>(())
    });
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
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
    let mut spec = Spec::new(std::env::current_exe()?)
        .args(["claude-exec", binary])
        .cwd(&repo)
        .group(Group::Session)
        .identity(Identity::Forbid)
        .stdin(Stdin::Fd(slave.into()));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if ["ANTHROPIC_", "CLAUDE_", "CLANK_", "CODEX_"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
            && name != "CLANK_TEST_RUNTIME"
        {
            spec = spec.env_remove(key);
        }
    }
    spec = spec
        .env("HOME", &home)
        .env("CLAUDE_CONFIG_DIR", &profile)
        .env("ANTHROPIC_BASE_URL", format!("http://{address}"))
        .env("ANTHROPIC_API_KEY", "fixture-local-only")
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("TERM", "xterm-256color");
    if let Some(args) = broker {
        spec = spec.args(["--session-id", &args[2]]);
    }
    spec = spec
        .args([
            "--model",
            "claude-sonnet-4-6",
            "--tools",
            "AskUserQuestion",
            "--settings",
        ])
        .arg(profile.join("settings.json"))
        .arg("--debug-file")
        .arg(root.join("debug.log"))
        .arg("Ask the fixture question.");
    let mut child = proc_run::start(spec)?;
    let master = tokio::io::unix::AsyncFd::new(master)?;
    let mut terminal = Vec::new();
    let outcome=tokio::time::timeout(Duration::from_secs(30),async {
        let (mut trusted,mut key_accepted,mut sent,mut late)=(false,false,false,false);
        let mut answer: Option<UnixStream>=None;
        let mut pre=Value::Null;
        loop {
            let screen=screen(&terminal);
            if !trusted && screen.contains("Yes, I trust this folder") {key(&master,b"\x1b[B\r")?;trusted=true;}
            if !key_accepted && screen.contains("Do you want to use this API key?") {key(&master,b"\x1b[A\r")?;key_accepted=true;}
            if !sent && root.join("hook-ready").exists() && screen.contains("Enter to select") {
                pre=load(root.join("PreToolUse.json"))?;
                let permission=load(root.join("PermissionRequest.json"))?;
                ensure!(pre["tool_use_id"]==TOOL_ID && pre["session_id"]==permission["session_id"] && pre["prompt_id"]==permission["prompt_id"]);
                ensure!(pre["tool_input"]==question() && permission["tool_input"]==question() && permission.get("tool_use_id").is_none());
                let transcript=Path::new(pre["transcript_path"].as_str().context("transcript")?);
                if published(transcript,&pre["prompt_id"])["tool_use"]==true {
                    ensure!(recorded_results(&profile).is_empty(),"question was answered before fixture action");
                    for event in ["PreToolUse","PermissionRequest"] {eprintln!("publication at {event}: {}",published(&root.join(format!("{event}.transcript")),&pre["prompt_id"]));}
                    if broker.is_some() {emit(&json!({"ready":true,"input":pre}))?;control("continue").await?;}
                    else {answer=Some(UnixStream::connect(root.join("answer.sock")).await?);}
                    if name=="remote" {if let Some(answer)=&mut answer {answer.write_all(b"South").await?;}}
                    else {key(&master,if name=="local" {b"\r"} else {b"\x1b"})?;}
                    sent=true;
                }
            }
            let results=if sent {recorded_results(&profile)} else {vec![]};
            if !results.is_empty() {
                ensure!(results.len()==1,"must be exactly one native tool result: {results:?}");
                let result=&results[0];
                let content=result["content"].as_str().context("result content")?;
                if name=="cancel" {
                    ensure!(result["is_error"]==true && content.to_lowercase().contains("rejected"));
                    if let Some(answer)=&mut answer {let mut b=[0];ensure!(answer.read(&mut b).await?==0,"cancel did not close hook");}
                    ensure!(child.try_wait()?.is_none()); break;
                }
                let expected=if name=="remote" {"South"} else {"North"};
                ensure!(content.contains(&format!("=\"{expected}\"")),"{result}");
                if root.join("PostToolUse.json").exists() {
                    let post=load(root.join("PostToolUse.json"))?;
                    ensure!(post["tool_use_id"]==TOOL_ID && post["session_id"]==pre["session_id"] && post["tool_input"]["questions"]==question()["questions"] && post["tool_response"]["answers"][QUESTION_TEXT]==expected);
                    if broker.is_none() && name=="local" && !late {
                        if let Some(answer)=&mut answer {match answer.write_all(b"South").await {Ok(())=>{},Err(e) if matches!(e.kind(),std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset)=>{},Err(e)=>return Err(e.into())}}
                        late=true;
                    }
                    let late_settled= if broker.is_none() && name=="local" {
                        if root.join("hook-returned.json").exists() {
                            let debug=std::fs::read_to_string(root.join("debug.log")).unwrap_or_default();
                            debug.contains("returned permissionDecision: allow") || debug.contains("cancelled")
                        } else {answer.as_ref().is_some_and(|s|matches!(s.try_read(&mut [0]),Ok(0)))}
                    } else {true};
                    let consumed:Vec<_>=requests.lock().unwrap().iter().flat_map(|r|self::results(r["messages"].as_array().map(Vec::as_slice).unwrap_or_default())).collect();
                    if late_settled && !consumed.is_empty() {
                        ensure!(consumed.iter().all(|r|r["content"].as_str().is_some_and(|s|s.contains(&format!("=\"{expected}\"")))));
                        ensure!(child.try_wait()?.is_none());break;
                    }
                }
            }
            let mut buffer=[0;65536];
            tokio::select! {
                n=pty_read(&master,&mut buffer)=> {let n=n?;ensure!(n>0,"native PTY closed");terminal.extend_from_slice(&buffer[..n]);}
                _=changes.recv()=>{}
                Some(result)=server.join_next()=>{anyhow::bail!("model server ended: {result:?}");}
            }
        }
        ensure!(sent && !recorded_results(&profile).is_empty());
        println!("PASS: interactive Claude {name}; exact native identity, local dialog and one result");
        std::io::stdout().flush()?;
        if broker.is_some() {control("finish").await?;}
        Ok::<_,anyhow::Error>(())
    }).await.context("timed out waiting for native question arbitration").and_then(|r|r);
    if outcome.is_err() {
        eprintln!(
            "{}",
            String::from_utf8_lossy(&terminal[terminal.len().saturating_sub(6000)..])
        );
    }
    child.terminate_async(Duration::from_secs(3)).await;
    server.abort_all();
    while server.join_next().await.is_some() {}
    outcome
}

pub async fn run(args: &[String]) -> Result<()> {
    let binary = args.first().context("missing native Claude binary")?;
    if args.get(1).is_some_and(|s| s == "--broker") {
        ensure!(
            args.len() == 6,
            "broker expects scenario, repo, clank and session"
        );
        scenario(binary, &args[2], Some(&args[3..])).await
    } else {
        for name in ["remote", "local", "cancel"] {
            scenario(binary, name, None).await?;
        }
        Ok(())
    }
}
