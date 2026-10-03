//! A small host with its own browser cookie and connector credential.
//! Run with `cargo run -p clank-web --example custom_host`; see
//! `examples/README.md` for the repo-side attachment configuration.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use clank_web::assets::{BrowserAuth, PageOptions};
use clank_web::bridge::Session;
use clank_web::service::{BrowserAdmission, BrowserLease, Reply, WebService};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

type Cookies = Arc<Mutex<HashMap<String, watch::Sender<bool>>>>;
const COOKIE_NAME: &str = "custom_host_session";

struct Admission {
    handle: String,
    cookies: Cookies,
}

#[async_trait]
impl BrowserAdmission for Admission {
    async fn admit(&self, headers: &HeaderMap, handle: &str) -> Option<BrowserLease> {
        if handle != self.handle {
            return None;
        }
        let cookie = cookie(headers)?;
        let sessions = self.cookies.lock().ok()?;
        let sender = sessions.get(&cookie)?;
        Some(BrowserLease::new(sender.subscribe()))
    }
}

struct App {
    session: Arc<Session>,
    web: Arc<WebService>,
    browser_token: String,
    connector_token: String,
    cookies: Cookies,
}

impl App {
    fn new(handle: String, browser_token: String, connector_token: String) -> anyhow::Result<Self> {
        anyhow::ensure!(!browser_token.is_empty(), "browser token is empty");
        anyhow::ensure!(!connector_token.is_empty(), "connector token is empty");
        anyhow::ensure!(
            browser_token != connector_token,
            "browser and connector tokens must differ"
        );
        let cookies = Cookies::default();
        let session = Arc::new(Session::new(handle.clone()));
        let web = Arc::new(WebService::new(
            session.clone(),
            PageOptions {
                session: &handle,
                base_path: "/app/",
                auth: BrowserAuth::Host {
                    login_path: "/auth/login",
                    sign_out_path: Some("/auth/logout"),
                },
            },
            Arc::new(Admission {
                handle: handle.clone(),
                cookies: cookies.clone(),
            }),
        )?);
        Ok(Self {
            session,
            web,
            browser_token,
            connector_token,
            cookies,
        })
    }

    async fn route(self: Arc<Self>, mut request: Request<Incoming>) -> Reply {
        match (request.method(), request.uri().path()) {
            (&Method::GET, "/connector") => {
                if !hyper_tungstenite::is_upgrade_request(&request) {
                    return response(StatusCode::UPGRADE_REQUIRED, "upgrade to WebSocket");
                }
                let Ok((reply, upgrade)) = hyper_tungstenite::upgrade(
                    &mut request,
                    Some(clank_web::bridge::websocket_config()),
                ) else {
                    return response(StatusCode::BAD_REQUEST, "invalid upgrade");
                };
                let session = self.session.clone();
                let credential = self.connector_token.clone();
                tokio::spawn(async move {
                    if let Ok(socket) = upgrade.await {
                        let _ = session
                            .accept(socket, move |_, candidate| {
                                let credential = credential.clone();
                                async move {
                                    bool::from(credential.as_bytes().ct_eq(candidate.as_bytes()))
                                }
                            })
                            .await;
                    }
                });
                return reply.map(|_| Full::new(Bytes::new()));
            }
            (&Method::GET, "/auth/login") => {
                return response(
                    StatusCode::OK,
                    "<!doctype html><title>Host login</title><form method=post action=/auth/login><label>Token <input name=token type=password required></label><button>Sign in</button></form>",
                );
            }
            (&Method::POST, "/auth/login") => {
                if !same_origin(request.headers()) {
                    return response(StatusCode::FORBIDDEN, "cross-origin request");
                }
                let secure = secure_cookie(request.headers());
                let body = http_body_util::Limited::new(request.into_body(), 4096);
                let Ok(body) = body.collect().await.map(|body| body.to_bytes()) else {
                    return response(StatusCode::PAYLOAD_TOO_LARGE, "login too long");
                };
                let token = url::form_urlencoded::parse(&body)
                    .find(|(key, _)| key == "token")
                    .map(|(_, value)| value.into_owned());
                let Some(token) = token else {
                    return response(StatusCode::BAD_REQUEST, "token required");
                };
                if !bool::from(self.browser_token.as_bytes().ct_eq(token.as_bytes())) {
                    return response(StatusCode::FORBIDDEN, "wrong token");
                }
                let mut secret = [0u8; 32];
                if getrandom::fill(&mut secret).is_err() {
                    return response(StatusCode::INTERNAL_SERVER_ERROR, "randomness unavailable");
                }
                let cookie: String = secret.iter().map(|byte| format!("{byte:02x}")).collect();
                let (sender, _) = watch::channel(false);
                self.cookies.lock().unwrap().insert(cookie.clone(), sender);
                let mut answer = redirect("/app/");
                answer.headers_mut().insert(
                    SET_COOKIE,
                    format!(
                        "{COOKIE_NAME}={cookie}; Path=/; HttpOnly; SameSite=Strict{}",
                        if secure { "; Secure" } else { "" }
                    )
                    .parse()
                    .expect("generated cookie is ASCII"),
                );
                return answer;
            }
            (&Method::POST, "/auth/logout") => {
                if !same_origin(request.headers()) {
                    return response(StatusCode::FORBIDDEN, "cross-origin request");
                }
                let secure = secure_cookie(request.headers());
                let Some(cookie) = cookie(request.headers()) else {
                    return response(StatusCode::UNAUTHORIZED, "not signed in");
                };
                let Some(sender) = self.cookies.lock().unwrap().remove(&cookie) else {
                    return response(StatusCode::UNAUTHORIZED, "not signed in");
                };
                let _ = sender.send(true);
                let mut answer = redirect("/auth/login");
                answer.headers_mut().insert(
                    SET_COOKIE,
                    format!(
                        "{COOKIE_NAME}=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{}",
                        if secure { "; Secure" } else { "" }
                    )
                    .parse()
                    .expect("static cookie is valid"),
                );
                return answer;
            }
            _ => {}
        }
        self.web.serve(request).await
    }
}

fn cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get(COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value.to_owned())
}

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
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

fn secure_cookie(headers: &HeaderMap) -> bool {
    headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin.starts_with("https://"))
}

fn response(status: StatusCode, body: impl Into<Bytes>) -> Reply {
    Response::builder()
        .status(status)
        .header("content-type", "text/html; charset=utf-8")
        .body(Full::new(body.into()))
        .expect("static response")
}

fn redirect(to: &str) -> Reply {
    let mut answer = response(StatusCode::SEE_OTHER, "");
    answer
        .headers_mut()
        .insert(LOCATION, to.parse().expect("static path"));
    answer
}

async fn serve(listener: TcpListener, app: Arc<App>, mut stop: watch::Receiver<bool>) {
    let mut workers = JoinSet::new();
    loop {
        let accepted = tokio::select! {
            _ = stop.changed() => break,
            answer = listener.accept() => answer,
        };
        let Ok((stream, _)) = accepted else { break };
        let app = app.clone();
        workers.spawn(async move {
            let service = service_fn(move |request| {
                let app = app.clone();
                async move { Ok::<_, Infallible>(app.route(request).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades()
                .await;
        });
        while workers.try_join_next().is_some() {}
    }
    workers.shutdown().await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind: SocketAddr = std::env::var("CUSTOM_HOST_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8181".into())
        .parse()?;
    anyhow::ensure!(
        bind.ip().is_loopback(),
        "bind to loopback behind your own TLS proxy"
    );
    let handle = std::env::var("CUSTOM_HOST_SESSION")?;
    let browser = std::env::var("CUSTOM_HOST_BROWSER_TOKEN")?;
    let connector = std::env::var("CUSTOM_HOST_CONNECTOR_TOKEN")?;
    let app = Arc::new(App::new(handle, browser, connector)?);
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    println!("browser: http://{addr}/app/");
    println!("connector: ws://{addr}/connector");
    let (stop, stopped) = watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = stop.send(true);
    });
    serve(listener, app, stopped).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clank_web::bridge::{Action, ActionResult, Backend, Snapshot, Update};
    use clank_web::service::BrowserFrames;
    use futures_util::StreamExt;
    use tokio::sync::broadcast;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    struct Repo(broadcast::Sender<Update>);

    #[async_trait]
    impl Backend for Repo {
        async fn subscribe_with_snapshot(&self) -> (Snapshot, broadcast::Receiver<Update>) {
            (
                Snapshot {
                    sequence: 0,
                    state: serde_json::to_value(BrowserFrames {
                        replay: vec![clank_web::service::BrowserFrame::new(
                            "status",
                            serde_json::json!({"phase":"ready"}),
                        )],
                        live: vec![],
                    })
                    .unwrap(),
                },
                self.0.subscribe(),
            )
        }

        async fn dispatch(&self, _: Action) -> ActionResult {
            ActionResult::Refused {
                why: "fixture has no actions".into(),
            }
        }
    }

    #[tokio::test]
    async fn own_admission_is_separate_and_logout_revokes_the_browser_stream() {
        let app = Arc::new(
            App::new("team".into(), "browser-key".into(), "connector-key".into()).unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        let (stop, stopped) = watch::channel(false);
        let server = tokio::spawn(serve(listener, app.clone(), stopped));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let no_cookie = client.get(format!("{base}/app/")).send().await.unwrap();
        assert_eq!(no_cookie.status(), StatusCode::SEE_OTHER);
        assert_eq!(no_cookie.headers()[LOCATION], "/auth/login");
        let wrong_login = client
            .post(format!("{base}/auth/login"))
            .header(ORIGIN, &base)
            .header("content-type", "application/x-www-form-urlencoded")
            .body("token=connector-key")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong_login.status(), StatusCode::FORBIDDEN);
        let cross_origin = client
            .post(format!("{base}/auth/login"))
            .header(ORIGIN, "https://other.example")
            .header("content-type", "application/x-www-form-urlencoded")
            .body("token=browser-key")
            .send()
            .await
            .unwrap();
        assert_eq!(cross_origin.status(), StatusCode::FORBIDDEN);

        let (updates, _) = broadcast::channel(8);
        let endpoint = format!("ws://{addr}/connector");
        assert!(
            clank_web::bridge::connect(
                &endpoint,
                "other-team",
                "connector-key",
                Arc::new(Repo(updates.clone()))
            )
            .await
            .is_err()
        );
        assert!(
            clank_web::bridge::connect(
                &endpoint,
                "team",
                "browser-key",
                Arc::new(Repo(updates.clone()))
            )
            .await
            .is_err()
        );
        let connector = tokio::spawn(async move {
            clank_web::bridge::connect(&endpoint, "team", "connector-key", Arc::new(Repo(updates)))
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !app.session.connected().await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let login = client
            .post(format!("{base}/auth/login"))
            .header(ORIGIN, &base)
            .header("content-type", "application/x-www-form-urlencoded")
            .body("token=browser-key")
            .send()
            .await
            .unwrap();
        assert_eq!(login.status(), StatusCode::SEE_OTHER);
        let cookie = login.headers()[SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let secure_login = client
            .post(format!("{base}/auth/login"))
            .header(ORIGIN, format!("https://{addr}"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body("token=browser-key")
            .send()
            .await
            .unwrap();
        assert!(
            secure_login.headers()[SET_COOKIE]
                .to_str()
                .unwrap()
                .contains("; Secure")
        );
        let page = client
            .get(format!("{base}/app/"))
            .header(COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let html = page.text().await.unwrap();
        assert!(html.contains("<title>clank</title>"));
        assert!(!html.contains("/*__CLANK_DOOR__*/"));

        let mut browser_request = format!("ws://{addr}/app/events")
            .into_client_request()
            .unwrap();
        browser_request
            .headers_mut()
            .insert(COOKIE, cookie.parse().unwrap());
        browser_request
            .headers_mut()
            .insert(ORIGIN, base.parse().unwrap());
        let (mut browser, _) = tokio_tungstenite::connect_async(browser_request)
            .await
            .unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(3), browser.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(first.to_text().unwrap().contains("ready"));

        let logout = client
            .post(format!("{base}/auth/logout"))
            .header(ORIGIN, &base)
            .header(COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(logout.status(), StatusCode::SEE_OTHER);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let message = browser.next().await;
                if matches!(&message, Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text))) if text.starts_with("event: connection\n")) {
                    continue;
                }
                break message;
            }
        }).await.unwrap();
        assert!(!matches!(
            closed,
            Some(Ok(tokio_tungstenite::tungstenite::Message::Text(_)))
        ));
        let revoked = client
            .get(format!("{base}/app/bridge/status"))
            .header(COOKIE, &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);
        assert!(
            app.session.connected().await,
            "browser logout does not detach the repo"
        );
        let _ = stop.send(true);
        server.await.unwrap();
        connector.abort();
        let _ = connector.await;
    }
}
