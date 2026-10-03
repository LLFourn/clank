//! The default browser host. It owns the listener and Door policy;
//! repository state reaches it only through an admitted connector.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use clank_web::assets::{BrowserAuth, PageOptions};
use clank_web::bridge::Session;
use clank_web::service::{BrowserAdmission, BrowserLease, Reply, WebService};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, LOCATION, ORIGIN, SET_COOKIE};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

use crate::door::{self, Door, Link};
use crate::origin::{alone_on_its_connection, repaired};

const LOGIN_PAGE: &str = "/*__CLANK_DOOR__*/";

struct Admission(Arc<Door>);

#[async_trait]
impl BrowserAdmission for Admission {
    async fn admit(&self, headers: &HeaderMap, _: &str) -> Option<BrowserLease> {
        let admitted = self
            .0
            .admit(headers.get(COOKIE).and_then(|value| value.to_str().ok()))?;
        let door = self.0.clone();
        let (revoke, lease) = watch::channel(false);
        let mut changes = door.watch_revocations();
        tokio::spawn(async move {
            loop {
                if tokio::time::Instant::now() >= admitted.expires
                    || !door.is_live(&admitted.id_hash)
                {
                    let _ = revoke.send(true);
                    return;
                }
                tokio::select! {
                    _ = revoke.closed() => return,
                    _ = changes.changed() => {},
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {},
                }
            }
        });
        Some(BrowserLease::new(lease))
    }
}

struct Context {
    session: Arc<Session>,
    web: Arc<WebService>,
    door: Arc<Door>,
    credential: String,
    admin_credential: String,
    nonce: String,
    public_url: Arc<RwLock<Option<url::Url>>>,
    workers: Mutex<JoinSet<()>>,
}

/// A listener that accepts one named repo connector and browsers
/// admitted by the default Door. It has no repo path or pane handle.
pub struct Host {
    pub local_url: String,
    pub connector_url: String,
    pub credential: String,
    /// Separate local capability for managing this host's browser policy.
    pub admin_credential: String,
    pub nonce: String,
    pub session: Arc<Session>,
    pub door: Arc<Door>,
    public_url: Arc<RwLock<Option<url::Url>>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Host {
    pub async fn start(port: u16, session: String, home: Option<PathBuf>) -> anyhow::Result<Self> {
        let listener = match TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await {
            Ok(listener) => listener,
            Err(_) if port != 0 => TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await?,
            Err(error) => return Err(error.into()),
        };
        let port = listener.local_addr()?.port();
        let credential = door::random_token();
        let admin_credential = door::random_token();
        let nonce = door::random_token();
        let door = Arc::new(Door::new(home));
        let public_url = Arc::new(RwLock::new(None));
        let session_state = Arc::new(Session::new(session.clone()));
        let web = Arc::new(WebService::new(
            session_state.clone(),
            PageOptions {
                session: &session,
                base_path: "/",
                auth: BrowserAuth::ClankDoor {
                    script: crate::DOOR_JS,
                },
            },
            Arc::new(Admission(door.clone())),
        )?);
        let context = Arc::new(Context {
            session: session_state.clone(),
            web,
            door: door.clone(),
            credential: credential.clone(),
            admin_credential: admin_credential.clone(),
            nonce,
            public_url: public_url.clone(),
            workers: Mutex::new(JoinSet::new()),
        });
        let (stop, stopped) = watch::channel(false);
        let task = tokio::spawn(serve(listener, context.clone(), stopped));
        Ok(Self {
            local_url: format!("http://localhost:{port}"),
            connector_url: format!("ws://127.0.0.1:{port}/connector"),
            credential,
            admin_credential,
            nonce: context.nonce.clone(),
            session: session_state,
            door,
            public_url,
            stop,
            task,
        })
    }

    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }

    /// Set the proven public tunnel URL after its readiness probe.
    /// This also marks its browser requests as secure if a forwarding
    /// layer omits the usual `X-Forwarded-Proto` header.
    pub fn set_public_url(&self, url: &str) -> anyhow::Result<()> {
        let parsed = url::Url::parse(url)?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some(),
            "invalid public URL"
        );
        *self
            .public_url
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(parsed);
        Ok(())
    }
}

async fn serve(listener: TcpListener, context: Arc<Context>, mut stop: watch::Receiver<bool>) {
    loop {
        let accepted = tokio::select! {
            _ = stop.changed() => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((stream, peer)) = accepted else { break };
        let task_context = context.clone();
        let mut stopped = stop.clone();
        let task = async move {
            let stream = tokio::select! {
                _ = stopped.changed() => return,
                answer = repaired(stream) => match answer { Ok(stream) => stream, Err(_) => return },
            };
            let service = service_fn(move |request: Request<Incoming>| {
                let context = task_context.clone();
                async move {
                    let mut answer = route(request, peer.ip(), context).await;
                    alone_on_its_connection(&mut answer);
                    Ok::<_, Infallible>(answer)
                }
            });
            let connection = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            tokio::pin!(connection);
            tokio::select! {
                _ = connection.as_mut() => {},
                _ = stopped.changed() => {
                    connection.as_mut().graceful_shutdown();
                    let _ = connection.await;
                },
            }
        };
        let mut workers = context
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        workers.spawn(task);
        while workers.try_join_next().is_some() {}
    }
    let mut workers = std::mem::take(
        &mut *context
            .workers
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
    workers.shutdown().await;
}

async fn route(mut request: Request<Incoming>, peer: IpAddr, context: Arc<Context>) -> Reply {
    if request.uri().path().starts_with("/admin/") {
        return admin_route(request, &context).await;
    }
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/instance") => {
            return json(StatusCode::OK, serde_json::json!({"nonce":context.nonce}));
        }
        (&Method::GET, "/instance/stream") => {
            if !hyper_tungstenite::is_upgrade_request(&request) {
                return text(StatusCode::UPGRADE_REQUIRED, "upgrade to a websocket");
            }
            let Ok((answer, socket)) = hyper_tungstenite::upgrade(&mut request, None) else {
                return text(StatusCode::BAD_REQUEST, "invalid websocket upgrade");
            };
            let nonce = context.nonce.clone();
            context
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .spawn(async move {
                    if let Ok(mut socket) = socket.await {
                        let _ = socket
                            .send(hyper_tungstenite::tungstenite::Message::text(nonce))
                            .await;
                        while let Some(Ok(frame)) = socket.next().await {
                            if frame.is_close() {
                                break;
                            }
                        }
                    }
                });
            return answer.map(|_| Full::new(Bytes::new()));
        }
        (&Method::GET, "/connector") => {
            if !hyper_tungstenite::is_upgrade_request(&request) {
                return text(StatusCode::UPGRADE_REQUIRED, "upgrade to a websocket");
            }
            let Ok((answer, socket)) = hyper_tungstenite::upgrade(
                &mut request,
                Some(clank_web::bridge::websocket_config()),
            ) else {
                return text(StatusCode::BAD_REQUEST, "invalid websocket upgrade");
            };
            let expected = context.credential.clone();
            let session = context.session.clone();
            context
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .spawn(async move {
                    if let Ok(socket) = socket.await {
                        let _ =
                            session
                                .accept(socket, move |_, credential| {
                                    let expected = expected.clone();
                                    async move {
                                        expected.as_bytes().ct_eq(credential.as_bytes()).into()
                                    }
                                })
                                .await;
                    }
                });
            return answer.map(|_| Full::new(Bytes::new()));
        }
        (&Method::GET, "/login") => {
            if let Some(token) = door::query_token(request.uri().query()) {
                if !context.door.attempt_allowed(peer) {
                    return text(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
                }
                match context.door.consume_shared(Link::Login, &token) {
                    Ok(true) => {}
                    Ok(false) => return redirect("/login?why=link"),
                    Err(error) => {
                        return text(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}"));
                    }
                }
                return match context.door.open_session("the TUI's link") {
                    Ok(cookie) => with_cookie(redirect("/"), &cookie, secure(&request, &context)),
                    Err(error) => text(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
                };
            }
            return login_page();
        }
        (&Method::GET, "/jsqr.js") => {
            return reply(
                StatusCode::OK,
                clank_web::assets::JSQR,
                "text/javascript; charset=utf-8",
            );
        }
        (&Method::POST, "/login") => {
            if !same_origin(request.headers()) {
                return text(StatusCode::FORBIDDEN, "cross-origin request");
            }
            if !context.door.attempt_allowed(peer) {
                return text(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
            }
            let secure = secure(&request, &context);
            let body = http_body_util::Limited::new(request.into_body(), 8 * 1024);
            let Ok(body) = body.collect().await.map(|body| body.to_bytes()) else {
                return text(StatusCode::PAYLOAD_TOO_LARGE, "too long");
            };
            let Ok(paste) = serde_json::from_slice::<door::Paste>(&body) else {
                return text(StatusCode::BAD_REQUEST, "expected a token");
            };
            return match context.door.open_session_for_token(paste.token.trim()) {
                Ok(Some(cookie)) => with_cookie(text(StatusCode::NO_CONTENT, ""), &cookie, secure),
                Ok(None) => json(
                    StatusCode::FORBIDDEN,
                    serde_json::json!({"refused":"token","say":"that is not this machine's token"}),
                ),
                Err(error) => text(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
            };
        }
        _ => {}
    }
    let cookie = request
        .headers()
        .get(COOKIE)
        .and_then(|value| value.to_str().ok());
    let Some(admitted) = context.door.admit(cookie) else {
        return context.web.serve(request).await;
    };
    match (request.method(), request.uri().path()) {
        (&Method::GET, "/token") => match context.door.token_for_session(&admitted.id_hash) {
            Ok(Some(token)) => json(StatusCode::OK, serde_json::json!({"token":token})),
            Ok(None) => text(StatusCode::UNAUTHORIZED, "session ended"),
            Err(error) => text(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
        },
        (&Method::POST, "/forget") => {
            if !same_origin(request.headers()) {
                return text(StatusCode::FORBIDDEN, "cross-origin request");
            }
            match context.door.revoke_session(&admitted.id_hash) {
                Ok(()) => {
                    let mut answer = text(StatusCode::NO_CONTENT, "");
                    answer.headers_mut().insert(
                        SET_COOKIE,
                        door::expired_cookie(secure(&request, &context))
                            .parse()
                            .expect("cookie is valid"),
                    );
                    answer
                }
                Err(error) => text(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
            }
        }
        _ => context.web.serve(request).await,
    }
}

async fn admin_route(request: Request<Incoming>, context: &Context) -> Reply {
    let allowed = request
        .headers()
        .get("x-clank-admin")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|presented| {
            context
                .admin_credential
                .as_bytes()
                .ct_eq(presented.as_bytes())
                .into()
        });
    if !allowed {
        return text(
            StatusCode::UNAUTHORIZED,
            "host administration requires its own capability",
        );
    }
    let result = match (request.method(), request.uri().path()) {
        (&Method::GET, "/admin/state") => {
            let token = context.door.token();
            let sessions = context.door.sessions();
            match (token, sessions) {
                (Ok(token), Ok(sessions)) => {
                    return json(
                        StatusCode::OK,
                        serde_json::json!({"token":token,"sessions":sessions}),
                    );
                }
                (Err(error), _) | (_, Err(error)) => Err(error),
            }
        }
        (&Method::POST, "/admin/link") => context
            .door
            .mint_shared(Link::Login)
            .map(|token| serde_json::json!({"token":token})),
        (&Method::POST, "/admin/rotate") => context
            .door
            .rotate_token()
            .map(|token| serde_json::json!({"token":token})),
        (&Method::POST, "/admin/revoke") => {
            let body = http_body_util::Limited::new(request.into_body(), 1024);
            let Ok(body) = body.collect().await.map(|body| body.to_bytes()) else {
                return text(StatusCode::PAYLOAD_TOO_LARGE, "too long");
            };
            let Ok(input) = serde_json::from_slice::<serde_json::Value>(&body) else {
                return text(StatusCode::BAD_REQUEST, "invalid session id");
            };
            let Some(id_hash) = input.get("id_hash").and_then(|value| value.as_str()) else {
                return text(StatusCode::BAD_REQUEST, "missing session id");
            };
            context
                .door
                .revoke_session(id_hash)
                .map(|()| serde_json::json!({}))
        }
        _ => return text(StatusCode::NOT_FOUND, "unknown admin action"),
    };
    match result {
        Ok(value) => json(StatusCode::OK, value),
        Err(error) => text(StatusCode::INTERNAL_SERVER_ERROR, format!("{error:#}")),
    }
}

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get("host").and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
        .is_some_and(|authority| authority.eq_ignore_ascii_case(host))
}

fn secure(request: &Request<Incoming>, context: &Context) -> bool {
    let forwarded = request
        .headers()
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .next()
                .is_some_and(|value| value.trim() == "https")
        });
    let public = context
        .public_url
        .read()
        .unwrap_or_else(|error| error.into_inner());
    forwarded
        || public
            .as_ref()
            .filter(|url| url.scheme() == "https")
            .is_some_and(|url| {
                let Some(host) = request
                    .headers()
                    .get("host")
                    .and_then(|value| value.to_str().ok())
                else {
                    return false;
                };
                let expected = match url.port() {
                    Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
                    None => url.host_str().unwrap_or_default().to_owned(),
                };
                host.eq_ignore_ascii_case(&expected)
            })
}

fn login_page() -> Reply {
    let nonce = door::random_token();
    let page = crate::LOGIN
        .replace(LOGIN_PAGE, crate::DOOR_JS)
        .replace("__CLANK_NONCE__", &nonce);
    let mut answer = reply(StatusCode::OK, page, "text/html; charset=utf-8");
    let policy = format!(
        "default-src 'none'; script-src 'self' 'nonce-{nonce}'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; media-src 'self' blob:; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'"
    );
    answer
        .headers_mut()
        .insert(CONTENT_SECURITY_POLICY, policy.parse().expect("static CSP"));
    answer
}

fn with_cookie(mut answer: Reply, cookie: &str, secure: bool) -> Reply {
    answer.headers_mut().insert(
        SET_COOKIE,
        door::session_cookie(cookie, secure)
            .parse()
            .expect("cookie is valid"),
    );
    answer
}

fn redirect(to: &str) -> Reply {
    let mut answer = text(StatusCode::SEE_OTHER, "");
    answer
        .headers_mut()
        .insert(LOCATION, to.parse().expect("static location"));
    answer
}

fn json(status: StatusCode, body: serde_json::Value) -> Reply {
    reply(status, body.to_string(), "application/json")
}

fn text(status: StatusCode, body: impl Into<Bytes>) -> Reply {
    reply(status, body, "text/plain; charset=utf-8")
}

fn reply(status: StatusCode, body: impl Into<Bytes>, content_type: &'static str) -> Reply {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(Full::new(body.into()))
        .expect("static response")
}
