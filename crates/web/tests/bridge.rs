use std::sync::Arc;

use async_trait::async_trait;
use clank_web::bridge::{
    Action, ActionResult, Backend, CallError, Session, SessionEvent, Snapshot, SnapshotTooLarge,
    Update, UpdateTooLarge, apply_event, connect,
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
        self.actions.lock().await.push(action);
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
    assert!(server.await.unwrap().is_ok());
    apply_event(&mut view, next(&mut events).await).unwrap();
    assert!(view.snapshot.is_none(), "disconnect clears the old image");
    assert_eq!(
        session
            .call(Action {
                kind: "say".into(),
                input: serde_json::Value::Null,
            })
            .await,
        Err(CallError::Disconnected)
    );
    assert!(session.subscribe_with_snapshot().await.0.snapshot.is_none());

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
    assert!(server.await.unwrap().is_ok());
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
    assert!(server.await.unwrap().is_ok());
    assert_eq!(
        session.upload("image/png".into(), vec![1]).await,
        Err(CallError::Disconnected)
    );
    assert_eq!(repo.uploads.lock().await.len(), 1);
}

#[tokio::test]
async fn oversized_snapshot_is_reported_to_both_connector_and_host() {
    let session = Arc::new(Session::new("repo-one"));
    let (endpoint, server) = serve_once(session.clone()).await;
    let repo = Arc::new(Repo::new());
    repo.state.lock().await.state = serde_json::json!({"text":"x".repeat(1024 * 1024)});

    let error = connect(&endpoint, "repo-one", "secret", repo)
        .await
        .expect_err("an oversized image cannot attach");
    assert!(
        error.downcast_ref::<SnapshotTooLarge>().is_some(),
        "{error:#}"
    );
    assert!(server.await.unwrap().is_err());
    assert_eq!(
        session
            .failure()
            .await
            .as_ref()
            .map(|failure| failure.code.as_str()),
        Some("snapshot_too_large")
    );
    assert!(!session.connected().await);
}

#[tokio::test]
async fn oversized_live_update_is_reported_to_both_connector_and_host() {
    let session = Arc::new(Session::new("repo-one"));
    let (endpoint, server) = serve_once(session.clone()).await;
    let repo = Arc::new(Repo::new());
    let client_repo = repo.clone();
    let client =
        tokio::spawn(async move { connect(&endpoint, "repo-one", "secret", client_repo).await });
    until(|| async { session.connected().await }).await;

    repo.publish(serde_json::json!({"text":"x".repeat(1024 * 1024)}), 1)
        .await;
    let error = client
        .await
        .unwrap()
        .expect_err("an oversized update cannot publish");
    assert!(
        error.downcast_ref::<UpdateTooLarge>().is_some(),
        "{error:#}"
    );
    assert!(server.await.unwrap().is_err());
    assert_eq!(
        session
            .failure()
            .await
            .as_ref()
            .map(|failure| failure.code.as_str()),
        Some("update_too_large")
    );
    assert!(!session.connected().await);
}
