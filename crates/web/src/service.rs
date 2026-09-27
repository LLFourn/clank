//! Browser-facing routes over one explicitly named session. The host owns
//! its listener and admission policy; this module has no repo access.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, LOCATION};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

use crate::assets::{self, BrowserAuth, PageOptions};
use crate::bridge::{self, ActionResult, CallError, Session, SessionEvent, SessionView};
use crate::request::{BrowserReply, BrowserRequest};

/// A browser's authorization remains live while its WebSocket is open.
/// Send `true` or drop the sender to revoke it. Separate browsers may
/// receive different watches, so one revocation need not close all peers.
pub struct BrowserLease {
    revoked: watch::Receiver<bool>,
}

impl BrowserLease {
    pub fn new(revoked: watch::Receiver<bool>) -> Self {
        Self { revoked }
    }

    fn is_revoked(&self) -> bool {
        *self.revoked.borrow() || self.revoked.has_changed().is_err()
    }

    async fn wait_revoked(&mut self) {
        while !self.is_revoked() {
            if self.revoked.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Called for every browser request, including assets and upgrades.
/// The host must check its own cookie/session and authorize this handle.
/// Returning `None` fails closed; connector admission is separate.
#[async_trait]
pub trait BrowserAdmission: Send + Sync + 'static {
    async fn admit(&self, headers: &HeaderMap, session: &str) -> Option<BrowserLease>;
}

/// The connector publishes a complete replay plus the frames caused by
/// its latest update. Browser reconnects receive `replay`; existing
/// browsers receive only `live`. Both are the page's SSE-shaped frames.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrowserFrames {
    pub replay: Vec<String>,
    #[serde(default)]
    pub live: Vec<String>,
}

/// A publication contains only the new frames. The host applies them
/// to its retained replay for later browsers and reconnects.
#[derive(Debug, Serialize, Deserialize)]
pub struct BrowserDelta {
    pub live: Vec<String>,
}

impl BrowserFrames {
    fn from_state(state: &serde_json::Value) -> anyhow::Result<Self> {
        Ok(serde_json::from_value(state.clone())?)
    }
}

pub type Body = Full<Bytes>;
pub type Reply = Response<Body>;

/// Mount under `PageOptions::base_path` with one host-supplied browser
/// admission policy. The host can route connector upgrades separately to
/// `Session::accept`; neither route grants the other kind of admission.
pub struct WebService {
    session: Arc<Session>,
    base: String,
    login: String,
    page: String,
    admission: Arc<dyn BrowserAdmission>,
    sockets: Mutex<JoinSet<()>>,
    upload_slots: Semaphore,
}

impl WebService {
    pub fn new(
        session: Arc<Session>,
        options: PageOptions<'_>,
        admission: Arc<dyn BrowserAdmission>,
    ) -> anyhow::Result<Self> {
        let login = match &options.auth {
            BrowserAuth::ClankDoor { .. } => format!("{}login", options.base_path),
            BrowserAuth::Host { login_path, .. } => (*login_path).to_owned(),
        };
        let base = options.base_path.to_owned();
        let page = assets::render_page(options)?;
        Ok(Self {
            session,
            base,
            login,
            page,
            admission,
            sockets: Mutex::new(JoinSet::new()),
            upload_slots: Semaphore::new(1),
        })
    }

    /// Supply this method to a Hyper service after the host has routed the
    /// mount. All served paths are still checked against the mount here.
    pub async fn serve(&self, req: Request<Incoming>) -> Reply {
        let Some(path) = req.uri().path().strip_prefix(&self.base) else {
            return reply(StatusCode::NOT_FOUND, "not here", "text/plain");
        };
        let path = path.to_owned();
        let method = req.method().clone();
        let recognized = matches!(
            (method.clone(), path.as_str()),
            (
                Method::GET,
                "" | "whoami"
                    | "bridge/status"
                    | "events"
                    | "xterm.js"
                    | "xterm.css"
                    | "plate.woff2"
            )
        ) || matches!(
            (method.clone(), path.as_str()),
            (
                Method::POST,
                "say"
                    | "stop"
                    | "queue"
                    | "stash"
                    | "agents"
                    | "blocks/answer"
                    | "agents/reopen"
                    | "upload"
            ) | (Method::GET, "agents/candidates")
        ) || (method == Method::GET && path.starts_with("html/"));
        if !recognized {
            return reply(StatusCode::NOT_FOUND, "not here", "text/plain");
        }
        let Some(lease) = self
            .admission
            .admit(req.headers(), self.session.handle())
            .await
        else {
            return self.unauthorized(&path);
        };
        if lease.is_revoked() {
            return self.unauthorized(&path);
        }
        match (method, path.as_str()) {
            (Method::GET, "") => self.page(),
            (Method::GET, "whoami") => reply(StatusCode::NO_CONTENT, "", "text/plain"),
            (Method::GET, "bridge/status") => {
                let failure = self.session.failure().await;
                reply(
                    StatusCode::OK,
                    serde_json::json!({
                        "connected": self.session.connected().await,
                        "failure": failure.as_ref().map(|failure| serde_json::json!({
                            "code": failure.code,
                            "why": failure.why,
                        })),
                    })
                    .to_string(),
                    "application/json",
                )
            }
            (Method::GET, "xterm.js") => reply(
                StatusCode::OK,
                assets::XTERM_JS,
                "text/javascript; charset=utf-8",
            ),
            (Method::GET, "xterm.css") => {
                reply(StatusCode::OK, assets::XTERM_CSS, "text/css; charset=utf-8")
            }
            (Method::GET, "plate.woff2") => bytes(StatusCode::OK, assets::PLATE_FONT, "font/woff2"),
            (Method::GET, "events") => self.events(req, lease),
            (Method::POST, "upload") => self.upload(req, lease).await,
            (
                Method::POST,
                "say" | "stop" | "queue" | "stash" | "agents" | "blocks/answer" | "agents/reopen",
            )
            | (Method::GET, "agents/candidates") => self.action(req, lease, &path).await,
            (Method::GET, path) if path.starts_with("html/") => self.action(req, lease, path).await,
            _ => unreachable!("recognized route already checked"),
        }
    }

    fn unauthorized(&self, path: &str) -> Reply {
        if path.is_empty() {
            let mut response = reply(StatusCode::SEE_OTHER, "", "text/plain");
            response
                .headers_mut()
                .insert(LOCATION, self.login.parse().expect("validated login path"));
            response
        } else {
            reply(
                StatusCode::UNAUTHORIZED,
                "browser admission refused",
                "text/plain",
            )
        }
    }

    fn page(&self) -> Reply {
        let mut nonce = [0u8; 16];
        if getrandom::fill(&mut nonce).is_err() {
            return reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                "nonce unavailable",
                "text/plain",
            );
        }
        let nonce = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let mut response = reply(
            StatusCode::OK,
            self.page.replace("__CLANK_NONCE__", &nonce),
            "text/html; charset=utf-8",
        );
        let policy = format!(
            "default-src 'none'; script-src 'self' 'nonce-{nonce}'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' blob:; font-src 'self'; connect-src 'self'; frame-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
        );
        response
            .headers_mut()
            .insert(CONTENT_SECURITY_POLICY, policy.parse().expect("static CSP"));
        response
    }

    fn events(&self, mut req: Request<Incoming>, lease: BrowserLease) -> Reply {
        if !hyper_tungstenite::is_upgrade_request(&req) {
            return reply(
                StatusCode::UPGRADE_REQUIRED,
                "connect with WebSocket",
                "text/plain",
            );
        }
        if !same_origin(req.headers()) {
            return reply(
                StatusCode::FORBIDDEN,
                "cross-origin WebSocket",
                "text/plain",
            );
        }
        let (response, upgrade) = match hyper_tungstenite::upgrade(&mut req, None) {
            Ok(pair) => pair,
            Err(_) => return reply(StatusCode::BAD_REQUEST, "invalid WebSocket", "text/plain"),
        };
        let session = self.session.clone();
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        while sockets.try_join_next().is_some() {}
        sockets.spawn(async move {
            if let Ok(socket) = upgrade.await {
                let _ = pump(socket, session, lease).await;
            }
        });
        response.map(|_| Full::new(Bytes::new()))
    }

    async fn action(&self, req: Request<Incoming>, mut lease: BrowserLease, path: &str) -> Reply {
        if req.method() == Method::POST && !same_origin(req.headers()) {
            return reply(StatusCode::FORBIDDEN, "cross-origin request", "text/plain");
        }
        let action = if req.method() == Method::GET {
            if path == "agents/candidates" {
                BrowserRequest::AgentCandidates
            } else {
                let resource = &path["html/".len()..];
                if !valid_site_path(resource) {
                    return json_why(StatusCode::BAD_REQUEST, "invalid generated-page path");
                }
                BrowserRequest::Site {
                    path: resource.to_owned(),
                }
            }
        } else {
            let limit = match path {
                "queue" => 512 * 1024,
                "say" | "blocks/answer" => 64 * 1024,
                _ => 4 * 1024,
            };
            let body = http_body_util::Limited::new(req.into_body(), limit);
            let Ok(body) = body.collect().await.map(|body| body.to_bytes()) else {
                return json_why(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
            };
            match parse_action(path, &body) {
                Ok(action) => action,
                Err(_) => return json_why(StatusCode::BAD_REQUEST, "invalid browser action"),
            }
        };
        if lease.is_revoked() {
            return json_why(StatusCode::UNAUTHORIZED, "browser session was revoked");
        }
        let outcome = tokio::select! {
            _ = lease.wait_revoked() => return unknown_outcome(),
            outcome = self.session.call(action.into_action()) => outcome,
        };
        match outcome {
            Ok(ActionResult::Completed { value }) => match BrowserReply::from_value(value) {
                Ok(answer) => reply(
                    StatusCode::from_u16(answer.status).expect("validated status"),
                    answer.body,
                    answer.content.mime(),
                ),
                Err(_) => json_why(StatusCode::BAD_GATEWAY, "invalid repo response"),
            },
            Ok(ActionResult::Refused { why }) => json_why(StatusCode::CONFLICT, &why),
            Err(CallError::Disconnected) => json_why(
                StatusCode::SERVICE_UNAVAILABLE,
                "repo connector is disconnected",
            ),
            Err(CallError::OutcomeUnknown) => unknown_outcome(),
        }
    }

    async fn upload(&self, req: Request<Incoming>, mut lease: BrowserLease) -> Reply {
        if !same_origin(req.headers()) {
            return reply(StatusCode::FORBIDDEN, "cross-origin request", "text/plain");
        }
        let Ok(_slot) = self.upload_slots.try_acquire() else {
            return json_why(
                StatusCode::TOO_MANY_REQUESTS,
                "another upload is in progress",
            );
        };
        let media_type = req
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        if media_type.len() > 256 {
            return json_why(StatusCode::BAD_REQUEST, "invalid attachment type");
        }
        let body = http_body_util::Limited::new(req.into_body(), bridge::MAX_UPLOAD_BYTES);
        let Ok(bytes) = body.collect().await.map(|body| body.to_bytes()) else {
            return json_why(
                StatusCode::PAYLOAD_TOO_LARGE,
                "that file is too big to attach",
            );
        };
        if bytes.is_empty() {
            return json_why(StatusCode::BAD_REQUEST, "an empty file");
        }
        if lease.is_revoked() {
            return json_why(StatusCode::UNAUTHORIZED, "browser session was revoked");
        }
        let outcome = tokio::select! {
            _ = lease.wait_revoked() => return unknown_outcome(),
            outcome = self.session.upload(media_type, bytes.to_vec()) => outcome,
        };
        match outcome {
            Ok(ActionResult::Completed { value }) => match BrowserReply::from_value(value) {
                Ok(answer) => reply(
                    StatusCode::from_u16(answer.status).expect("validated status"),
                    answer.body,
                    answer.content.mime(),
                ),
                Err(_) => json_why(StatusCode::BAD_GATEWAY, "invalid repo response"),
            },
            Ok(ActionResult::Refused { why }) => json_why(StatusCode::CONFLICT, &why),
            Err(CallError::Disconnected) => json_why(
                StatusCode::SERVICE_UNAVAILABLE,
                "repo connector is disconnected",
            ),
            Err(CallError::OutcomeUnknown) => unknown_outcome(),
        }
    }
}

#[derive(Deserialize)]
struct SayInput {
    pane: String,
    label: String,
    session: Option<String>,
    text: String,
}
#[derive(Deserialize)]
struct StopInput {
    pane: String,
    label: String,
    session: Option<String>,
}
#[derive(Deserialize)]
struct ReopenInput {
    label: String,
}
#[derive(Deserialize)]
struct BlockInput {
    agent: String,
    name: String,
    question: String,
    answer: String,
}

fn parse_action(path: &str, bytes: &[u8]) -> anyhow::Result<BrowserRequest> {
    Ok(match path {
        "say" => {
            let input: SayInput = serde_json::from_slice(bytes)?;
            BrowserRequest::Say {
                pane: input.pane,
                label: input.label,
                session: input.session,
                text: input.text,
            }
        }
        "stop" => {
            let input: StopInput = serde_json::from_slice(bytes)?;
            BrowserRequest::Stop {
                pane: input.pane,
                label: input.label,
                session: input.session,
            }
        }
        "blocks/answer" => {
            let input: BlockInput = serde_json::from_slice(bytes)?;
            BrowserRequest::BlockAnswer {
                agent: input.agent,
                name: input.name,
                question: input.question,
                answer: input.answer,
            }
        }
        "agents/reopen" => {
            let input: ReopenInput = serde_json::from_slice(bytes)?;
            BrowserRequest::Reopen { label: input.label }
        }
        "queue" | "stash" | "agents" => {
            let ask: serde_json::Value = serde_json::from_slice(bytes)?;
            anyhow::ensure!(ask.is_object(), "expected an object");
            match path {
                "queue" => BrowserRequest::Queue { ask },
                "stash" => BrowserRequest::Stash { ask },
                _ => BrowserRequest::Agent { ask },
            }
        }
        _ => anyhow::bail!("unsupported browser action"),
    })
}

fn json_why(status: StatusCode, why: &str) -> Reply {
    reply(
        status,
        serde_json::json!({"why": why}).to_string(),
        "application/json",
    )
}

fn unknown_outcome() -> Reply {
    reply(
        StatusCode::BAD_GATEWAY,
        serde_json::json!({
            "code": "outcome_unknown",
            "why": "outcome unknown after dispatch; check the agent before retrying"
        })
        .to_string(),
        "application/json",
    )
}

fn valid_site_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 || path.contains('\\') {
        return false;
    }
    let mut parts = path.split('/');
    let first = parts.next().unwrap_or_default();
    let second = parts.next();
    if parts.next().is_some() {
        return false;
    }
    let valid = |part: &str| !part.is_empty() && part != "." && part != "..";
    if !valid(first) || second.is_some_and(|part| !valid(part)) {
        return false;
    }
    match second {
        None => matches!(first, "index.html" | "style.css"),
        Some(file) => {
            matches!(first, "plan" | "queue" | "stash" | "commit")
                && file.ends_with(".html")
                && !file.to_ascii_lowercase().contains("%2e%2e")
                && !file.to_ascii_lowercase().contains("%2f")
                && !file.to_ascii_lowercase().contains("%5c")
        }
    }
}

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let Some(origin) = headers.get("origin").and_then(|h| h.to_str().ok()) else {
        return false;
    };
    let Some(authority) = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    else {
        return false;
    };
    authority.eq_ignore_ascii_case(host)
}

async fn pump(
    socket: hyper_tungstenite::HyperWebsocketStream,
    session: Arc<Session>,
    mut lease: BrowserLease,
) -> anyhow::Result<()> {
    use std::collections::VecDeque;

    let (mut view, mut events) = session.subscribe_with_snapshot().await;
    if view.snapshot.is_none() {
        return Ok(());
    }
    let (mut outgoing, mut incoming) = socket.split();
    let mut queue: VecDeque<String> = replay(&view)?.into();
    // `feed` can be cancelled before a frame is accepted. After it
    // succeeds, remove that frame exactly once and retry only `flush`,
    // which is safe to interrupt for revocation or peer input.
    let mut unflushed = false;
    loop {
        if let Some(frame) = queue.front().cloned() {
            tokio::select! {
                biased;
                _ = lease.wait_revoked() => return Ok(()),
                message = incoming.next() => match message {
                    Some(Ok(hyper_tungstenite::tungstenite::Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                    // A Ping can queue an automatic Pong inside the sink.
                    Some(Ok(_)) => unflushed = true,
                },
                sent = outgoing.feed(hyper_tungstenite::tungstenite::Message::Text(frame.into())) => {
                    sent?;
                    queue.pop_front();
                    unflushed = true;
                },
            }
        } else if unflushed {
            tokio::select! {
                biased;
                _ = lease.wait_revoked() => return Ok(()),
                message = incoming.next() => match message {
                    Some(Ok(hyper_tungstenite::tungstenite::Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                    Some(Ok(_)) => {},
                },
                flushed = outgoing.flush() => {
                    flushed?;
                    unflushed = false;
                },
            }
        } else {
            tokio::select! {
                biased;
                _ = lease.wait_revoked() => return Ok(()),
                message = incoming.next() => match message {
                    Some(Ok(hyper_tungstenite::tungstenite::Message::Close(_))) | None | Some(Err(_)) => return Ok(()),
                    // Flush a Pong even if no page frame is waiting.
                    Some(Ok(_)) => unflushed = true,
                },
                event = events.recv() => match event {
                    Ok(event) => {
                        let mut next = view.clone();
                        if bridge::apply_event(&mut next, event.clone()).is_err() {
                            (view, events) = session.subscribe_with_snapshot().await;
                            if view.snapshot.is_none() { return Ok(()); }
                            queue.extend(replay(&view)?);
                            continue;
                        }
                        if next.revision == view.revision { continue; }
                        view = next;
                        match event {
                            SessionEvent::Snapshot { .. } => queue.extend(replay(&view)?),
                            SessionEvent::Update { update, .. } => {
                                queue.extend(serde_json::from_value::<BrowserDelta>(update.state)?.live);
                            }
                            SessionEvent::Disconnected { .. } => return Ok(()),
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        (view, events) = session.subscribe_with_snapshot().await;
                        if view.snapshot.is_none() { return Ok(()); }
                        queue.extend(replay(&view)?);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                },
            }
        }
    }
}

fn replay(view: &SessionView) -> anyhow::Result<Vec<String>> {
    let Some(snapshot) = &view.snapshot else {
        return Ok(Vec::new());
    };
    Ok(BrowserFrames::from_state(&snapshot.state)?.replay)
}

fn reply(status: StatusCode, body: impl Into<Bytes>, content_type: &'static str) -> Reply {
    bytes(status, body, content_type)
}

fn bytes(status: StatusCode, body: impl Into<Bytes>, content_type: &'static str) -> Reply {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(Full::new(body.into()))
        .expect("static response")
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use futures_util::{SinkExt, StreamExt};
    use http_body_util::Full;
    use hyper::body::Bytes;
    use hyper::service::service_fn;
    use hyper::{HeaderMap, Request, header};
    use hyper_util::rt::TokioIo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Mutex as TokioMutex, broadcast, watch};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::{
        BrowserAdmission, BrowserDelta, BrowserFrames, BrowserLease, WebService, same_origin,
        valid_site_path,
    };
    use crate::assets::{BrowserAuth, PageOptions};
    use crate::bridge::{Action, ActionResult, Backend, Session, Snapshot, Update};
    use crate::request::{BrowserReply, BrowserRequest, Content};

    struct Admission(watch::Sender<bool>);

    #[async_trait]
    impl BrowserAdmission for Admission {
        async fn admit(&self, headers: &HeaderMap, session: &str) -> Option<BrowserLease> {
            (session == "team" && headers.get("cookie")?.to_str().ok()? == "session=allowed")
                .then(|| BrowserLease::new(self.0.subscribe()))
        }
    }

    struct Fixture {
        updates: broadcast::Sender<Update>,
        calls: Arc<TokioMutex<Vec<BrowserRequest>>>,
        uploads: Arc<TokioMutex<Vec<(String, Vec<u8>)>>>,
    }

    #[async_trait]
    impl Backend for Fixture {
        async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>) {
            (
                Snapshot {
                    sequence: 0,
                    state: serde_json::to_value(BrowserFrames {
                        replay: vec!["event: status\ndata: {\"phase\":\"first\"}\n\n".into()],
                        live: vec![],
                    })
                    .unwrap(),
                },
                self.updates.subscribe(),
            )
        }

        async fn dispatch(&self, action: Action) -> ActionResult {
            let Ok(action) = BrowserRequest::from_action(action) else {
                return ActionResult::Refused {
                    why: "unsupported action".into(),
                };
            };
            let hangs = matches!(&action, BrowserRequest::Say { text, .. } if text == "hang");
            self.calls.lock().await.push(action);
            if hangs {
                return std::future::pending::<ActionResult>().await;
            }
            ActionResult::Completed {
                value: BrowserReply {
                    status: 204,
                    content: Content::Plain,
                    body: String::new(),
                }
                .into_value(),
            }
        }

        async fn upload(&self, media_type: String, bytes: Vec<u8>) -> ActionResult {
            self.uploads.lock().await.push((media_type, bytes));
            ActionResult::Completed {
                value: BrowserReply {
                    status: 200,
                    content: Content::Json,
                    body: r#"{"path":"/repo/.clank/attachments/a.png"}"#.into(),
                }
                .into_value(),
            }
        }
    }

    struct SlowRepo {
        snapshot: TokioMutex<Snapshot>,
        updates: broadcast::Sender<Update>,
    }

    impl SlowRepo {
        fn new() -> Self {
            let (updates, _) = broadcast::channel(128);
            Self {
                snapshot: TokioMutex::new(Snapshot {
                    sequence: 0,
                    state: serde_json::to_value(BrowserFrames {
                        replay: vec!["event: status\ndata: {\"phase\":\"first\"}\n\n".into()],
                        live: vec![],
                    })
                    .unwrap(),
                }),
                updates,
            }
        }

        async fn publish(&self, sequence: usize, large: &str) {
            let frame = format!(
                "event: status\ndata: {}\n\n",
                serde_json::json!({"sequence":sequence,"text":large})
            );
            let mut snapshot = self.snapshot.lock().await;
            snapshot.sequence += 1;
            snapshot.state = serde_json::to_value(BrowserFrames {
                replay: vec![frame.clone()],
                live: vec![],
            })
            .unwrap();
            let _ = self.updates.send(Update {
                sequence: snapshot.sequence,
                state: serde_json::to_value(BrowserDelta { live: vec![frame] }).unwrap(),
            });
        }
    }

    #[async_trait]
    impl Backend for SlowRepo {
        async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>) {
            let updates = self.updates.subscribe();
            (self.snapshot.lock().await.clone(), updates)
        }

        async fn dispatch(&self, _: Action) -> ActionResult {
            ActionResult::Refused {
                why: "no actions in slow reader fixture".into(),
            }
        }
    }

    #[test]
    fn websocket_origin_matches_the_browser_host() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "team.example:8443".parse().unwrap());
        headers.insert("origin", "https://team.example:8443".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert("origin", "https://other.example:8443".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.remove("origin");
        assert!(!same_origin(&headers));
    }

    #[test]
    fn browser_frames_require_a_complete_replay() {
        let state = serde_json::json!({"replay":["event: status\ndata: {}\n\n"],"live":[]});
        assert_eq!(BrowserFrames::from_state(&state).unwrap().replay.len(), 1);
        assert!(BrowserFrames::from_state(&serde_json::json!({"live":[]})).is_err());
    }

    #[test]
    fn generated_pages_reject_traversal_before_dispatch() {
        for path in [
            "",
            ".",
            "..",
            "../secret",
            "plan/",
            "plan/./a.html",
            "plan/../a.html",
            "plan/%2e%2e.html",
            "plan/a%2Fb.html",
        ] {
            assert!(!valid_site_path(path), "{path}");
        }
        for path in [
            "index.html",
            "style.css",
            "plan/my-plan.html",
            "queue/a%20plan.html",
            "commit/abc1234.html",
        ] {
            assert!(valid_site_path(path), "{path}");
        }
    }

    #[tokio::test]
    async fn mounted_host_serves_an_admitted_live_session_and_revokes_it() {
        let (revoker, _) = watch::channel(false);
        let session = Arc::new(Session::new("team"));
        let web = Arc::new(
            WebService::new(
                session.clone(),
                PageOptions {
                    session: "team",
                    base_path: "/app/team/",
                    auth: BrowserAuth::Host {
                        login_path: "/auth/login",
                        sign_out_path: None,
                    },
                },
                Arc::new(Admission(revoker.clone())),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let observed = session.clone();
        let server = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let web = web.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    let svc = service_fn(move |mut req: Request<hyper::body::Incoming>| {
                        let web = web.clone();
                        let session = session.clone();
                        async move {
                            let response = if req.uri().path() == "/connector" {
                                let (response, upgrade) =
                                    hyper_tungstenite::upgrade(&mut req, None).unwrap();
                                tokio::spawn(async move {
                                    if let Ok(socket) = upgrade.await {
                                        let _ = session
                                            .accept(socket, |_, credential| async move {
                                                credential == "connector-secret"
                                            })
                                            .await;
                                    }
                                });
                                response.map(|_| Full::new(Bytes::new()))
                            } else {
                                web.serve(req).await
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .with_upgrades()
                        .await;
                });
            }
        });

        let (updates, _) = broadcast::channel(8);
        let calls = Arc::new(TokioMutex::new(Vec::new()));
        let uploads = Arc::new(TokioMutex::new(Vec::new()));
        let endpoint = format!("ws://{addr}/connector");
        let backend = Arc::new(Fixture {
            updates: updates.clone(),
            calls: calls.clone(),
            uploads: uploads.clone(),
        });
        let connector = tokio::spawn(async move {
            crate::bridge::connect(&endpoint, "team", "connector-secret", backend).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !observed.connected().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut unauthenticated = TcpStream::connect(addr).await.unwrap();
        unauthenticated
            .write_all(b"GET /app/team/ HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(3),
            unauthenticated.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 303"), "{response}");
        assert!(response.contains("location: /auth/login"), "{response}");

        let mut authorized = TcpStream::connect(addr).await.unwrap();
        authorized
            .write_all(b"GET /app/team/ HTTP/1.1\r\nHost: localhost\r\nCookie: session=allowed\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(3),
            authorized.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("href=\"/app/team/xterm.css\""));
        assert!(response.contains("content-security-policy:"));
        assert!(!response.contains("/*__CLANK_DOOR__*/"));

        let mut traversal = TcpStream::connect(addr).await.unwrap();
        traversal
            .write_all(b"GET /app/team/html/plan/%2e%2e.html HTTP/1.1\r\nHost: localhost\r\nCookie: session=allowed\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), traversal.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(
            String::from_utf8(response)
                .unwrap()
                .starts_with("HTTP/1.1 400")
        );
        assert!(
            calls.lock().await.is_empty(),
            "invalid paths stay on the host"
        );

        let mut browser_request = format!("ws://{addr}/app/team/events")
            .into_client_request()
            .unwrap();
        browser_request
            .headers_mut()
            .insert("cookie", "session=allowed".parse().unwrap());
        browser_request
            .headers_mut()
            .insert("origin", format!("http://{addr}").parse().unwrap());
        let (mut browser, _) = tokio_tungstenite::connect_async(browser_request)
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(3), browser.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(first.to_text().unwrap().contains("first"));

        updates
            .send(Update {
                sequence: 1,
                state: serde_json::to_value(BrowserDelta {
                    live: vec!["event: status\ndata: {\"phase\":\"second\"}\n\n".into()],
                })
                .unwrap(),
            })
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(3), browser.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(second.to_text().unwrap().contains("second"));

        let mut later_request = format!("ws://{addr}/app/team/events")
            .into_client_request()
            .unwrap();
        later_request
            .headers_mut()
            .insert("cookie", "session=allowed".parse().unwrap());
        later_request
            .headers_mut()
            .insert("origin", format!("http://{addr}").parse().unwrap());
        let (mut later, _) = tokio_tungstenite::connect_async(later_request)
            .await
            .unwrap();
        let retained = tokio::time::timeout(Duration::from_secs(3), later.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(retained.to_text().unwrap().contains("second"));

        let body = r#"{"pane":"terminal_1","label":"agent","session":"s1","text":"hello"}"#;
        let mut action = TcpStream::connect(addr).await.unwrap();
        let ask = format!(
            "POST /app/team/say HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nCookie: session=allowed\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut cross_origin = TcpStream::connect(addr).await.unwrap();
        cross_origin
            .write_all(
                ask.replace(
                    &format!("Origin: http://{addr}"),
                    "Origin: http://other.example",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut refused = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(3),
            cross_origin.read_to_end(&mut refused),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            String::from_utf8(refused)
                .unwrap()
                .starts_with("HTTP/1.1 403")
        );
        assert!(calls.lock().await.is_empty());
        action.write_all(ask.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), action.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 204"), "{response}");
        assert_eq!(
            calls.lock().await.as_slice(),
            &[BrowserRequest::Say {
                pane: "terminal_1".into(),
                label: "agent".into(),
                session: Some("s1".into()),
                text: "hello".into()
            }]
        );

        let bytes = vec![0xa5; 128 * 1024 + 3];
        let mut upload = TcpStream::connect(addr).await.unwrap();
        upload.write_all(format!(
            "POST /app/team/upload HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nCookie: session=allowed\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        ).as_bytes()).await.unwrap();
        upload.write_all(&bytes).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), upload.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("/repo/.clank/attachments/a.png"));
        assert_eq!(
            uploads.lock().await.as_slice(),
            &[("image/png".into(), bytes)]
        );

        let hanging_body = r#"{"pane":"terminal_1","label":"agent","session":"s1","text":"hang"}"#;
        let hanging_ask = format!(
            "POST /app/team/say HTTP/1.1\r\nHost: {addr}\r\nOrigin: http://{addr}\r\nCookie: session=allowed\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{hanging_body}",
            hanging_body.len()
        );
        let mut hanging = TcpStream::connect(addr).await.unwrap();
        hanging.write_all(hanging_ask.as_bytes()).await.unwrap();
        let pending = tokio::spawn(async move {
            let mut response = Vec::new();
            hanging.read_to_end(&mut response).await.unwrap();
            String::from_utf8(response).unwrap()
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while calls.lock().await.len() != 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        revoker.send(true).unwrap();
        let unknown = tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap();
        assert!(unknown.starts_with("HTTP/1.1 502"), "{unknown}");
        assert!(unknown.contains("outcome_unknown"), "{unknown}");
        let closed = tokio::time::timeout(Duration::from_secs(3), browser.next())
            .await
            .unwrap();
        assert!(!matches!(
            closed,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
        ));
        revoker.send_replace(false);
        connector.abort();
        let _ = connector.await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while observed.connected().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut disconnected = TcpStream::connect(addr).await.unwrap();
        disconnected.write_all(ask.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(3),
            disconnected.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert_eq!(
            calls.lock().await.len(),
            2,
            "a disconnected request cannot reach the repo"
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pings_during_backpressured_writes_deliver_each_browser_frame_once() {
        use tokio_tungstenite::tungstenite::Message;

        let (revoke, _) = watch::channel(false);
        let session = Arc::new(Session::new("team"));
        let web = Arc::new(
            WebService::new(
                session.clone(),
                PageOptions {
                    session: "team",
                    base_path: "/app/",
                    auth: BrowserAuth::Host {
                        login_path: "/auth/login",
                        sign_out_path: None,
                    },
                },
                Arc::new(Admission(revoke)),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served_session = session.clone();
        let host = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let web = web.clone();
                let session = served_session.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |mut request: Request<hyper::body::Incoming>| {
                        let web = web.clone();
                        let session = session.clone();
                        async move {
                            let response = if request.uri().path() == "/connector" {
                                let (response, upgrade) =
                                    hyper_tungstenite::upgrade(&mut request, None).unwrap();
                                tokio::spawn(async move {
                                    if let Ok(socket) = upgrade.await {
                                        let _ = session
                                            .accept(socket, |_, credential| async move {
                                                credential == "connector-secret"
                                            })
                                            .await;
                                    }
                                });
                                response.map(|_| Full::new(Bytes::new()))
                            } else {
                                web.serve(request).await
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });

        let repo = Arc::new(SlowRepo::new());
        let endpoint = format!("ws://{addr}/connector");
        let connector_repo = repo.clone();
        let connector = tokio::spawn(async move {
            crate::bridge::connect(&endpoint, "team", "connector-secret", connector_repo).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !session.connected().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut request = format!("ws://{addr}/app/events")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("cookie", "session=allowed".parse().unwrap());
        request
            .headers_mut()
            .insert("origin", format!("http://{addr}").parse().unwrap());
        let (mut browser, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(3), browser.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .to_text()
                .unwrap()
                .contains("first")
        );

        let (mut sender, mut receiver) = browser.split();
        let pings = tokio::spawn(async move {
            for _ in 0..400 {
                if sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        const SENT: usize = 60;
        let publisher = tokio::spawn(async move {
            let bulk = "y".repeat(256 * 1024);
            for sequence in 0..SENT {
                repo.publish(sequence, &bulk).await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        });
        let mut seen = Vec::new();
        let collected = tokio::time::timeout(Duration::from_secs(30), async {
            while seen.len() < SENT {
                match receiver.next().await {
                    Some(Ok(Message::Text(frame))) => {
                        let Some(data) = frame.split_once("data: ").map(|(_, data)| data) else {
                            continue;
                        };
                        let payload: serde_json::Value = serde_json::from_str(data.trim()).unwrap();
                        if let Some(sequence) = payload["sequence"].as_u64() {
                            seen.push(sequence as usize);
                            // Let the producer fill the socket while the peer keeps Pinging.
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }
                    Some(Ok(_)) => {}
                    None | Some(Err(_)) => {
                        panic!("browser socket closed after {} frames", seen.len())
                    }
                }
            }
        })
        .await;
        assert!(
            collected.is_ok(),
            "received {} of {SENT} frames",
            seen.len()
        );
        assert_eq!(
            seen,
            (0..SENT).collect::<Vec<_>>(),
            "each frame exactly once"
        );
        publisher.await.unwrap();
        pings.abort();
        connector.abort();
        host.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn revoking_a_browser_stops_a_stalled_websocket_write() {
        let (revoke, _) = watch::channel(false);
        let session = Arc::new(Session::new("team"));
        let web = Arc::new(
            WebService::new(
                session.clone(),
                PageOptions {
                    session: "team",
                    base_path: "/app/",
                    auth: BrowserAuth::Host {
                        login_path: "/auth/login",
                        sign_out_path: None,
                    },
                },
                Arc::new(Admission(revoke.clone())),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = web.clone();
        let host = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let web = served.clone();
                let session = session.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |mut request: Request<hyper::body::Incoming>| {
                        let web = web.clone();
                        let session = session.clone();
                        async move {
                            let response = if request.uri().path() == "/connector" {
                                let (response, upgrade) =
                                    hyper_tungstenite::upgrade(&mut request, None).unwrap();
                                tokio::spawn(async move {
                                    if let Ok(socket) = upgrade.await {
                                        let _ = session
                                            .accept(socket, |_, credential| async move {
                                                credential == "connector-secret"
                                            })
                                            .await;
                                    }
                                });
                                response.map(|_| Full::new(Bytes::new()))
                            } else {
                                web.serve(request).await
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await;
                });
            }
        });
        let repo = Arc::new(SlowRepo::new());
        let endpoint = format!("ws://{addr}/connector");
        let connector_repo = repo.clone();
        let connector = tokio::spawn(async move {
            crate::bridge::connect(&endpoint, "team", "connector-secret", connector_repo).await
        });
        let mut browser_request = format!("ws://{addr}/app/events")
            .into_client_request()
            .unwrap();
        browser_request
            .headers_mut()
            .insert("cookie", "session=allowed".parse().unwrap());
        browser_request
            .headers_mut()
            .insert("origin", format!("http://{addr}").parse().unwrap());
        tokio::time::timeout(Duration::from_secs(3), async {
            while !web.session.connected().await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let (mut browser, _) = tokio_tungstenite::connect_async(browser_request)
            .await
            .unwrap();
        // The client deliberately stops reading. Overfill loopback's
        // send buffer while leaving the connection alive.
        let wide = "x".repeat(64 * 1024);
        for sequence in 0..240 {
            repo.publish(sequence, &wide).await;
            tokio::time::sleep(Duration::from_millis(3)).await;
        }
        assert_eq!(web.sockets.lock().unwrap().len(), 1);
        revoke.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let done = {
                    let mut sockets = web.sockets.lock().unwrap();
                    while sockets.try_join_next().is_some() {}
                    sockets.is_empty()
                };
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("revocation stops a browser even when its writes cannot flush");
        let graceful_close = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match browser.next().await {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Close(_))) => return true,
                    None | Some(Err(_)) => return false,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await
        .expect("the revoked socket closes after buffered frames drain");
        assert!(
            !graceful_close,
            "revocation drops the socket without a goodbye frame"
        );
        connector.abort();
        host.abort();
    }
}
