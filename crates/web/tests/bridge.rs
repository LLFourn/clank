use std::sync::Arc;

use async_trait::async_trait;
use clank_web::bridge::{
    Action, ActionResult, Backend, CallError, Session, SessionEvent, Snapshot, Update, apply_event,
    connect,
};
use tokio::sync::{Mutex, broadcast};

struct Repo {
    state: Mutex<Snapshot>,
    changes: broadcast::Sender<Update>,
    actions: Mutex<Vec<Action>>,
    uploads: Mutex<Vec<(String, Vec<u8>)>>,
}

impl Repo {
    fn new() -> Self {
        let (changes, _) = broadcast::channel(8);
        Self {
            state: Mutex::new(Snapshot {
                sequence: 0,
                state: serde_json::json!({"status":"ready"}),
            }),
            changes,
            actions: Mutex::new(Vec::new()),
            uploads: Mutex::new(Vec::new()),
        }
    }

    async fn publish(&self, value: serde_json::Value, advance: u64) {
        let mut state = self.state.lock().await;
        state.sequence += advance;
        state.state = value.clone();
        let _ = self.changes.send(Update {
            sequence: state.sequence,
            state: value,
        });
    }
}

#[async_trait]
impl Backend for Repo {
    async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>) {
        let receiver = self.changes.subscribe();
        let snapshot = self.state.lock().await.clone();
        (snapshot, receiver)
    }

    async fn dispatch(&self, action: Action) -> ActionResult {
        let echo = action.kind == "echo";
        let value = action.input.clone();
        self.actions.lock().await.push(action);
        if echo {
            return ActionResult::Completed { value };
        }
        ActionResult::Completed {
            value: serde_json::json!({"sent":true}),
        }
    }

    async fn upload(&self, media_type: String, bytes: Vec<u8>) -> ActionResult {
        let size = bytes.len();
        self.uploads.lock().await.push((media_type, bytes));
        ActionResult::Completed {
            value: serde_json::json!({"size":size}),
        }
    }
}

async fn serve_once(
    session: Arc<Session>,
) -> (String, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/session", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        session
            .accept(socket, |handle, credential| async move {
                handle == "repo-one" && credential == "secret"
            })
            .await
    });
    (endpoint, server)
}

async fn next(receiver: &mut broadcast::Receiver<SessionEvent>) -> SessionEvent {
    tokio::time::timeout(std::time::Duration::from_secs(2), receiver.recv())
        .await
        .expect("session event arrived")
        .expect("session event was retained")
}

async fn until(mut check: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !check().await {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("connection state changed");
}

#[tokio::test]
async fn one_stream_delivers_images_resync_actions_and_reconnect() {
    let session = Arc::new(Session::new("repo-one"));
    // Subscribe before the first image. The same receiver must report
    // its arrival, later resyncs, disconnection, and a new connector.
    let (mut view, mut events) = session.subscribe_with_snapshot().await;
    assert!(view.snapshot.is_none());
    let (endpoint, server) = serve_once(session.clone()).await;
    let repo = Arc::new(Repo::new());
    let client_repo = repo.clone();
    let client =
        tokio::spawn(async move { connect(&endpoint, "repo-one", "secret", client_repo).await });
    until(|| async { session.connected().await }).await;
    apply_event(&mut view, next(&mut events).await).unwrap();
    assert_eq!(
        view.snapshot.as_ref().unwrap().state,
        serde_json::json!({"status":"ready"})
    );

    repo.publish(serde_json::json!({"status":"busy"}), 1).await;
    let old_update = next(&mut events).await;
    apply_event(&mut view, old_update.clone()).unwrap();
    assert_eq!(
        view.snapshot.as_ref().unwrap().state,
        serde_json::json!({"status":"busy"})
    );
    // A repo-side sequence gap sends a replacement image to the same
    // receiver; a quiet resync cannot leave the old state displayed.
    repo.publish(serde_json::json!({"status":"resynced"}), 2)
        .await;
    apply_event(&mut view, next(&mut events).await).unwrap();
    assert_eq!(view.snapshot.as_ref().unwrap().sequence, 3);

    let action = Action {
        kind: "say".into(),
        input: serde_json::json!({"text":"hello"}),
    };
    assert!(matches!(
        session.call(action.clone()).await,
        Ok(ActionResult::Completed { .. })
    ));
    assert_eq!(repo.actions.lock().await.as_slice(), &[action]);

    client.abort();
    let _ = client.await;
    let result = server.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    apply_event(&mut view, next(&mut events).await).unwrap();
    assert_eq!(
        view.snapshot.as_ref().unwrap().sequence,
        3,
        "disconnect retains the last complete image"
    );
    assert!(!session.connected().await);
    assert!(!view.connected);
    assert_eq!(
        session
            .call(Action {
                kind: "say".into(),
                input: serde_json::Value::Null,
            })
            .await,
        Err(CallError::Disconnected)
    );
    assert_eq!(
        session
            .subscribe_with_snapshot()
            .await
            .0
            .snapshot
            .unwrap()
            .sequence,
        3
    );

    let (endpoint, server) = serve_once(session.clone()).await;
    let replacement = Arc::new(Repo::new());
    let new_repo = replacement.clone();
    let client =
        tokio::spawn(async move { connect(&endpoint, "repo-one", "secret", new_repo).await });
    apply_event(&mut view, next(&mut events).await).unwrap();
    assert_eq!(view.generation, 2);
    assert_eq!(view.snapshot.as_ref().unwrap().sequence, 0);
    let current = view.clone();
    apply_event(&mut view, old_update).unwrap();
    assert_eq!(view, current, "an old event cannot change the new image");
    replacement
        .publish(serde_json::json!({"status":"new session"}), 1)
        .await;
    apply_event(&mut view, next(&mut events).await).unwrap();
    assert_eq!(
        view.snapshot.as_ref().unwrap().state,
        serde_json::json!({"status":"new session"})
    );
    client.abort();
    let _ = client.await;
    let result = server.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn connector_admission_fails_closed() {
    let session = Arc::new(Session::new("repo-one"));
    let (endpoint, server) = serve_once(session.clone()).await;
    let attempted = connect(&endpoint, "repo-one", "wrong", Arc::new(Repo::new())).await;
    assert!(attempted.is_err());
    assert!(server.await.unwrap().is_err());
    assert!(!session.connected().await);
    assert!(session.subscribe_with_snapshot().await.0.snapshot.is_none());
}

#[tokio::test]
async fn binary_upload_crosses_the_connector_in_bounded_chunks() {
    let session = Arc::new(Session::new("repo-one"));
    let (endpoint, server) = serve_once(session.clone()).await;
    let repo = Arc::new(Repo::new());
    let client_repo = repo.clone();
    let client =
        tokio::spawn(async move { connect(&endpoint, "repo-one", "secret", client_repo).await });
    until(|| async { session.connected().await }).await;

    let bytes = vec![0xa5; 1024 * 1024 + 13];
    assert_eq!(
        session.upload("image/png".into(), bytes.clone()).await,
        Ok(ActionResult::Completed {
            value: serde_json::json!({"size":bytes.len()})
        })
    );
    assert_eq!(
        repo.uploads.lock().await.as_slice(),
        &[("image/png".into(), bytes)]
    );

    client.abort();
    let _ = client.await;
    let result = server.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        session.upload("image/png".into(), vec![1]).await,
        Err(CallError::Disconnected)
    );
    assert_eq!(repo.uploads.lock().await.len(), 1);
}

#[tokio::test]
async fn large_snapshot_has_no_logical_or_default_wincode_ceiling() {
    let session = Arc::new(Session::new("repo-one"));
    let (endpoint, server) = serve_once(session.clone()).await;
    let repo = Arc::new(Repo::new());
    let expected = serde_json::json!({"text":"x".repeat(5 * 1024 * 1024), "unicode":"🦀"});
    repo.state.lock().await.state = expected.clone();
    let client = tokio::spawn(async move { connect(&endpoint, "repo-one", "secret", repo).await });
    until(|| async { session.connected().await }).await;
    assert_eq!(
        session
            .subscribe_with_snapshot()
            .await
            .0
            .snapshot
            .unwrap()
            .state,
        expected
    );
    assert!(session.failure().await.is_none());
    client.abort();
    let _ = client.await;
    let result = server.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn reported_large_live_frame_is_delivered_and_retained_for_late_readers() {
    let session = Arc::new(Session::new("repo-one"));
    let (_, mut events) = session.subscribe_with_snapshot().await;
    let (endpoint, server) = serve_once(session.clone()).await;
    let repo = Arc::new(Repo::new());
    repo.state.lock().await.state = serde_json::json!({"replay":[],"live":[]});
    let client_repo = repo.clone();
    let client =
        tokio::spawn(async move { connect(&endpoint, "repo-one", "secret", client_repo).await });
    until(|| async { session.connected().await }).await;
    assert!(matches!(
        next(&mut events).await,
        SessionEvent::Snapshot { .. }
    ));
    // The exact synthetic shape that previously produced 1,082,662
    // bytes of outer JSON. A single frame is larger than the old cap.
    let data = serde_json::json!({"text":"x".repeat(1_082_559)});
    let frame = format!("event: status\ndata: {data}\n\n");
    let delta = serde_json::json!({"live":[{"event":"status","data":data}]});
    let old_wire = serde_json::json!({"type":"update","update":{"sequence":1,"state":{"live":[frame.clone()]}}});
    assert_eq!(serde_json::to_vec(&old_wire).unwrap().len(), 1_082_662);
    repo.publish(delta.clone(), 1).await;
    let SessionEvent::Update { update, .. } = next(&mut events).await else {
        panic!("live publication missing")
    };
    assert_eq!(update.state, delta);
    assert_eq!(
        session
            .subscribe_with_snapshot()
            .await
            .0
            .snapshot
            .unwrap()
            .state["replay"],
        delta["live"]
    );
    assert!(session.connected().await);
    assert!(session.failure().await.is_none());
    client.abort();
    let _ = client.await;
    let result = server.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn large_commands_and_results_use_the_same_framed_path() {
    let session = Arc::new(Session::new("repo-one"));
    let (endpoint, server) = serve_once(session.clone()).await;
    let repo = Arc::new(Repo::new());
    let client = tokio::spawn(async move { connect(&endpoint, "repo-one", "secret", repo).await });
    until(|| async { session.connected().await }).await;
    let value = serde_json::json!({"text":"\"\\\n🦀".repeat(700_000), "number":u64::MAX});
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.call(Action {
            kind: "echo".into(),
            input: value.clone(),
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(answer, ActionResult::Completed { value });
    client.abort();
    let _ = client.await;
    let result = server.await.unwrap();
    assert!(result.is_ok(), "{result:?}");
}

#[tokio::test]
async fn a_new_connector_classifies_an_old_hosts_upgrade_refusal() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}", listener.local_addr().unwrap());
    let host = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let Message::Text(hello) = socket.next().await.unwrap().unwrap() else {
            panic!("no JSON bootstrap")
        };
        let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
        assert_eq!(hello["version"], 2);
        socket
            .send(Message::Text(
                r#"{"type":"refused","why":"protocol version or handshake mismatch"}"#.into(),
            ))
            .await
            .unwrap();
    });
    let error = connect(&endpoint, "repo-one", "secret", Arc::new(Repo::new()))
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<clank_web::bridge::ProtocolMismatch>()
            .is_some(),
        "{error:#}"
    );
    assert!(error.to_string().contains("upgrade"));
    host.await.unwrap();
}
