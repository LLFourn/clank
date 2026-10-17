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
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub const VERSION: u32 = 2;
// Only the pre-authentication JSON hello is bounded. Publications have no
// logical-size ceiling; BULK_CHUNK_BYTES controls transport buffering.
pub const MAX_BOOTSTRAP_BYTES: usize = 1024 * 1024;
pub const MAX_UPLOAD_BYTES: usize = 12 * 1024 * 1024;
pub const BULK_CHUNK_BYTES: usize = 64 * 1024;

/// Both peers must understand the binary schema; retrying cannot upgrade one.
#[derive(Debug)]
pub struct ProtocolMismatch(pub String);
impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bridge protocol mismatch: {}; upgrade the connector and host together",
            self.0
        )
    }
}
impl std::error::Error for ProtocolMismatch {}

/// Configure the Rust connector socket. The pre-authentication hello is the
/// largest physical message; admitted transfers use smaller 64 KiB blocks.
/// These are transport buffers, never logical publication limits.
pub fn websocket_config() -> tokio_tungstenite::tungstenite::protocol::WebSocketConfig {
    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .read_buffer_size(BULK_CHUNK_BYTES)
        .write_buffer_size(0)
        .max_message_size(Some(MAX_BOOTSTRAP_BYTES))
        .max_frame_size(Some(MAX_BOOTSTRAP_BYTES))
}

#[derive(Debug, Clone, PartialEq)]
pub struct BridgeFailure {
    pub code: String,
    pub why: String,
}

/// A state image and its last included update number. The backend must
/// return this together with a subscription that cannot miss later updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub sequence: u64,
    pub state: serde_json::Value,
}

/// A subscriber's authoritative position. The revision orders every
/// host publication, including a disconnect retaining the last image. A browser
/// can discard already included events from its receiver.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionView {
    pub revision: u64,
    pub generation: u64,
    /// Whether the retained image belongs to a currently ready connector.
    pub connected: bool,
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
            view.connected = true;
        }
        SessionEvent::Update {
            generation, update, ..
        } => {
            let Some(previous) = view.snapshot.as_mut() else {
                return Err(());
            };
            if !view.connected
                || view.generation != generation
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
            view.connected = false;
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
pub(crate) enum Wire {
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
    UploadChunk {
        bytes: Vec<u8>,
    },
    Resync,
    Result {
        id: u64,
        result: ActionResult,
    },
}

enum Incoming {
    Control(Wire),
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
    anyhow::ensure!(
        text.len() <= MAX_BOOTSTRAP_BYTES,
        "control message too large"
    );
    socket.send(Message::Text(text.into())).await?;
    Ok(())
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
                anyhow::ensure!(
                    text.len() <= MAX_BOOTSTRAP_BYTES,
                    "control message too large"
                );
                return Ok(Some(Incoming::Control(serde_json::from_str(&text)?)));
            }
            Message::Close(_) => return Ok(None),
            Message::Binary(bytes) => {
                anyhow::ensure!(bytes.len() <= BULK_CHUNK_BYTES, "bulk chunk too large");
                anyhow::bail!("binary transfer before bridge handshake");
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

/// Latest connector readiness, rather than a queue of stale attachment acks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Ready,
    Disconnected(String),
}

/// As `connect`, with a signal after the host acknowledges its first
/// authoritative image. A parent can distinguish a listening child
/// from a repo session that is actually attached. This function publishes
/// Ready; the connection supervisor owns Connecting/Disconnected transitions
/// when starting, reconnecting or stopping this future.
pub async fn connect_signaled(
    endpoint: &str,
    session: &str,
    credential: &str,
    backend: Arc<dyn Backend>,
    attached: Option<tokio::sync::watch::Sender<ConnectionStatus>>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !session.is_empty() && session.len() <= 128,
        "invalid session handle"
    );
    // Only connection establishment has a deadline, before publication bytes.
    let (socket, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async_with_config(endpoint, Some(websocket_config()), false),
    )
    .await
    .map_err(|_| anyhow::anyhow!("connector WebSocket establishment timed out"))??;
    run_connector(socket, session, credential, backend, attached).await
}

async fn run_connector<S>(
    mut socket: WebSocketStream<S>,
    session: &str,
    credential: &str,
    backend: Arc<dyn Backend>,
    attached: Option<tokio::sync::watch::Sender<ConnectionStatus>>,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    send(
        &mut socket,
        &Wire::Hello {
            version: VERSION,
            session: session.to_owned(),
            credential: credential.to_owned(),
        },
    )
    .await?;
    match tokio::time::timeout(std::time::Duration::from_secs(10), receive(&mut socket))
        .await
        .map_err(|_| anyhow::anyhow!("host did not answer the protocol handshake"))??
    {
        Some(Incoming::Control(Wire::Ready { version: VERSION })) => {}
        Some(Incoming::Control(Wire::Ready { version })) => {
            return Err(
                ProtocolMismatch(format!("host uses {version}, connector uses {VERSION}")).into(),
            );
        }
        Some(Incoming::Control(Wire::Refused { why })) => {
            if why.starts_with("protocol version") {
                return Err(ProtocolMismatch(why).into());
            }
            anyhow::bail!("connection refused: {why}")
        }
        _ => anyhow::bail!("server did not accept the session protocol"),
    }
    let mut transport = crate::bridge_transport::Connection::new(socket);
    let (snapshot, mut updates) = backend.subscribe_with_snapshot().await;
    let mut last = snapshot.sequence;
    transport.send(Wire::Snapshot { snapshot }).await?;
    match transport.recv().await? {
        Some(Wire::Applied { sequence }) if sequence == last => {
            if let Some(attached) = attached {
                attached.send_replace(ConnectionStatus::Ready);
            }
        }
        _ => anyhow::bail!("host did not acknowledge the initial snapshot"),
    }
    let mut upload: Option<(u64, String, usize, Vec<u8>)> = None;
    let sender = transport.outbound.clone();
    let mut pending = std::collections::VecDeque::new();
    let mut in_flight = None;
    let result = async { loop {
        tokio::select! {
            permit = sender.reserve(), if !pending.is_empty() => {
                permit.map_err(|_| anyhow::anyhow!("bridge disconnected"))?.send(pending.pop_front().unwrap());
            }
            update = updates.recv(), if pending.is_empty() && in_flight.is_none() => match update {
                Ok(update) if update.sequence <= last => {}
                Ok(update) if Some(update.sequence) == last.checked_add(1) => {
                    last = update.sequence;
                    in_flight = Some(last);
                    pending.push_back(Wire::Update { update });
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    let (snapshot, replacement) = backend.subscribe_with_snapshot().await;
                    last = snapshot.sequence;
                    updates = replacement;
                    in_flight = Some(last);
                    pending.push_back(Wire::Snapshot { snapshot });
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            // One unsent result/publication owns the pending slot.
            message = transport.recv(), if pending.is_empty() => match message? {
                Some(Wire::Request { id, action }) => {
                    let result = backend.dispatch(action).await;
                    pending.push_back(Wire::Result { id, result });
                }
                Some(Wire::UploadBegin { id, media_type, size }) => {
                    anyhow::ensure!(upload.is_none(), "upload already in progress");
                    anyhow::ensure!(size > 0 && size <= MAX_UPLOAD_BYTES, "invalid upload size");
                    anyhow::ensure!(media_type.len() <= 256, "invalid upload media type");
                    upload = Some((id, media_type, size, Vec::with_capacity(size)));
                }
                Some(Wire::UploadChunk { bytes: chunk }) => {
                    let Some((_, _, size, bytes)) = upload.as_mut() else {
                        anyhow::bail!("upload chunk without a begin message");
                    };
                    anyhow::ensure!(bytes.len() + chunk.len() <= *size, "upload exceeded declared size");
                    bytes.extend_from_slice(&chunk);
                }
                Some(Wire::UploadEnd { id }) => {
                    let Some((begin_id, media_type, size, bytes)) = upload.take() else {
                        anyhow::bail!("upload ended without a begin message");
                    };
                    anyhow::ensure!(id == begin_id && bytes.len() == size, "incomplete upload");
                    let result = backend.upload(media_type, bytes).await;
                    pending.push_back(Wire::Result { id, result });
                }
                Some(Wire::Applied { sequence }) => {
                    if in_flight == Some(sequence) { in_flight = None; }
                }
                Some(Wire::Resync) => {
                    let (snapshot, replacement) = backend.subscribe_with_snapshot().await;
                    last = snapshot.sequence; updates = replacement;
                    in_flight = Some(last);
                    pending.push_back(Wire::Snapshot { snapshot });
                }
                None => return Ok(()),
                _ => anyhow::bail!("unexpected host message"),
            },
        }
    } }.await;
    transport.shutdown().await;
    result
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallError {
    Disconnected,
    OutcomeUnknown,
}

struct State {
    active: bool,
    ready: bool,
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
                ready: false,
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
                connected: state.ready,
                snapshot: state.snapshot.clone(),
            },
            receiver,
        )
    }

    pub async fn connected(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).ready
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
            if !state.active || !state.ready {
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
            if !state.active || !state.ready {
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
        mut socket: WebSocketStream<S>,
        admit: F,
    ) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        F: Fn(String, String) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let hello = tokio::time::timeout(std::time::Duration::from_secs(10), receive(&mut socket))
            .await??;
        let (session, credential) = match hello {
            Some(Incoming::Control(Wire::Hello {
                version: VERSION,
                session,
                credential,
            })) => (session, credential),
            _ => {
                send(
                    &mut socket,
                    &Wire::Refused {
                        why: format!("protocol version or handshake mismatch; host requires {VERSION}; upgrade the connector and host together"),
                    },
                )
                .await?;
                anyhow::bail!("protocol version or handshake mismatch")
            }
        };
        if session != self.handle || !admit(session.clone(), credential).await {
            send(
                &mut socket,
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
                state.ready = false;
                state.generation += 1;
                state.outbound = Some(tx);
                Some(state.generation)
            }
        };
        let Some(generation) = generation else {
            send(
                &mut socket,
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
            send(&mut socket, &Wire::Ready { version: VERSION }).await?;
            let mut transport = crate::bridge_transport::Connection::new(socket);
            let sender = transport.outbound.clone();
            let mut pending = std::collections::VecDeque::new();
            let run = async { loop {
                tokio::select! {
                    permit = sender.reserve(), if !pending.is_empty() => {
                        permit.map_err(|_| anyhow::anyhow!("bridge disconnected"))?.send(pending.pop_front().unwrap());
                    }
                    outbound = rx.recv(), if pending.is_empty() => match outbound {
                        Some(Outgoing::Control(wire)) => pending.push_back(wire),
                        Some(Outgoing::Binary(bytes)) => pending.push_back(Wire::UploadChunk { bytes }),
                        None => return Ok(()),
                    },
                    // One queued request plus one publication acknowledgement.
                    inbound = transport.recv(), if pending.len() < 2 => match inbound? {
                        Some(Wire::Snapshot { snapshot }) => {
                            let sequence = snapshot.sequence;
                            let snapshot = Snapshot { sequence, state: crate::replay::stamp_snapshot(&snapshot.state)? };
                            {
                                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                                if state.ready {
                                    anyhow::ensure!(state.snapshot.as_ref().is_none_or(|previous| snapshot.sequence >= previous.sequence), "snapshot moved backwards within a connection");
                                }
                                state.failure = None;
                                state.ready = true;
                                state.revision += 1;
                                state.snapshot = Some(snapshot.clone());
                                let _ = self.events.send(SessionEvent::Snapshot { revision: state.revision, generation, snapshot });
                            }
                            pending.push_back(Wire::Applied { sequence });
                        }
                        Some(Wire::Update { update }) => {
                            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                            anyhow::ensure!(state.ready, "session update before current snapshot");
                            let Some(previous) = state.snapshot.as_ref() else {
                                anyhow::bail!("session update before snapshot");
                            };
                            if previous.sequence.checked_add(1) != Some(update.sequence) {
                                drop(state);
                                pending.push_back(Wire::Resync);
                                continue;
                            }
                            let merged = crate::replay::merge(&previous.state, &update.state)?;
                            state.revision += 1;
                            state.snapshot = Some(Snapshot { sequence: update.sequence, state: merged });
                            let sequence = update.sequence;
                            let _ = self.events.send(SessionEvent::Update { revision: state.revision, generation, update });
                            pending.push_back(Wire::Applied { sequence });
                        }
                        Some(Wire::Result { id, result }) => {
                            if let Some(pending) = self.state.lock().unwrap_or_else(|e| e.into_inner()).pending.remove(&id) {
                                let _ = pending.send(result);
                            }
                        }
                        Some(Wire::Failure { code, why }) => {
                            self.state.lock().unwrap_or_else(|e| e.into_inner()).failure = Some(BridgeFailure { code, why: why.clone() });
                            anyhow::bail!("connector failed: {why}");
                        }
                        None => return Ok(()),
                        _ => anyhow::bail!("unexpected connector message"),
                    },
                }
            } }.await;
            transport.shutdown().await;
            run
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
            state.ready = false;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn admitted(
        session: Arc<Session>,
    ) -> (
        WebSocketStream<DuplexStream>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let (a, b) = tokio::io::duplex(BULK_CHUNK_BYTES * 2);
        let socket = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let host = tokio::spawn(session.accept(socket, |_, _| async { true }));
        let mut peer = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        send(
            &mut peer,
            &Wire::Hello {
                version: VERSION,
                session: "test".into(),
                credential: "local-fixture".into(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            receive(&mut peer).await.unwrap(),
            Some(Incoming::Control(Wire::Ready { version: VERSION }))
        ));
        (peer, host)
    }
    async fn frame(
        peer: &mut WebSocketStream<DuplexStream>,
        tag: u8,
        id: u64,
        offset: u64,
        bytes: &[u8],
    ) {
        let mut packet = vec![tag];
        packet.extend_from_slice(&id.to_le_bytes());
        packet.extend_from_slice(&offset.to_le_bytes());
        packet.extend_from_slice(bytes);
        peer.send(Message::Binary(packet.into())).await.unwrap();
    }
    #[tokio::test]
    async fn an_unfinished_image_cannot_replace_the_committed_image() {
        for corrupt in [false, true] {
            let session = Arc::new(Session::new("test"));
            let (mut peer, host) = admitted(session.clone()).await;
            let previous = Snapshot {
                sequence: 0,
                state: serde_json::json!({"text":"committed"}),
            };
            let mut bytes = Vec::new();
            crate::bridge_codec::encode(
                &mut bytes,
                &Wire::Snapshot {
                    snapshot: previous.clone(),
                },
            )
            .unwrap();
            frame(&mut peer, 0, 1, 0, &[]).await;
            frame(&mut peer, 1, 1, 0, &bytes).await;
            frame(&mut peer, 2, 1, bytes.len() as u64, &[]).await;
            // Drain the complete Applied transfer; host publication precedes it.
            for _ in 0..3 {
                peer.next().await.unwrap().unwrap();
            }
            assert!(session.connected().await);
            let mut candidate = Vec::new();
            crate::bridge_codec::encode(
                &mut candidate,
                &Wire::Update {
                    update: Update {
                        sequence: 1,
                        state: serde_json::json!({"text":"x".repeat(3 * BULK_CHUNK_BYTES)}),
                    },
                },
            )
            .unwrap();
            frame(&mut peer, 0, 2, 0, &[]).await;
            for (index, block) in candidate.chunks(BULK_CHUNK_BYTES).enumerate() {
                frame(&mut peer, 1, 2, (index * BULK_CHUNK_BYTES) as u64, block).await;
            }
            if corrupt {
                frame(&mut peer, 2, 2, candidate.len() as u64 + 1, &[]).await;
            } else {
                // Seeing the Pong proves the reader staged all preceding Data,
                // including a spilled file, without publishing those bytes.
                peer.send(Message::Ping(vec![1].into())).await.unwrap();
                assert!(matches!(
                    peer.next().await.unwrap().unwrap(),
                    Message::Pong(_)
                ));
                assert_eq!(
                    session.subscribe_with_snapshot().await.0.snapshot,
                    Some(previous.clone())
                );
                drop(peer);
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), host)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.is_err(), corrupt, "{result:?}");
            assert!(!session.connected().await);
            assert_eq!(
                session.subscribe_with_snapshot().await.0.snapshot,
                Some(previous)
            );
        }
    }
    /// A quiet pane in the connector's snapshot, held by the host for
    /// five minutes, replays to a late browser five minutes older: the
    /// host stamps it as it ingests the snapshot.
    #[tokio::test]
    async fn a_snapshot_pane_held_by_the_host_replays_older_by_the_time_held() {
        use std::sync::atomic::Ordering;
        let session = Arc::new(Session::new("test"));
        let (peer, _host) = admitted(session.clone()).await;
        let mut peer = crate::bridge_transport::Connection::new(peer);
        peer.send(Wire::Snapshot {
            snapshot: Snapshot {
                sequence: 0,
                state: serde_json::json!({"replay": [
                    {"event":"pane","data":{"pane_id":"terminal_6","screen":"x","revision":2,"age_ms":40}}
                ]}),
            },
        })
        .await
        .unwrap();
        assert_eq!(
            peer.recv().await.unwrap(),
            Some(Wire::Applied { sequence: 0 })
        );
        let (view, _) = session.subscribe_with_snapshot().await;
        crate::replay::TEST_CLOCK_OFFSET.fetch_add(300_000, Ordering::SeqCst);
        let frames = crate::service::replay(&view).unwrap();
        crate::replay::TEST_CLOCK_OFFSET.fetch_sub(300_000, Ordering::SeqCst);
        let pane = frames
            .iter()
            .find(|f| f.starts_with("event: pane"))
            .unwrap();
        let data: serde_json::Value =
            serde_json::from_str(pane.lines().find_map(|l| l.strip_prefix("data: ")).unwrap())
                .unwrap();
        let age = data["age_ms"].as_u64().unwrap();
        assert!(age >= 300_040, "replayed at {age} ms old");
    }

    #[tokio::test]
    async fn a_host_gap_requests_resync_and_old_snapshots_cannot_move_it_backwards() {
        let session = Arc::new(Session::new("test"));
        let (peer, host) = admitted(session.clone()).await;
        let mut peer = crate::bridge_transport::Connection::new(peer);
        peer.send(Wire::Snapshot {
            snapshot: Snapshot {
                sequence: 0,
                state: serde_json::json!({"status":"first"}),
            },
        })
        .await
        .unwrap();
        assert_eq!(
            peer.recv().await.unwrap(),
            Some(Wire::Applied { sequence: 0 })
        );
        peer.send(Wire::Update {
            update: Update {
                sequence: 2,
                state: serde_json::json!({"status":"gap"}),
            },
        })
        .await
        .unwrap();
        assert_eq!(peer.recv().await.unwrap(), Some(Wire::Resync));
        assert_eq!(
            session
                .subscribe_with_snapshot()
                .await
                .0
                .snapshot
                .unwrap()
                .sequence,
            0
        );
        peer.send(Wire::Snapshot {
            snapshot: Snapshot {
                sequence: 3,
                state: serde_json::json!({"status":"current"}),
            },
        })
        .await
        .unwrap();
        assert_eq!(
            peer.recv().await.unwrap(),
            Some(Wire::Applied { sequence: 3 })
        );
        peer.send(Wire::Snapshot {
            snapshot: Snapshot {
                sequence: 1,
                state: serde_json::json!({"status":"old"}),
            },
        })
        .await
        .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), host)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
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
        peer.shutdown().await;
    }
    #[tokio::test]
    async fn old_connectors_get_an_upgrade_refusal_before_authentication() {
        let session = Arc::new(Session::new("test"));
        let (a, b) = tokio::io::duplex(4096);
        let socket = WebSocketStream::from_raw_socket(a, Role::Server, None).await;
        let host = tokio::spawn(session.accept(socket, |_, _| async {
            panic!("unsupported protocol must not reach admission")
        }));
        let mut peer = WebSocketStream::from_raw_socket(b, Role::Client, None).await;
        send(
            &mut peer,
            &Wire::Hello {
                version: 1,
                session: "test".into(),
                credential: "unused".into(),
            },
        )
        .await
        .unwrap();
        let Some(Incoming::Control(Wire::Refused { why })) = receive(&mut peer).await.unwrap()
        else {
            panic!("missing refusal")
        };
        assert!(why.contains("upgrade"));
        assert!(host.await.unwrap().is_err());
    }
    #[tokio::test]
    async fn a_slow_publication_uses_credit_and_coalesces_broadcast_lag() {
        struct Source {
            state: Mutex<Snapshot>,
            updates: broadcast::Sender<Update>,
        }
        #[async_trait]
        impl Backend for Source {
            async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>) {
                let receiver = self.updates.subscribe();
                (self.state.lock().unwrap().clone(), receiver)
            }
            async fn dispatch(&self, _: Action) -> ActionResult {
                unreachable!()
            }
        }
        impl Source {
            fn publish(&self, sequence: u64, text: &str) {
                let mut state = self.state.lock().unwrap();
                *state = Snapshot {
                    sequence,
                    state: serde_json::json!({"text":text,"sequence":sequence}),
                };
                let _ = self.updates.send(Update {
                    sequence,
                    state: state.state.clone(),
                });
            }
        }
        let (updates, _) = broadcast::channel(2);
        let source = Arc::new(Source {
            state: Mutex::new(Snapshot {
                sequence: 0,
                state: serde_json::Value::Null,
            }),
            updates,
        });
        let (a, b) = tokio::io::duplex(BULK_CHUNK_BYTES);
        let socket = WebSocketStream::from_raw_socket(a, Role::Client, None).await;
        let mut host = WebSocketStream::from_raw_socket(b, Role::Server, None).await;
        let backend = source.clone();
        let connector =
            tokio::spawn(
                async move { run_connector(socket, "test", "fixture", backend, None).await },
            );
        assert!(matches!(
            receive(&mut host).await.unwrap(),
            Some(Incoming::Control(Wire::Hello {
                version: VERSION,
                ..
            }))
        ));
        send(&mut host, &Wire::Ready { version: VERSION })
            .await
            .unwrap();
        let mut host = crate::bridge_transport::Connection::new(host);
        assert!(matches!(
            host.recv().await.unwrap(),
            Some(Wire::Snapshot {
                snapshot: Snapshot { sequence: 0, .. }
            })
        ));
        host.send(Wire::Applied { sequence: 0 }).await.unwrap();
        let large = "x".repeat(2 * 1024 * 1024);
        source.publish(1, &large);
        assert!(matches!(
            host.recv().await.unwrap(),
            Some(Wire::Update {
                update: Update { sequence: 1, .. }
            })
        ));
        // Withhold publication credit while filling a two-slot broadcast.
        // No encoder/transfer queue grows with these twenty updates.
        for sequence in 2..=20 {
            source.publish(sequence, &large);
        }
        host.send(Wire::Applied { sequence: 1 }).await.unwrap();
        let fresh = tokio::time::timeout(std::time::Duration::from_secs(5), host.recv())
            .await
            .unwrap()
            .unwrap();
        let Some(Wire::Snapshot { snapshot }) = fresh else {
            panic!("lag must replace the backlog with one snapshot")
        };
        assert_eq!(snapshot.sequence, 20);
        assert_eq!(snapshot.state["text"], large);
        host.send(Wire::Applied { sequence: 20 }).await.unwrap();
        host.shutdown().await;
        tokio::time::timeout(std::time::Duration::from_secs(2), connector)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
