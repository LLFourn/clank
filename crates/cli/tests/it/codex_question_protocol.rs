//! Acceptance against an installed Codex, with an isolated profile and a
//! localhost model fixture. No user credentials, external inference or real
//! agent session is used. Run explicitly:
//! CLANK_CODEX_PROTOCOL_BINARY=/path/to/codex cargo test -p clank --test it \
//!   codex_question_protocol:: -- --ignored

use std::collections::VecDeque;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::{TcpListener, UnixStream};
use tokio::task::JoinSet;
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const TEST_WAIT: Duration = Duration::from_secs(10);

struct Client {
    socket: WebSocketStream<UnixStream>,
    backlog: VecDeque<Value>,
    next: u64,
    sent: Vec<String>,
}

impl Client {
    async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path).await?;
        let (socket, _) = tokio_tungstenite::client_async("ws://localhost/", stream).await?;
        let mut client = Self {
            socket,
            backlog: VecDeque::new(),
            next: 0,
            sent: Vec::new(),
        };
        client
            .call(
                "initialize",
                json!({"clientInfo":{"name":"clank-question-proof","version":"0"},
            "capabilities":{"experimentalApi":true}}),
            )
            .await?;
        client
            .send(json!({"method":"initialized","params":{}}))
            .await?;
        Ok(client)
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        self.socket
            .send(Message::Text(message.to_string().into()))
            .await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<Value> {
        while let Some(message) = self.socket.next().await {
            if let Message::Text(text) = message? {
                return Ok(serde_json::from_str(&text)?);
            }
        }
        bail!("native connection closed");
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next += 1;
        // Client and server request IDs occupy different namespaces. A
        // native request carrying a numeric ID is never our RPC reply.
        let id = format!("clank-proof-{}", self.next);
        self.sent.push(method.into());
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        tokio::time::timeout(TEST_WAIT, async {
            loop {
                let message = self.receive().await?;
                if message.get("method").is_none() && message["id"] == id {
                    if let Some(error) = message.get("error") {
                        bail!("{method}: {error}");
                    }
                    return message
                        .get("result")
                        .cloned()
                        .ok_or_else(|| anyhow!("{method} has no result"));
                }
                // Native requests may precede the resume reply. Keep
                // them rather than throwing them away during RPC calls.
                self.backlog.push_back(message);
            }
        })
        .await
        .with_context(|| format!("waiting for {method}"))?
    }

    async fn wait(&mut self, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        if let Some(index) = self.backlog.iter().position(&matches) {
            return Ok(self.backlog.remove(index).unwrap());
        }
        tokio::time::timeout(TEST_WAIT, async {
            loop {
                let message = self.receive().await?;
                if matches(&message) {
                    return Ok(message);
                }
                self.backlog.push_back(message);
            }
        })
        .await
        .context("waiting for native question lifecycle event")?
    }

    async fn attach_loaded(&mut self, thread: &str, cwd: &Path) -> Result<()> {
        let loaded = self.call("thread/loaded/list", json!({})).await?;
        ensure!(
            loaded["data"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == thread)),
            "target is not already loaded"
        );
        let read = self
            .call(
                "thread/read",
                json!({"threadId":thread,"includeTurns":false}),
            )
            .await?;
        ensure!(
            read["thread"]["id"] == thread && read["thread"]["cwd"] == cwd.to_str().unwrap(),
            "loaded thread does not belong to the expected workspace"
        );
        self.call(
            "thread/resume",
            json!({"threadId":thread,"excludeTurns":true}),
        )
        .await?;
        Ok(())
    }

    async fn question(&mut self) -> Result<Value> {
        self.wait(|m| m["method"] == "item/tool/requestUserInput")
            .await
    }

    async fn answer(&mut self, request: &Value, answer: &str) -> Result<()> {
        self.send(
            json!({"id":request["id"],"result":{"answers":{"direction":{"answers":[answer]}}}}),
        )
        .await
    }

    async fn resolved(&mut self, request: &Value) -> Result<()> {
        self.wait(|m| {
            m["method"] == "serverRequest/resolved"
                && m["params"]["threadId"] == request["params"]["threadId"]
                && m["params"]["requestId"] == request["id"]
        })
        .await?;
        Ok(())
    }

    async fn completed(&mut self, turn: &Value) -> Result<()> {
        self.wait(|m| {
            m["method"] == "turn/completed" && m["params"]["turn"]["id"] == turn["turn"]["id"]
        })
        .await?;
        Ok(())
    }

    async fn close(mut self) -> Result<()> {
        self.socket.close(None).await?;
        Ok(())
    }
}

struct OwnedCodex {
    child: std::process::Child,
}

impl Drop for OwnedCodex {
    fn drop(&mut self) {
        // This process group is created exclusively for this test. Kill
        // and reap it on assertion/error too, including pending questions.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

struct Model {
    url: String,
    inputs: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Model {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Model {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let inputs = Arc::new(Mutex::new(Vec::new()));
        let captured = inputs.clone();
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((socket, _)) = accepted else { break };
                        let inputs = captured.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                                let inputs = inputs.clone();
                                async move {
                                    ensure!(req.uri().path() == "/v1/responses", "unexpected model endpoint");
                                    let body = req.into_body().collect().await?.to_bytes();
                                    let input: Value = serde_json::from_slice(&body)?;
                                    let count = {
                                        let mut inputs = inputs.lock().unwrap();
                                        inputs.push(input);
                                        inputs.len()
                                    };
                                    let id = format!("response-{count}");
                                    let item = if count % 2 == 1 {
                                        json!({"type":"function_call","id":format!("fc-{count}"),
                                            "call_id":format!("question-{count}"),"name":"request_user_input",
                                            "arguments":json!({"questions":[{"id":"direction","header":"Direction",
                                                "question":"Which direction?","options":[
                                                    {"label":"North","description":"First"},
                                                    {"label":"South","description":"Second"}]}]}).to_string()})
                                    } else {
                                        json!({"type":"message","id":format!("message-{count}"),"role":"assistant",
                                            "content":[{"type":"output_text","text":"Answered.","annotations":[]}]})
                                    };
                                    let events = [
                                        json!({"type":"response.created","response":{"id":id}}),
                                        json!({"type":"response.output_item.done","output_index":0,"item":item}),
                                        json!({"type":"response.completed","response":{"id":id,"status":"completed",
                                            "usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
                                    ];
                                    let body = events.iter().map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap())).collect::<String>();
                                    Ok::<_, anyhow::Error>(hyper::Response::builder()
                                        .header("content-type", "text/event-stream").body(Full::new(Bytes::from(body)))?)
                                }
                            });
                            let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(socket), service).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Ok(Self { url, inputs, task })
    }

    fn output(&self, call_id: &str) -> Result<String> {
        self.inputs
            .lock()
            .unwrap()
            .last()
            .and_then(|input| input["input"].as_array())
            .and_then(|items| {
                items.iter().find(|item| {
                    item["type"] == "function_call_output" && item["call_id"] == call_id
                })
            })
            .and_then(|item| item["output"].as_str())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("model did not receive result for {call_id}"))
    }
}

async fn start_codex(home: &Path, model: &Model) -> Result<(OwnedCodex, PathBuf)> {
    let binary = std::env::var_os("CLANK_CODEX_PROTOCOL_BINARY").ok_or_else(|| {
        anyhow!("set CLANK_CODEX_PROTOCOL_BINARY to an installed Codex executable")
    })?;
    ensure!(
        Path::new(&binary).is_absolute(),
        "Codex executable must be absolute"
    );
    let socket = home.join("control.sock");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "model = \"mock-model\"\nmodel_provider = \"mock\"\n[model_providers.mock]\nname = \"Mock\"\nbase_url = \"{}\"\nwire_api = \"responses\"\nrequires_openai_auth = false\nenv_key = \"CLANK_QUESTION_PROOF_KEY\"\n[features]\ndefault_mode_request_user_input = true\n",
            model.url
        ),
    )?;
    let mut command = std::process::Command::new(binary);
    command
        .args([
            "app-server",
            "--listen",
            &format!("unix://{}", socket.display()),
        ])
        .current_dir(home)
        .env_clear()
        .env("CODEX_HOME", home)
        .env("HOME", home)
        .env("CLANK_QUESTION_PROOF_KEY", "fixture-only")
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(home.join("server.stderr"))?)
        .process_group(0);
    let mut server = OwnedCodex {
        child: command.spawn()?,
    };
    tokio::time::timeout(TEST_WAIT, async {
        while !socket.exists() {
            ensure!(
                server.child.try_wait()?.is_none(),
                "test Codex exited during startup"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("starting isolated Codex")??;
    Ok((server, socket))
}

async fn turn(client: &mut Client, thread: &str, mode: &str) -> Result<Value> {
    client.call("turn/start", json!({"threadId":thread,"input":[{"type":"text","text":"Ask the fixture question."}],
        "collaborationMode":{"mode":mode,"settings":{"model":"mock-model","reasoning_effort":"medium","developer_instructions":null}}})).await
}

#[tokio::test]
#[ignore = "acceptance against an installed Codex; uses an isolated localhost model fixture"]
async fn installed_codex_replays_questions_and_arbitrates_native_answers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let home = temp.path().canonicalize()?;
    let model = Model::start().await?;
    let (_server, socket) = start_codex(&home, &model).await?;
    let mut terminal = Client::connect(&socket).await?;
    let started = terminal
        .call("thread/start", json!({"model":"mock-model","cwd":home}))
        .await?;
    let thread = started["thread"]["id"].as_str().context("thread id")?;
    let first_turn = turn(&mut terminal, thread, "plan").await?;
    let request = terminal.question().await?;
    ensure!(
        request["params"]["threadId"] == thread
            && request["params"]["turnId"] == first_turn["turn"]["id"]
    );
    ensure!(request["params"]["itemId"] == "question-1" && request["params"]["isBlocking"] == true);
    let mut remote = Client::connect(&socket).await?;
    let before = remote.sent.iter().filter(|m| *m == "thread/resume").count();
    ensure!(
        remote
            .attach_loaded("00000000-0000-0000-0000-000000000000", &home)
            .await
            .is_err()
    );
    ensure!(
        remote
            .attach_loaded(thread, &home.join("another-repo"))
            .await
            .is_err()
    );
    ensure!(
        remote.sent.iter().filter(|m| *m == "thread/resume").count() == before,
        "a foreign target was resumed"
    );
    remote.attach_loaded(thread, &home).await?;
    ensure!(
        remote.question().await? == request,
        "replay must preserve native request id and content"
    );
    remote.close().await?;
    let mut remote = Client::connect(&socket).await?;
    remote.attach_loaded(thread, &home).await?;
    ensure!(
        remote.question().await? == request,
        "disconnect must not cancel the provider question"
    );
    terminal.answer(&request, "North").await?;
    remote.resolved(&request).await?;
    remote.answer(&request, "South").await?;
    terminal.resolved(&request).await?;
    terminal.completed(&first_turn).await?;
    let output = model.output("question-1")?;
    ensure!(
        output.contains("North") && !output.contains("South"),
        "first native response must win: {output}"
    );

    let next_turn = turn(&mut terminal, thread, "default").await?;
    let background = terminal.question().await?;
    ensure!(
        background["params"]["isBlocking"] == false,
        "default-mode input is a background question"
    );
    let replay = remote.question().await?;
    ensure!(replay == background);
    remote.answer(&background, "South").await?;
    terminal.resolved(&background).await?;
    remote.resolved(&background).await?;
    terminal.completed(&next_turn).await?;
    ensure!(model.output("question-3")?.contains("South"));

    let cancelled_turn = turn(&mut terminal, thread, "plan").await?;
    let cancelled = terminal.question().await?;
    ensure!(remote.question().await? == cancelled);
    terminal
        .call(
            "turn/interrupt",
            json!({"threadId":thread,"turnId":cancelled_turn["turn"]["id"]}),
        )
        .await?;
    remote.resolved(&cancelled).await?;
    terminal.completed(&cancelled_turn).await?;
    // A cleanup emits the same resolved notification as an answer. It
    // must not be interpreted as confirmation of this client's answer.
    remote.close().await?;
    terminal.close().await?;
    Ok(())
}
