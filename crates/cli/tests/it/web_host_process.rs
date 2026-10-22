use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use clank_web::bridge::{Action, ActionResult, Backend, Snapshot, Update};
use clank_web::request::{BrowserReply, BrowserRequest, Content};
use clank_web::service::BrowserFrames;
use clank_web_cloudflare::door::Door;
use futures_util::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, broadcast};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[tokio::test]
async fn failed_readiness_after_tunnel_proof_stops_the_live_tunnel() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("tunnel.pid");
    let stopped_file = home.path().join("tunnel.stopped");
    let script = format!(
        "echo $$ > '{}'; echo http://127.0.0.1:{{port}}; trap 'sleep 0.7; echo graceful > \"{}\"; exit 0' TERM; while :; do sleep 0.05; done",
        pid_file.display(),
        stopped_file.display()
    );
    std::fs::create_dir_all(home.path().join(".clank")).unwrap();
    std::fs::write(
        home.path().join(".clank/config.json"),
        serde_json::json!({"remote":{"tunnel":{"provider":"command","run":["/bin/sh","-c",script]}}}).to_string(),
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_clank-web-cloudflare"))
        .env("HOME", home.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"{\"session\":\"team\",\"port\":0,\"tunnel\":true}\n")
        .await
        .unwrap();
    // Keep stdin open so setup reaches the readiness write. Closing
    // stdout makes that write fail after the tunnel has been proven.
    drop(child.stdout.take());
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    drop(input);
    assert!(pid_file.exists(), "the command tunnel must have started");
    assert_eq!(
        std::fs::read_to_string(stopped_file).unwrap(),
        "graceful\n",
        "failed readiness must await the tunnel's graceful stop"
    );
}

struct Repo {
    root: PathBuf,
    snapshot: Mutex<Snapshot>,
    sequence: AtomicU64,
    updates: broadcast::Sender<Update>,
    actions: Mutex<Vec<BrowserRequest>>,
    uploads: Mutex<Vec<(String, Vec<u8>)>>,
}

impl Repo {
    fn new(root: PathBuf) -> Self {
        let (updates, _) = broadcast::channel(8);
        Self {
            root,
            snapshot: Mutex::new(Snapshot {
                sequence: 0,
                state: serde_json::to_value(BrowserFrames {
                    replay: vec![
                        frame("panes", serde_json::json!({"panes":[{"id":"terminal_1"}],"closed":[]})),
                        frame("status", serde_json::json!({"phase":"ready","facts":{"agents":[{"label":"grok"}]}})),
                        frame("turns", serde_json::json!({"agent":"grok","session":"s1","generation":1,"turns":[{"id":"old-turn"}]})),
                        frame("pane", serde_json::json!({"pane_id":"terminal_1","screen":"old terminal"})),
                    ],
                    live: vec![],
                })
                .unwrap(),
            }),
            sequence: AtomicU64::new(0),
            updates,
            actions: Mutex::new(Vec::new()),
            uploads: Mutex::new(Vec::new()),
        }
    }

    fn publish(&self, event: &str, value: serde_json::Value) {
        let _ = self.updates.send(Update {
            sequence: self.sequence.fetch_add(1, Ordering::SeqCst) + 1,
            state: serde_json::json!({"live":[frame(event, value)]}),
        });
    }

    async fn rebind(&self, session: &str, generation: u32, turn: &str) {
        let reset = frame(
            "turns",
            serde_json::json!({
                "agent":"grok", "session":session, "generation":generation,
                "turns":[{"id":turn}]
            }),
        );
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let mut snapshot = self.snapshot.lock().await;
        snapshot.sequence = sequence;
        snapshot.state = serde_json::to_value(BrowserFrames {
            replay: vec![
                frame(
                    "panes",
                    serde_json::json!({"panes":[{"id":"terminal_1"}],"closed":[]}),
                ),
                frame(
                    "status",
                    serde_json::json!({"phase":"ready","facts":{"agents":[{"label":"grok"}]}}),
                ),
                reset.clone(),
                frame(
                    "pane",
                    serde_json::json!({"pane_id":"terminal_1","screen":"old terminal"}),
                ),
            ],
            live: vec![],
        })
        .unwrap();
        let _ = self.updates.send(Update {
            sequence,
            state: serde_json::json!({"live":[reset]}),
        });
    }
}

fn frame(event: &str, value: serde_json::Value) -> clank_web::service::BrowserFrame {
    clank_web::service::BrowserFrame::new(event, value)
}

#[async_trait]
impl Backend for Repo {
    async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>) {
        let updates = self.updates.subscribe();
        (self.snapshot.lock().await.clone(), updates)
    }

    async fn dispatch(&self, action: Action) -> ActionResult {
        let request = BrowserRequest::from_action(action).unwrap();
        let hangs = matches!(&request, BrowserRequest::Say { text, .. } if text == "hang");
        self.actions.lock().await.push(request.clone());
        if hangs {
            return std::future::pending::<ActionResult>().await;
        }
        let reply = match request {
            BrowserRequest::Site { path } => BrowserReply {
                status: 200,
                content: Content::Html,
                body: std::fs::read_to_string(self.root.join("site").join(path)).unwrap(),
            },
            BrowserRequest::AgentCandidates => BrowserReply {
                status: 200,
                content: Content::Json,
                body: "[]".into(),
            },
            _ => BrowserReply {
                status: 200,
                content: Content::Json,
                body: r#"{"from":"repo"}"#.into(),
            },
        };
        ActionResult::Completed {
            value: reply.into_value(),
        }
    }

    async fn upload(&self, media_type: String, bytes: Vec<u8>) -> ActionResult {
        std::fs::write(self.root.join("attachment.bin"), &bytes).unwrap();
        let hangs = media_type == "application/x-hang";
        self.uploads.lock().await.push((media_type, bytes));
        if hangs {
            return std::future::pending::<ActionResult>().await;
        }
        ActionResult::Completed {
            value: BrowserReply {
                status: 200,
                content: Content::Json,
                body: serde_json::json!({"path":self.root.join("attachment.bin")}).to_string(),
            }
            .into_value(),
        }
    }
}

#[tokio::test]
async fn a_crashed_child_closes_the_connector_without_stopping_the_repo_backend() {
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_clank-web-cloudflare"))
        .env("HOME", home.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"{\"session\":\"team\",\"port\":0,\"tunnel\":false}\n")
        .await
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
    let endpoint = ready["connector_url"].as_str().unwrap().to_owned();
    let credential = ready["credential"].as_str().unwrap().to_owned();
    let repo = Arc::new(Repo::new(root.path().to_path_buf()));
    let connector_repo = repo.clone();
    let (attached, mut attachment) =
        tokio::sync::watch::channel(clank_web::bridge::ConnectionStatus::Connecting);
    let connector = tokio::spawn(async move {
        clank_web::bridge::connect_signaled(
            &endpoint,
            "team",
            &credential,
            connector_repo,
            Some(attached),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), attachment.changed())
        .await
        .unwrap()
        .expect("repo attached before the crash");
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), connector)
        .await
        .expect("host crash closes the connector")
        .unwrap()
        .unwrap();
    drop(input);
    let request = BrowserRequest::Say {
        pane: "terminal_1".into(),
        label: "grok".into(),
        session: None,
        text: "local work continues".into(),
    };
    assert!(matches!(
        repo.dispatch(request.clone().into_action()).await,
        ActionResult::Completed { .. }
    ));
    assert_eq!(repo.actions.lock().await.as_slice(), &[request]);
}

#[tokio::test]
async fn a_child_host_serves_an_attached_repo_without_the_repo_directory() {
    feature_parity().await;
}

#[tokio::test]
async fn an_existing_host_is_ready_before_repo_attachment() {
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_clank-web-cloudflare"))
        .env("HOME", home.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"{\"session\":\"team\",\"port\":0,\"tunnel\":false}\n")
        .await
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
    let local_url = ready["local_url"].as_str().unwrap();
    assert!(
        reqwest::get(format!("{local_url}/instance"))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let cookie = Door::new(Some(home.path().to_path_buf()))
        .open_session("existing")
        .unwrap();
    let status: serde_json::Value = reqwest::Client::new()
        .get(format!("{local_url}/bridge/status"))
        .header(
            "cookie",
            format!("{}={cookie}", clank_web_cloudflare::door::COOKIE),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["connected"], false);
    drop(input);
    tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
}

async fn feature_parity() {
    let home = tempfile::tempdir().unwrap();
    let unrelated = tempfile::tempdir().unwrap();
    let repo_root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo_root.path().join("site/plan")).unwrap();
    std::fs::write(
        repo_root.path().join("site/plan/plan.html"),
        "<h1>repo plan</h1>",
    )
    .unwrap();
    let token = Door::new(Some(home.path().to_path_buf())).token().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_clank-web-cloudflare"))
        .current_dir(unrelated.path())
        .env("HOME", home.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input
        .write_all(b"{\"session\":\"team\",\"port\":0,\"tunnel\":false}\n")
        .await
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let ready: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert!(ready.get("error").is_none(), "{ready}");
    let local_url = ready["local_url"].as_str().unwrap();
    let endpoint = ready["connector_url"].as_str().unwrap().to_owned();
    let credential = ready["credential"].as_str().unwrap().to_owned();
    let admin = ready["admin_credential"].as_str().unwrap().to_owned();
    assert_ne!(
        admin, credential,
        "repo attachment is not host administration"
    );
    let client = reqwest::Client::new();
    for denied in [None, Some(credential.as_str())] {
        let mut request = client.get(format!("{local_url}/admin/state"));
        if let Some(capability) = denied {
            request = request.header("x-clank-admin", capability);
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
    }
    let admin_state: serde_json::Value = client
        .get(format!("{local_url}/admin/state"))
        .header("x-clank-admin", &admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(admin_state["token"], token);

    let link: serde_json::Value = client
        .post(format!("{local_url}/admin/link"))
        .header("x-clank-admin", &admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let link = link["token"].as_str().unwrap();
    let no_redirect = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let linked = no_redirect
        .get(format!("{local_url}/login?t={link}"))
        .send()
        .await
        .unwrap();
    assert_eq!(linked.status(), reqwest::StatusCode::SEE_OTHER);
    assert!(linked.headers().contains_key("set-cookie"));
    let spent = no_redirect
        .get(format!("{local_url}/login?t={link}"))
        .send()
        .await
        .unwrap();
    assert_eq!(spent.headers().get("location").unwrap(), "/login?why=link");

    let login = client
        .post(format!("{local_url}/login"))
        .header("origin", local_url)
        .json(&serde_json::json!({"token":token}))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), reqwest::StatusCode::NO_CONTENT);
    let cookie = login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(
        client
            .get(format!("{local_url}/admin/state"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a browser session cannot administer the host"
    );

    let repo = Arc::new(Repo::new(repo_root.path().to_path_buf()));
    assert!(
        clank_web::bridge::connect(&endpoint, "team", "not-the-connector-secret", repo.clone(),)
            .await
            .is_err(),
        "a browser cookie cannot admit a repo connector"
    );
    let connector_repo = repo.clone();
    let reconnect_credential = credential.clone();
    let reconnect_endpoint = endpoint.clone();
    let connector = tokio::spawn(async move {
        clank_web::bridge::connect(&endpoint, "team", &credential, connector_repo).await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status: serde_json::Value = client
                .get(format!("{local_url}/bridge/status"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["connected"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut browser = format!("{}/events", local_url.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    browser
        .headers_mut()
        .insert("cookie", cookie.parse().unwrap());
    browser
        .headers_mut()
        .insert("origin", local_url.parse().unwrap());
    let (mut browser, _) = tokio_tungstenite::connect_async(browser).await.unwrap();
    let mut initial = Vec::new();
    for _ in 0..4 {
        let message = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            next_page_frame(&mut browser),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        initial.push(message.to_text().unwrap().to_owned());
    }
    assert!(initial.iter().any(|frame| frame.contains("ready")));
    assert!(initial.iter().any(|frame| frame.contains("old-turn")));
    assert!(initial.iter().any(|frame| frame.contains("old terminal")));
    repo.publish(
        "pane",
        serde_json::json!({"pane_id":"terminal_1","screen":"live terminal"}),
    );
    let live = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        next_page_frame(&mut browser),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert!(live.to_text().unwrap().contains("live terminal"));

    // A reread replaces a transcript window, then a new harness session
    // replaces it again. Both changes cross the connector as live frames.
    repo.rebind("s1", 2, "rolled-turn").await;
    let rolled = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        next_page_frame(&mut browser),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap()
    .to_text()
    .unwrap()
    .to_owned();
    assert!(rolled.contains("rolled-turn") && rolled.contains("\"generation\":2"));
    assert!(!rolled.contains("old-turn"));
    repo.rebind("s2", 0, "new-session-turn").await;
    let rebound = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        next_page_frame(&mut browser),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap()
    .to_text()
    .unwrap()
    .to_owned();
    assert!(rebound.contains("new-session-turn") && rebound.contains("\"session\":\"s2\""));
    assert!(!rebound.contains("rolled-turn"));

    let mut later_request = format!("{}/events", local_url.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    later_request
        .headers_mut()
        .insert("cookie", cookie.parse().unwrap());
    later_request
        .headers_mut()
        .insert("origin", local_url.parse().unwrap());
    let (mut later, _) = tokio_tungstenite::connect_async(later_request)
        .await
        .unwrap();
    let mut retained = Vec::new();
    for _ in 0..4 {
        retained.push(
            tokio::time::timeout(std::time::Duration::from_secs(5), later.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .to_text()
                .unwrap()
                .to_owned(),
        );
    }
    assert!(
        retained
            .iter()
            .any(|frame| frame.contains("new-session-turn"))
    );
    assert!(
        !retained
            .iter()
            .any(|frame| frame.contains("old-turn") || frame.contains("rolled-turn"))
    );
    drop(later);

    let routes = [
        (
            "say",
            serde_json::json!({"pane":"terminal_1","label":"grok","session":"old-session","text":"hello"}),
            BrowserRequest::Say {
                pane: "terminal_1".into(),
                label: "grok".into(),
                session: Some("old-session".into()),
                text: "hello".into(),
            },
        ),
        (
            "stop",
            serde_json::json!({"pane":"terminal_1","label":"grok","session":"old-session"}),
            BrowserRequest::Stop {
                pane: "terminal_1".into(),
                label: "grok".into(),
                session: Some("old-session".into()),
            },
        ),
        (
            "queue",
            serde_json::json!({"do":"save","handle":"plan","revision":"r1","body":"new"}),
            BrowserRequest::Queue {
                ask: serde_json::json!({"do":"save","handle":"plan","revision":"r1","body":"new"}),
            },
        ),
        (
            "stash",
            serde_json::json!({"do":"restore","handle":"stash"}),
            BrowserRequest::Stash {
                ask: serde_json::json!({"do":"restore","handle":"stash"}),
            },
        ),
        (
            "agents",
            serde_json::json!({"do":"auto","label":"grok","enabled":true}),
            BrowserRequest::Agent {
                ask: serde_json::json!({"do":"auto","label":"grok","enabled":true}),
            },
        ),
        (
            "blocks/answer",
            serde_json::json!({"agent":"grok","name":"question",
                "occurrence":"0123456789abcdef0123456789abcdef","question":"expected?","answer":"yes"}),
            BrowserRequest::BlockAnswer {
                agent: "grok".into(),
                name: "question".into(),
                occurrence: Some("0123456789abcdef0123456789abcdef".into()),
                question: "expected?".into(),
                answer: "yes".into(),
            },
        ),
        (
            "agents/reopen",
            serde_json::json!({"label":"grok"}),
            BrowserRequest::Reopen {
                label: "grok".into(),
            },
        ),
    ];
    let mut expected = Vec::new();
    for (path, body, request) in routes {
        let answer = client
            .post(format!("{local_url}/{path}"))
            .header("cookie", &cookie)
            .header("origin", local_url)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(answer.status(), reqwest::StatusCode::OK, "{path}");
        assert_eq!(
            answer.json::<serde_json::Value>().await.unwrap()["from"],
            "repo"
        );
        expected.push(request);
    }
    let page = client
        .get(format!("{local_url}/html/plan/plan.html"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), reqwest::StatusCode::OK);
    assert_eq!(page.text().await.unwrap(), "<h1>repo plan</h1>");
    expected.push(BrowserRequest::Site {
        path: "plan/plan.html".into(),
    });
    let candidates = client
        .get(format!("{local_url}/agents/candidates"))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(candidates.status(), reqwest::StatusCode::OK);
    assert_eq!(
        candidates.json::<serde_json::Value>().await.unwrap(),
        serde_json::json!([])
    );
    expected.push(BrowserRequest::AgentCandidates);
    assert_eq!(*repo.actions.lock().await, expected);

    let bytes = vec![0xa5; clank_web::bridge::BULK_CHUNK_BYTES + 17];
    let attachment: serde_json::Value = client
        .post(format!("{local_url}/upload"))
        .header("cookie", &cookie)
        .header("origin", local_url)
        .header("content-type", "image/png")
        .body(bytes.clone())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        attachment["path"],
        repo_root.path().join("attachment.bin").to_str().unwrap()
    );
    assert_eq!(
        std::fs::read(repo_root.path().join("attachment.bin")).unwrap(),
        bytes
    );
    assert_eq!(
        repo.uploads.lock().await.as_slice(),
        &[("image/png".into(), bytes)]
    );
    assert!(!unrelated.path().join("attachment.bin").exists());

    let unauthenticated = client
        .post(format!("{local_url}/say"))
        .header("origin", local_url)
        .json(&serde_json::json!({"pane":"terminal_1","label":"grok","text":"denied"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);
    let cross_origin = client
        .post(format!("{local_url}/say"))
        .header("cookie", &cookie)
        .header("origin", "https://other.example")
        .json(&serde_json::json!({"pane":"terminal_1","label":"grok","text":"denied"}))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), reqwest::StatusCode::FORBIDDEN);
    assert_eq!(*repo.actions.lock().await, expected);

    drop(browser);
    connector.abort();
    let _ = connector.await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status: serde_json::Value = client
                .get(format!("{local_url}/bridge/status"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["connected"] == false {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let while_disconnected = client
        .post(format!("{local_url}/say"))
        .header("cookie", &cookie)
        .header("origin", local_url)
        .json(&serde_json::json!({"pane":"terminal_1","label":"grok","text":"not delivered"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        while_disconnected.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(*repo.actions.lock().await, expected);
    let restart = || {
        let endpoint = reconnect_endpoint.clone();
        let credential = reconnect_credential.clone();
        let repo = repo.clone();
        tokio::spawn(async move {
            clank_web::bridge::connect(&endpoint, "team", &credential, repo).await
        })
    };
    let mut reconnected = restart();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status: serde_json::Value = client
                .get(format!("{local_url}/bridge/status"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["connected"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let mut browser_request = format!("{}/events", local_url.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    browser_request
        .headers_mut()
        .insert("cookie", cookie.parse().unwrap());
    browser_request
        .headers_mut()
        .insert("origin", local_url.parse().unwrap());
    let (mut browser, _) = tokio_tungstenite::connect_async(browser_request)
        .await
        .unwrap();
    let mut replay = Vec::new();
    for _ in 0..4 {
        let message = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            next_page_frame(&mut browser),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        replay.push(message.to_text().unwrap().to_owned());
    }
    assert!(replay.iter().any(|frame| frame.contains("old terminal")));
    assert!(!replay.iter().any(|frame| frame.contains("live terminal")));
    assert!(
        replay
            .iter()
            .any(|frame| frame.contains("new-session-turn"))
    );
    assert!(
        !replay
            .iter()
            .any(|frame| frame.contains("old-turn") || frame.contains("rolled-turn"))
    );
    assert_eq!(
        *repo.actions.lock().await,
        expected,
        "reconnect did not replay mutations"
    );

    drop(browser);
    let pending_client = client.clone();
    let pending_url = local_url.to_owned();
    let pending_cookie = cookie.clone();
    let pending_say = tokio::spawn(async move {
        pending_client
            .post(format!("{pending_url}/say"))
            .header("cookie", pending_cookie)
            .header("origin", pending_url.clone())
            .json(&serde_json::json!({"pane":"terminal_1","label":"grok","text":"hang"}))
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while repo.actions.lock().await.len() != expected.len() + 1 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    reconnected.abort();
    let _ = reconnected.await;
    let unknown = tokio::time::timeout(std::time::Duration::from_secs(5), pending_say)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unknown.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(unknown.text().await.unwrap().contains("outcome_unknown"));
    reconnected = restart();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status: serde_json::Value = client
                .get(format!("{local_url}/bridge/status"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["connected"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(repo.actions.lock().await.len(), expected.len() + 1);

    let pending_client = client.clone();
    let pending_url = local_url.to_owned();
    let pending_cookie = cookie.clone();
    let pending_upload = tokio::spawn(async move {
        pending_client
            .post(format!("{pending_url}/upload"))
            .header("cookie", pending_cookie)
            .header("origin", pending_url.clone())
            .header("content-type", "application/x-hang")
            .body(vec![0x7f; clank_web::bridge::BULK_CHUNK_BYTES + 1])
            .send()
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while repo.uploads.lock().await.len() != 2 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    reconnected.abort();
    let _ = reconnected.await;
    let unknown = tokio::time::timeout(std::time::Duration::from_secs(5), pending_upload)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unknown.status(), reqwest::StatusCode::BAD_GATEWAY);
    assert!(unknown.text().await.unwrap().contains("outcome_unknown"));
    reconnected = restart();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status: serde_json::Value = client
                .get(format!("{local_url}/bridge/status"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if status["connected"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        repo.uploads.lock().await.len(),
        2,
        "reconnect did not replay upload"
    );

    let mut browser_request = format!("{}/events", local_url.replace("http://", "ws://"))
        .into_client_request()
        .unwrap();
    browser_request
        .headers_mut()
        .insert("cookie", cookie.parse().unwrap());
    browser_request
        .headers_mut()
        .insert("origin", local_url.parse().unwrap());
    let (mut browser, _) = tokio_tungstenite::connect_async(browser_request)
        .await
        .unwrap();
    for _ in 0..4 {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            next_page_frame(&mut browser),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    }

    let state: serde_json::Value = client
        .get(format!("{local_url}/admin/state"))
        .header("x-clank-admin", &admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for session in state["sessions"].as_array().unwrap() {
        let revoked = client
            .post(format!("{local_url}/admin/revoke"))
            .header("x-clank-admin", &admin)
            .json(&serde_json::json!({"id_hash":session["id_hash"]}))
            .send()
            .await
            .unwrap();
        assert!(revoked.status().is_success());
    }
    let closed = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        next_page_frame(&mut browser),
    )
    .await
    .unwrap();
    assert!(!matches!(
        closed,
        Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
    ));

    let rotated: serde_json::Value = client
        .post(format!("{local_url}/admin/rotate"))
        .header("x-clank-admin", &admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(rotated["token"], token);

    drop(input);
    tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    let _ = reconnected.await;
}

async fn next_page_frame(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Option<Result<tokio_tungstenite::tungstenite::Message, tokio_tungstenite::tungstenite::Error>>
{
    while let Some(message) = socket.next().await {
        if matches!(&message, Ok(tokio_tungstenite::tungstenite::Message::Text(text)) if text.starts_with("event: connection\n"))
        {
            continue;
        }
        return Some(message);
    }
    None
}
