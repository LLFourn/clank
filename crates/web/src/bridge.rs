//! A bounded, versioned connection between a repo process and a web host.
//!
//! The repo process initiates the WebSocket. A host may use this module
//! with its own listener and authentication policy. No repo filesystem
//! or harness access exists on this side of the connection.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

pub const VERSION: u32 = 1;
pub const MAX_CONTROL_BYTES: usize = 1024 * 1024;
pub const MAX_UPLOAD_BYTES: usize = 12 * 1024 * 1024;
pub const BULK_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct BridgeFailure {
    pub code: String,
    pub why: String,
}

#[derive(Debug)]
pub struct SnapshotTooLarge {
    pub bytes: usize,
}

#[derive(Debug)]
pub struct UpdateTooLarge {
    pub bytes: usize,
}

impl std::fmt::Display for SnapshotTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session snapshot is {} bytes, above the {}-byte bridge limit; reduce retained content before reconnecting",
            self.bytes, MAX_CONTROL_BYTES
        )
    }
}

impl std::error::Error for SnapshotTooLarge {}

impl std::fmt::Display for UpdateTooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "session update is {} bytes, above the {}-byte bridge limit; reduce the content of this update before reconnecting",
            self.bytes, MAX_CONTROL_BYTES
        )
    }
}

impl std::error::Error for UpdateTooLarge {}

/// A state image and its last included update number. The backend must
/// return this together with a subscription that cannot miss later updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub sequence: u64,
    pub state: serde_json::Value,
}

/// A subscriber's authoritative position. The revision orders every
/// host publication, including a disconnect with no image. A browser
/// can discard already included events from its receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    pub revision: u64,
    pub generation: u64,
    pub snapshot: Option<Snapshot>,
}

/// A state change. Browser feed updates may contain only new live frames;
/// the host merges those into the authoritative replay for late browsers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Update {
    pub sequence: u64,
    pub state: serde_json::Value,
}

/// Every state transition uses one ordered stream. A subscriber that
/// began before the first snapshot, or lives through a reconnect,
/// receives the image change on the receiver it already holds.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    Snapshot {
        revision: u64,
        generation: u64,
        snapshot: Snapshot,
    },
    Update {
        revision: u64,
        generation: u64,
        update: Update,
    },
    Disconnected {
        revision: u64,
        generation: u64,
    },
}

/// Apply the stream to one browser's view. A newer generation replaces
/// the old image; an update cannot cross that boundary. `Err` means a
/// broadcast gap, so the caller should subscribe with a fresh image.
pub fn apply_event(view: &mut SessionView, event: SessionEvent) -> Result<(), ()> {
    let revision = match &event {
        SessionEvent::Snapshot { revision, .. }
        | SessionEvent::Update { revision, .. }
        | SessionEvent::Disconnected { revision, .. } => *revision,
    };
    if revision <= view.revision {
        return Ok(());
    }
    if view.revision.checked_add(1) != Some(revision) {
        return Err(());
    }
    match event {
        SessionEvent::Snapshot {
            generation,
            snapshot,
            ..
        } => {
            if generation < view.generation {
                return Err(());
            }
            view.generation = generation;
            view.snapshot = Some(snapshot);
        }
        SessionEvent::Update {
            generation, update, ..
        } => {
            let Some(previous) = view.snapshot.as_mut() else {
                return Err(());
            };
            if view.generation != generation
                || previous.sequence.checked_add(1) != Some(update.sequence)
            {
                return Err(());
            }
            let state = crate::replay::merge(&previous.state, &update.state).map_err(|_| ())?;
            *previous = Snapshot {
                sequence: update.sequence,
                state,
            };
        }
        SessionEvent::Disconnected { generation, .. } => {
            if generation < view.generation {
                return Err(());
            }
            view.generation = generation;
            view.snapshot = None;
        }
    }
    view.revision = revision;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Action {
    pub kind: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ActionResult {
    Completed { value: serde_json::Value },
    Refused { why: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Wire {
    Hello {
        version: u32,
        session: String,
        credential: String,
    },
    Ready {
        version: u32,
    },
    Refused {
        why: String,
    },
    Failure {
        code: String,
        why: String,
    },
    Snapshot {
        snapshot: Snapshot,
    },
    Applied {
        sequence: u64,
    },
    Update {
        update: Update,
    },
    Request {
        id: u64,
        action: Action,
    },
    UploadBegin {
        id: u64,
        media_type: String,
        size: usize,
    },
    UploadEnd {
        id: u64,
    },
    Result {
        id: u64,
        result: ActionResult,
    },
}

enum Incoming {
    Control(Wire),
    Binary(Vec<u8>),
}

enum Outgoing {
    Control(Wire),
    Binary(Vec<u8>),
}

async fn send<S>(socket: &mut S, wire: &Wire) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let text = serde_json::to_string(wire)?;
    anyhow::ensure!(text.len() <= MAX_CONTROL_BYTES, "control message too large");
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}

async fn send_snapshot<S>(socket: &mut S, snapshot: Snapshot) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let wire = Wire::Snapshot { snapshot };
    let bytes = serde_json::to_vec(&wire)?.len();
    if bytes > MAX_CONTROL_BYTES {
        let error = SnapshotTooLarge { bytes };
        let _ = send(
            socket,
            &Wire::Failure {
                code: "snapshot_too_large".into(),
                why: error.to_string(),
            },
        )
        .await;
        return Err(error.into());
    }
    send(socket, &wire).await
}

async fn send_update<S>(socket: &mut S, update: Update) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let wire = Wire::Update { update };
    let bytes = serde_json::to_vec(&wire)?.len();
    if bytes > MAX_CONTROL_BYTES {
        let error = UpdateTooLarge { bytes };
        let _ = send(
            socket,
            &Wire::Failure {
                code: "update_too_large".into(),
                why: error.to_string(),
            },
        )
        .await;
        return Err(error.into());
    }
    send(socket, &wire).await
}

async fn receive<S>(socket: &mut S) -> anyhow::Result<Option<Incoming>>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(frame) = socket.next().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(tokio_tungstenite::tungstenite::Error::ConnectionClosed)
            | Err(tokio_tungstenite::tungstenite::Error::AlreadyClosed)
            | Err(tokio_tungstenite::tungstenite::Error::Protocol(
                tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake,
            )) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        match frame {
            Message::Text(text) => {
                anyhow::ensure!(text.len() <= MAX_CONTROL_BYTES, "control message too large");
                return Ok(Some(Incoming::Control(serde_json::from_str(&text)?)));
            }
            Message::Close(_) => return Ok(None),
            Message::Binary(bytes) => {
                anyhow::ensure!(bytes.len() <= BULK_CHUNK_BYTES, "bulk chunk too large");
                return Ok(Some(Incoming::Binary(bytes.to_vec())));
            }
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Ok(None)
}

/// The repo-side adapter owns all reads and mutations. It must subscribe
/// before taking the snapshot, and both must share one ordering source.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>);
    async fn dispatch(&self, action: Action) -> ActionResult;
    async fn upload(&self, _media_type: String, _bytes: Vec<u8>) -> ActionResult {
        ActionResult::Refused {
            why: "uploads are unavailable for this repo connector".into(),
        }
    }
}

/// Connector for either a locally launched server or an existing one.
/// Both destinations use this exact handshake and dispatch loop.
pub async fn connect(
    endpoint: &str,
    session: &str,
    credential: &str,
    backend: Arc<dyn Backend>,
) -> anyhow::Result<()> {
    connect_signaled(endpoint, session, credential, backend, None).await
}

/// As `connect`, with a signal after the host acknowledges its first
/// authoritative image. A parent can distinguish a listening child
/// from a repo session that is actually attached.
pub async fn connect_signaled(
    endpoint: &str,
    session: &str,
    credential: &str,
    backend: Arc<dyn Backend>,
    attached: Option<tokio::sync::mpsc::Sender<()>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !session.is_empty() && session.len() <= 128,
        "invalid session handle"
    );
    let (socket, _) = tokio_tungstenite::connect_async(endpoint).await?;
    run_connector(socket, session, credential, backend, attached).await
}

async fn run_connector(
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    session: &str,
    credential: &str,
    backend: Arc<dyn Backend>,
    attached: Option<tokio::sync::mpsc::Sender<()>>,
) -> anyhow::Result<()> {
    let (mut outgoing, mut incoming) = socket.split();
    send(
        &mut outgoing,
        &Wire::Hello {
            version: VERSION,
            session: session.to_owned(),
            credential: credential.to_owned(),
        },
    )
    .await?;
    match receive(&mut incoming).await? {
        Some(Incoming::Control(Wire::Ready { version: VERSION })) => {}
        Some(Incoming::Control(Wire::Refused { why })) => {
            anyhow::bail!("connection refused: {why}")
        }
        _ => anyhow::bail!("server did not accept the session protocol"),
    }
    let (snapshot, mut updates) = backend.subscribe_with_snapshot().await;
    let mut last = snapshot.sequence;
    send_snapshot(&mut outgoing, snapshot).await?;
    match receive(&mut incoming).await? {
        Some(Incoming::Control(Wire::Applied { sequence })) if sequence == last => {
            if let Some(attached) = attached {
                let _ = attached.try_send(());
            }
        }
        _ => anyhow::bail!("host did not acknowledge the initial snapshot"),
    }
    let mut upload: Option<(u64, String, usize, Vec<u8>)> = None;
    loop {
        tokio::select! {
            update = updates.recv() => match update {
                Ok(update) if update.sequence <= last => {}
                Ok(update) if update.sequence == last + 1 => {
                    last = update.sequence;
                    send_update(&mut outgoing, update).await?;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (snapshot, replacement) = backend.subscribe_with_snapshot().await;
                    last = snapshot.sequence;
                    updates = replacement;
                    send_snapshot(&mut outgoing, snapshot).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            message = receive(&mut incoming) => match message? {
                Some(Incoming::Control(Wire::Request { id, action })) => {
                    let result = backend.dispatch(action).await;
                    send(&mut outgoing, &Wire::Result { id, result }).await?;
                }
                Some(Incoming::Control(Wire::UploadBegin { id, media_type, size })) => {
                    anyhow::ensure!(size > 0 && size <= MAX_UPLOAD_BYTES, "invalid upload size");
                    anyhow::ensure!(media_type.len() <= 256, "invalid upload media type");
                    upload = Some((id, media_type, size, Vec::with_capacity(size)));
                }
                Some(Incoming::Binary(chunk)) => {
                    let Some((_, _, size, bytes)) = upload.as_mut() else {
                        anyhow::bail!("upload chunk without a begin message");
                    };
                    anyhow::ensure!(bytes.len() + chunk.len() <= *size, "upload exceeded declared size");
                    bytes.extend_from_slice(&chunk);
                }
                Some(Incoming::Control(Wire::UploadEnd { id })) => {
                    let Some((begin_id, media_type, size, bytes)) = upload.take() else {
                        anyhow::bail!("upload ended without a begin message");
                    };
                    anyhow::ensure!(id == begin_id && bytes.len() == size, "incomplete upload");
                    let result = backend.upload(media_type, bytes).await;
                    send(&mut outgoing, &Wire::Result { id, result }).await?;
                }
                Some(Incoming::Control(Wire::Applied { .. })) => {}
                None => return Ok(()),
                _ => anyhow::bail!("unexpected host message"),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    Disconnected,
    OutcomeUnknown,
}

struct State {
    active: bool,
    snapshot: Option<Snapshot>,
    generation: u64,
    revision: u64,
    next_id: u64,
    outbound: Option<mpsc::Sender<Outgoing>>,
    pending: HashMap<u64, oneshot::Sender<ActionResult>>,
    failure: Option<BridgeFailure>,
}

/// One explicit session handle on the host. It retains the latest
/// snapshot and ordered updates, and rejects a second live connector.
pub struct Session {
    handle: String,
    state: Mutex<State>,
    events: broadcast::Sender<SessionEvent>,
    upload_gate: tokio::sync::Mutex<()>,
}

impl Session {
    pub fn new(handle: impl Into<String>) -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            handle: handle.into(),
            state: Mutex::new(State {
                active: false,
                snapshot: None,
                generation: 0,
                revision: 0,
                next_id: 1,
                outbound: None,
                pending: HashMap::new(),
                failure: None,
            }),
            events,
            upload_gate: tokio::sync::Mutex::new(()),
        }
    }

    pub fn handle(&self) -> &str {
        &self.handle
    }

    /// Subscribe before reading the current image, matching the
    /// snapshot/live handoff used by the browser feed.
    pub async fn subscribe_with_snapshot(
        &self,
    ) -> (SessionView, broadcast::Receiver<SessionEvent>) {
        let receiver = self.events.subscribe();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (
            SessionView {
                revision: state.revision,
                generation: state.generation,
                snapshot: state.snapshot.clone(),
            },
            receiver,
        )
    }

    pub async fn connected(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot
            .is_some()
    }

    pub async fn failure(&self) -> Option<BridgeFailure> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .failure
            .clone()
    }

    pub async fn call(&self, action: Action) -> Result<ActionResult, CallError> {
        let (id, outbound, answer) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.snapshot.is_none() {
                return Err(CallError::Disconnected);
            }
            let outbound = state.outbound.clone().ok_or(CallError::Disconnected)?;
            let id = state.next_id;
            state.next_id = state
                .next_id
                .checked_add(1)
                .ok_or(CallError::Disconnected)?;
            let (tx, rx) = oneshot::channel();
            state.pending.insert(id, tx);
            (id, outbound, rx)
        };
        let _guard = PendingGuard { session: self, id };
        if outbound
            .send(Outgoing::Control(Wire::Request { id, action }))
            .await
            .is_err()
        {
            return Err(CallError::Disconnected);
        }
        answer.await.map_err(|_| CallError::OutcomeUnknown)
    }

    /// Send one bounded upload as binary frames. Only one upload may
    /// occupy this session at a time; ordinary control requests remain
    /// available while its chunks are queued.
    pub async fn upload(
        &self,
        media_type: String,
        bytes: Vec<u8>,
    ) -> Result<ActionResult, CallError> {
        if bytes.is_empty() || bytes.len() > MAX_UPLOAD_BYTES || media_type.len() > 256 {
            return Ok(ActionResult::Refused {
                why: "invalid upload size or media type".into(),
            });
        }
        let _upload = self.upload_gate.lock().await;
        let (id, outbound, answer) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.snapshot.is_none() {
                return Err(CallError::Disconnected);
            }
            let outbound = state.outbound.clone().ok_or(CallError::Disconnected)?;
            let id = state.next_id;
            state.next_id = state
                .next_id
                .checked_add(1)
                .ok_or(CallError::Disconnected)?;
            let (tx, rx) = oneshot::channel();
            state.pending.insert(id, tx);
            (id, outbound, rx)
        };
        let _guard = PendingGuard { session: self, id };
        if outbound
            .send(Outgoing::Control(Wire::UploadBegin {
                id,
                media_type,
                size: bytes.len(),
            }))
            .await
            .is_err()
        {
            return Err(CallError::Disconnected);
        }
        for chunk in bytes.chunks(BULK_CHUNK_BYTES) {
            if outbound
                .send(Outgoing::Binary(chunk.to_vec()))
                .await
                .is_err()
            {
                return Err(CallError::OutcomeUnknown);
            }
        }
        if outbound
            .send(Outgoing::Control(Wire::UploadEnd { id }))
            .await
            .is_err()
        {
            return Err(CallError::OutcomeUnknown);
        }
        answer.await.map_err(|_| CallError::OutcomeUnknown)
    }

    /// The HTTP host upgrades an already authorized connector. It
    /// supplies the credential check independently of browser auth.
    pub async fn accept<S, F, Fut>(
        self: Arc<Self>,
        socket: WebSocketStream<S>,
        admit: F,
    ) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
        F: Fn(String, String) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let (mut outgoing, mut incoming) = socket.split();
        let hello =
            tokio::time::timeout(std::time::Duration::from_secs(10), receive(&mut incoming))
                .await??;
        let (session, credential) = match hello {
            Some(Incoming::Control(Wire::Hello {
                version: VERSION,
                session,
                credential,
            })) => (session, credential),
            _ => {
                send(
                    &mut outgoing,
                    &Wire::Refused {
                        why: "protocol version or handshake mismatch".into(),
                    },
                )
                .await?;
                anyhow::bail!("protocol version or handshake mismatch")
            }
        };
        if session != self.handle || !admit(session.clone(), credential).await {
            send(
                &mut outgoing,
                &Wire::Refused {
                    why: "session admission refused".into(),
                },
            )
            .await?;
            anyhow::bail!("session admission refused")
        }
        let (tx, mut rx) = mpsc::channel(64);
        let generation = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.active {
                None
            } else {
                state.active = true;
                state.generation += 1;
                state.snapshot = None;
                state.outbound = Some(tx);
                Some(state.generation)
            }
        };
        let Some(generation) = generation else {
            send(
                &mut outgoing,
                &Wire::Refused {
                    why: "session already connected".into(),
                },
            )
            .await?;
            anyhow::bail!("session already connected")
        };
        let _guard = ConnectionGuard {
            session: self.clone(),
            generation,
        };
        let result = async {
            send(&mut outgoing, &Wire::Ready { version: VERSION }).await?;
            loop {
                tokio::select! {
                    outbound = rx.recv() => match outbound {
                        Some(Outgoing::Control(wire)) => send(&mut outgoing, &wire).await?,
                        Some(Outgoing::Binary(bytes)) => outgoing.send(Message::Binary(bytes.into())).await?,
                        None => return Ok(()),
                    },
                    inbound = receive(&mut incoming) => match inbound? {
                        Some(Incoming::Control(Wire::Snapshot { snapshot })) => {
                            let sequence = snapshot.sequence;
                            {
                                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                                state.failure = None;
                                state.revision += 1;
                                state.snapshot = Some(snapshot.clone());
                                let _ = self.events.send(SessionEvent::Snapshot { revision: state.revision, generation, snapshot });
                            }
                            send(&mut outgoing, &Wire::Applied { sequence }).await?;
                        }
                        Some(Incoming::Control(Wire::Update { update })) => {
                            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                            let Some(previous) = state.snapshot.as_ref() else {
                                anyhow::bail!("session update before snapshot");
                            };
                            if previous.sequence.checked_add(1) != Some(update.sequence) {
                                anyhow::bail!("session update sequence gap");
                            }
                            let merged = crate::replay::merge(&previous.state, &update.state)?;
                            state.revision += 1;
                            state.snapshot = Some(Snapshot { sequence: update.sequence, state: merged });
                            let _ = self.events.send(SessionEvent::Update { revision: state.revision, generation, update });
                        }
                        Some(Incoming::Control(Wire::Result { id, result })) => {
                            if let Some(pending) = self.state.lock().unwrap_or_else(|e| e.into_inner()).pending.remove(&id) {
                                let _ = pending.send(result);
                            }
                        }
                        Some(Incoming::Control(Wire::Failure { code, why })) => {
                            self.state.lock().unwrap_or_else(|e| e.into_inner()).failure = Some(BridgeFailure { code, why: why.clone() });
                            anyhow::bail!("connector failed: {why}");
                        }
                        None => return Ok(()),
                        _ => anyhow::bail!("unexpected connector message"),
                    },
                }
            }
        }.await;
        result
    }
}

/// Releases the connection even when its task is cancelled. A former
/// connector cannot clear a later attachment to the same handle.
struct ConnectionGuard {
    session: Arc<Session>,
    generation: u64,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut state = self.session.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.generation == self.generation {
            state.active = false;
            state.snapshot = None;
            state.outbound = None;
            state.pending.clear();
            state.revision += 1;
            let _ = self.session.events.send(SessionEvent::Disconnected {
                revision: state.revision,
                generation: self.generation,
            });
        }
    }
}

struct PendingGuard<'a> {
    session: &'a Session,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.session
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pending
            .remove(&self.id);
    }
}
