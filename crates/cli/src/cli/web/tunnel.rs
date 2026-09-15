//! The tunnel: a fixed public hostname forwarded to the remote's
//! port, by a provider chosen in the user config. Up means proven —
//! the TUI reaches its own nonce and its own event stream through
//! the public URL before the URL is shown — and one holder per
//! hostname, by a lease taken before the provider starts
//! (a-tunnel-is-a-switch-too).

use std::path::{Path, PathBuf};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

/// `~/.clank/config.json#/remote/tunnel`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "provider", rename_all = "kebab-case")]
pub enum TunnelSection {
    /// ngrok through its SDK: nothing installed. The authtoken is
    /// the agent's own — `NGROK_AUTHTOKEN`, or the agent's config
    /// file — never clank's config.
    Ngrok { domain: String },
    /// A binary that forwards `url` to the port: `{port}` in `run`
    /// is the port.
    Command { run: Vec<String>, url: String },
}

/// How long a start has to come up: the provider's connect, then
/// the probe through the public URL, together.
pub(crate) const TUNNEL_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a stop waits for the provider to end before letting go
/// of it: the SDK's close, a child's exit.
const STOP_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// A tunnel provider: a fixed URL, and a start that yields the handle
/// the remote owns.
pub(crate) trait Provider: Send + Sync + 'static {
    fn url(&self) -> String;
    fn start(&self, port: u16) -> BoxFuture<'_, anyhow::Result<Box<dyn Handle>>>;
}

/// A running tunnel, ended by `stop` and nothing else. Stop answers
/// the provider's last words, if it has any — a child's stderr, read
/// to its end — for the reason when the tunnel did not come up.
pub(crate) trait Handle: Send + 'static {
    fn stop(self: Box<Self>) -> BoxFuture<'static, Option<String>>;
}

/// The public endpoint an URL names, provider-independent, for the
/// lease: host and port, lowercased, the path and a trailing slash
/// ignored — `https://clank.example.com/` and ngrok's
/// `clank.example.com` are one endpoint (codex on 6f60efa).
pub(crate) fn endpoint_identity(url: &str) -> anyhow::Result<String> {
    let parsed = url::Url::parse(url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("the tunnel's URL `{url}` has no host"))?
        .to_ascii_lowercase();
    Ok(match parsed.port() {
        Some(p) => format!("{host}:{p}"),
        None => host,
    })
}

/// The provider the user config names, if any, with the grace a
/// start gets.
pub(crate) fn configured(
    home: Option<&Path>,
) -> anyhow::Result<Option<(std::sync::Arc<dyn Provider>, std::time::Duration)>> {
    let Some(home) = home else { return Ok(None) };
    let cfg = crate::cli::team::read_user_config(home)?;
    let Some(section) = cfg.remote.and_then(|r| r.tunnel) else {
        return Ok(None);
    };
    let provider: std::sync::Arc<dyn Provider> = match section {
        TunnelSection::Ngrok { domain } => std::sync::Arc::new(Ngrok {
            authtoken: ngrok_authtoken(home).ok_or_else(|| {
                anyhow::anyhow!(
                    "ngrok: no authtoken — set NGROK_AUTHTOKEN, or `ngrok config add-authtoken …` \
                     so the agent's config file has it"
                )
            })?,
            domain,
        }),
        TunnelSection::Command { run, url } => std::sync::Arc::new(Command { run, url }),
    };
    Ok(Some((provider, TUNNEL_GRACE)))
}

/// The ngrok agent's authtoken, from the environment or the agent's
/// own config file: the credential stays in ngrok's files.
pub(crate) fn ngrok_authtoken(home: &Path) -> Option<String> {
    if let Ok(t) = std::env::var("NGROK_AUTHTOKEN")
        && !t.trim().is_empty()
    {
        return Some(t.trim().to_string());
    }
    [
        home.join("Library/Application Support/ngrok/ngrok.yml"),
        home.join(".config/ngrok/ngrok.yml"),
        home.join(".ngrok2/ngrok.yml"),
    ]
    .iter()
    .filter_map(|p| std::fs::read_to_string(p).ok())
    .find_map(|yml| authtoken_in(&yml))
}

fn authtoken_in(yml: &str) -> Option<String> {
    yml.lines().find_map(|line| {
        let value = line.trim().strip_prefix("authtoken:")?.trim();
        let value = value.trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_string())
    })
}

// ---- ngrok ----

pub(crate) struct Ngrok {
    domain: String,
    authtoken: String,
}

/// The ngrok tunnel runs on a runtime of its own so a stop OWNS the
/// whole connector: the SDK detaches a task per accepted connection
/// (`forward_tunnel` discards each `forward_to` handle) and spawns an
/// unlisten task on drop that keeps a `Session` clone, so aborting
/// the accept loop alone would leave connection and control tasks —
/// and the transport — alive past the lease's release (codex on
/// 22eed48). Dropping the isolated runtime cancels every one of them.
struct NgrokHandle {
    isolated: Isolated,
}

impl Provider for Ngrok {
    fn url(&self) -> String {
        format!("https://{}", self.domain)
    }
    fn start(&self, port: u16) -> BoxFuture<'_, anyhow::Result<Box<dyn Handle>>> {
        let (domain, authtoken) = (self.domain.clone(), self.authtoken.clone());
        Box::pin(async move {
            let build = move || -> BoxFuture<'static, anyhow::Result<((), Close)>> {
                Box::pin(async move {
                    use ngrok::config::ForwarderBuilder;
                    let session = ngrok::Session::builder()
                        .authtoken(&authtoken)
                        .metadata("clank remote")
                        .connect()
                        .await
                        .map_err(|e| anyhow::anyhow!("ngrok: {e}"))?;
                    let to = url::Url::parse(&format!("http://localhost:{port}"))?;
                    let forwarder = session
                        .http_endpoint()
                        .domain(&domain)
                        .listen_and_forward(to)
                        .await
                        .map_err(|e| anyhow::anyhow!("ngrok: {e}"))?;
                    // The graceful close, run on the isolated runtime
                    // before it is dropped: the unlisten RPC, then the
                    // session. Whatever it does not finish, the drop
                    // finishes by force.
                    let close: Close = Box::new(move || {
                        Box::pin(async move {
                            use ngrok::tunnel::TunnelCloser;
                            let mut forwarder = forwarder;
                            let mut session = session;
                            let _ = forwarder.close().await;
                            let _ = session.close().await;
                        })
                    });
                    Ok(((), close))
                })
            };
            let ((), isolated) = Isolated::start(build).await?;
            Ok(Box::new(NgrokHandle { isolated }) as Box<dyn Handle>)
        })
    }
}

impl Handle for NgrokHandle {
    fn stop(self: Box<Self>) -> BoxFuture<'static, Option<String>> {
        Box::pin(async move {
            self.isolated.shutdown().await;
            None
        })
    }
}

/// How long the graceful close gets on the isolated runtime before it
/// is dropped out from under whatever has not finished.
const CLOSE_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

/// A graceful close, run on the isolated runtime: consumes the SDK
/// handles it captured.
type Close = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// A native connector on a runtime it owns, on a thread of its own.
/// Everything the connector spawns lives on that runtime; `shutdown`
/// asks the graceful close for a bound and then drops the runtime,
/// which cancels every task the connector left — the guarantee a
/// handle drop cannot give (codex on 22eed48).
struct Isolated {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Isolated {
    /// Build the connector on a runtime it owns and hand its public
    /// value back, holding the handle across readiness so an
    /// abandoned start — the future dropped before readiness — tears
    /// the runtime down through `Drop`, not leaves it running
    /// (codex on c776ad1). The thread races the build against
    /// cancellation, so a stalled connect is abandoned too.
    async fn start<T: Send + 'static>(
        build: impl FnOnce() -> BoxFuture<'static, anyhow::Result<(T, Close)>> + Send + 'static,
    ) -> anyhow::Result<(T, Isolated)> {
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<T>>();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow::anyhow!("tunnel runtime: {e}")));
                    return;
                }
            };
            rt.block_on(async move {
                // The signal fires on an explicit stop or on the
                // sender being dropped (an abandoned start): either
                // ends the build in flight and the running phase.
                let mut cancel = shutdown_rx;
                tokio::select! {
                    biased;
                    _ = &mut cancel => {}
                    built = build() => match built {
                        Ok((public, close)) => {
                            if ready_tx.send(Ok(public)).is_ok() {
                                let _ = cancel.await;
                                let _ = tokio::time::timeout(CLOSE_BOUND, close()).await;
                            }
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                        }
                    }
                }
            });
            // Dropping the runtime here cancels every task the
            // connector spawned, in the build phase or after.
        });
        let isolated = Isolated {
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        };
        match ready_rx.await {
            Ok(Ok(public)) => Ok((public, isolated)),
            Ok(Err(e)) => Err(e),
            Err(_) => anyhow::bail!("the tunnel runtime ended before it came up"),
        }
    }

    /// Signal the graceful close, then wait for the thread — and so
    /// the runtime's drop — off the loop, within a bound.
    async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = tokio::time::timeout(
                STOP_BOUND,
                tokio::task::spawn_blocking(move || {
                    let _ = thread.join();
                }),
            )
            .await;
        }
    }
}

impl Drop for Isolated {
    /// The abandoned-start path: the start future was dropped before
    /// `shutdown` could run. Signal the thread and JOIN it here, so
    /// the runtime is gone before the caller (Tunnel::up) releases
    /// the lease. Bounded: the thread self-terminates on the signal
    /// within `CLOSE_BOUND`, then drops its runtime. `shutdown` takes
    /// both fields, so this is a no-op after the graceful path.
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

// ---- command ----

pub(crate) struct Command {
    run: Vec<String>,
    url: String,
}

struct CommandHandle {
    child: tokio::process::Child,
    /// The last of the child's stderr, kept for the reason when the
    /// tunnel does not come up; the reader ends at the pipe's EOF,
    /// and stop waits for it — for a bound — so the reason is whole.
    stderr: std::sync::Arc<std::sync::Mutex<String>>,
    reader: Option<tokio::task::JoinHandle<()>>,
}

impl Provider for Command {
    fn url(&self) -> String {
        self.url.trim_end_matches('/').to_string()
    }
    fn start(&self, port: u16) -> BoxFuture<'_, anyhow::Result<Box<dyn Handle>>> {
        Box::pin(async move {
            let argv: Vec<String> = self
                .run
                .iter()
                .map(|a| a.replace("{port}", &port.to_string()))
                .collect();
            let Some((program, args)) = argv.split_first() else {
                anyhow::bail!("tunnel command: `run` is empty");
            };
            // Its own process group: a wrapper's forwarding child is
            // the tunnel too, and stop ends the whole group, not the
            // one process that was spawned (codex on 6f60efa).
            let mut child = tokio::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .process_group(0)
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| anyhow::anyhow!("tunnel command `{program}`: {e}"))?;
            let stderr = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let reader = child.stderr.take().map(|pipe| {
                let tail = stderr.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt;
                    let mut lines = tokio::io::BufReader::new(pipe).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
                        t.push_str(&line);
                        t.push('\n');
                        if t.len() > 4096 {
                            let cut = t.len() - 4096;
                            let at = t
                                .char_indices()
                                .map(|(i, _)| i)
                                .find(|i| *i >= cut)
                                .unwrap_or(0);
                            t.drain(..at);
                        }
                    }
                })
            });
            Ok(Box::new(CommandHandle {
                child,
                stderr,
                reader,
            }) as Box<dyn Handle>)
        })
    }
}

/// Whether any process of the group is left, zombies included.
fn group_alive(group: i32) -> bool {
    // SAFETY: signal 0 checks existence and sends nothing.
    unsafe { libc::killpg(group, 0) == 0 }
}

fn signal_group(group: i32, signal: i32) {
    // SAFETY: signalling a process group this process created; no
    // memory effects.
    unsafe { libc::killpg(group, signal) };
}

impl Handle for CommandHandle {
    fn stop(self: Box<Self>) -> BoxFuture<'static, Option<String>> {
        Box::pin(async move {
            let mut this = *self;
            if let Some(pid) = this.child.id() {
                let group = pid as i32;
                signal_group(group, libc::SIGTERM);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(1), this.child.wait())
                    .await;
                // The leader's exit says nothing of the group: a
                // forwarding child that ignores TERM is still the
                // tunnel, and is ended by name of the group
                // (codex on f248156).
                if group_alive(group) {
                    signal_group(group, libc::SIGKILL);
                    let _ = this.child.wait().await;
                    let gone = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
                    while group_alive(group) && tokio::time::Instant::now() < gone {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                }
            }
            let _ = this.child.wait().await;
            // The reader ends at the pipe's EOF; one held open past
            // the bound by a process that left the group is aborted
            // and joined, not left behind.
            if let Some(mut r) = this.reader.take()
                && tokio::time::timeout(std::time::Duration::from_secs(1), &mut r)
                    .await
                    .is_err()
            {
                r.abort();
                let _ = r.await;
            }
            let tail = this.stderr.lock().unwrap_or_else(|e| e.into_inner());
            let tail = tail.trim();
            (!tail.is_empty()).then(|| tail.to_string())
        })
    }
}

// ---- the lease ----

/// One holder per endpoint, user-wide: `~/.clank/remote/<identity>.lock`,
/// flocked, the holder recorded for the refusal. Released on drop.
pub(crate) struct Lease {
    _file: std::fs::File,
}

fn lease_path(home: &Path, identity: &str) -> PathBuf {
    let name: String = identity
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    home.join(".clank/remote").join(format!("{name}.lock"))
}

impl Lease {
    /// Claim `identity`; refused with the holder's description when
    /// another process has it. A refusal is tried again for a moment
    /// first: a child this process is forking at that instant holds
    /// every open file until it execs, this one included.
    pub(crate) fn take(home: &Path, identity: &str) -> anyhow::Result<Self> {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let path = lease_path(home, identity);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| anyhow::anyhow!("opening `{}`: {e}", path.display()))?;
        let mut tries = 0;
        loop {
            // SAFETY: valid owned fd; flock has no memory effects.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                break;
            }
            tries += 1;
            if tries >= 10 {
                let holder = std::fs::read(&path)
                    .ok()
                    .and_then(|b| {
                        serde_json::from_slice::<crate::cli::status_tui::lease::Holder>(&b).ok()
                    })
                    .map(|h| h.describe())
                    .unwrap_or_else(|| "another process".to_string());
                anyhow::bail!("tunnel `{identity}` is held by {holder}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = file.set_len(0);
        if let Ok(body) = serde_json::to_vec(&crate::cli::status_tui::lease::Holder::here()) {
            let _ = file.write_all(&body);
            let _ = file.flush();
        }
        Ok(Self { _file: file })
    }
}

// ---- readiness ----

/// Why the public URL is not this remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NotUp {
    /// Nothing answered `/instance` within the grace.
    NoAnswer(String),
    /// `/instance` answered with another remote's nonce.
    AnotherInstance,
    /// The route answers but the event stream does not arrive: the
    /// tunnel buffers streams, and the page lives on one.
    NoStream(String),
}

impl std::fmt::Display for NotUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NotUp::NoAnswer(why) => write!(f, "not reachable through the tunnel: {why}"),
            NotUp::AnotherInstance => write!(f, "another clank remote answers at that URL"),
            NotUp::NoStream(why) => write!(
                f,
                "the tunnel does not carry event streams, which the page lives on: {why}"
            ),
        }
    }
}

/// Prove the public URL is this remote, and live: `GET /instance`
/// answers `nonce` (retried until the deadline), then
/// `/instance/stream` delivers its first event before it.
pub(crate) async fn probe(
    url: &str,
    nonce: &str,
    deadline: tokio::time::Instant,
) -> Result<(), NotUp> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| NotUp::NoAnswer(e.to_string()))?;
    loop {
        let last = match client.get(format!("{url}/instance")).send().await {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(v) if v["nonce"] == nonce => break,
                Ok(_) => return Err(NotUp::AnotherInstance),
                Err(e) => e.to_string(),
            },
            Ok(r) => format!("HTTP {}", r.status()),
            Err(e) => e.to_string(),
        };
        if tokio::time::Instant::now() + std::time::Duration::from_secs(1) >= deadline {
            return Err(NotUp::NoAnswer(last));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let stream = async {
        let mut resp = client
            .get(format!("{url}/instance/stream"))
            .timeout(std::time::Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let mut got = String::new();
        while !got.contains(nonce) {
            match resp.chunk().await {
                Ok(Some(c)) => got.push_str(&String::from_utf8_lossy(&c)),
                Ok(None) => return Err("the stream ended before its first event".to_string()),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    };
    match tokio::time::timeout_at(deadline, stream).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(why)) => Err(NotUp::NoStream(why)),
        Err(_) => Err(NotUp::NoStream("no event within the grace".to_string())),
    }
}

/// What a start brings up, if the user config names a tunnel: the
/// provider, the grace the whole start gets, and the switch that
/// aborts it — the TUI quitting, or the remote switched off, while
/// it is still coming up.
pub(crate) struct Start {
    pub(crate) provider: std::sync::Arc<dyn Provider>,
    pub(crate) grace: std::time::Duration,
    pub(crate) abort: tokio::sync::watch::Receiver<bool>,
}

/// A tunnel the remote owns: leased, started, proven. `stop` ends
/// it and releases the endpoint.
pub(crate) struct Tunnel {
    pub(crate) url: String,
    handle: Option<Box<dyn Handle>>,
    _lease: Lease,
}

impl Tunnel {
    /// Lease the endpoint, start the provider, prove the URL — all
    /// within the grace, and all abandoned on the abort: a failure at
    /// any step leaves nothing running and says why.
    pub(crate) async fn up(
        start: &mut Start,
        home: &Path,
        port: u16,
        nonce: &str,
    ) -> anyhow::Result<Self> {
        let deadline = tokio::time::Instant::now() + start.grace;
        let url = start.provider.url();
        let lease = Lease::take(home, &endpoint_identity(&url)?)?;
        let handle = tokio::select! {
            _ = abandoned(&mut start.abort) => anyhow::bail!("the start was abandoned"),
            started = tokio::time::timeout_at(deadline, start.provider.start(port)) => match started {
                Ok(Ok(handle)) => handle,
                Ok(Err(e)) => return Err(e),
                Err(_) => anyhow::bail!(
                    "the tunnel did not come up within {}s: still connecting",
                    start.grace.as_secs()
                ),
            },
        };
        let proven = tokio::select! {
            _ = abandoned(&mut start.abort) => Err(anyhow::anyhow!("the start was abandoned")),
            probed = probe(&url, nonce, deadline) => probed.map_err(|e| anyhow::anyhow!("{e}")),
        };
        if let Err(why) = proven {
            return Err(match end(handle).await {
                Some(said) => anyhow::anyhow!("{why}\n{said}"),
                None => why,
            });
        }
        Ok(Self {
            url,
            handle: Some(handle),
            _lease: lease,
        })
    }

    pub(crate) async fn stop(mut self) {
        if let Some(h) = self.handle.take() {
            let _ = end(h).await;
        }
    }
}

/// Resolves when the abort is thrown; a sender that is simply gone
/// is nobody throwing it, and never resolves.
async fn abandoned(abort: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if abort.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        if *abort.borrow() {
            return;
        }
    }
}

/// End a handle, for a bound: past it the handle is dropped, which
/// kills a child and closes an SDK session without waiting.
async fn end(handle: Box<dyn Handle>) -> Option<String> {
    tokio::time::timeout(STOP_BOUND, handle.stop())
        .await
        .unwrap_or(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn err_of<T>(r: anyhow::Result<T>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => format!("{e:#}"),
        }
    }

    /// A public endpoint as a tunnel would present this remote: it
    /// answers `/instance` with `nonce`, and `/instance/stream` with
    /// the event (`streams`), with nothing ever (`hangs`), or not at
    /// all.
    async fn fake_public(
        nonce: &'static str,
        streams: bool,
        hangs: bool,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let io = hyper_util::rt::TokioIo::new(stream);
                let svc = hyper::service::service_fn(
                    move |req: hyper::Request<hyper::body::Incoming>| async move {
                        use http_body_util::{BodyExt, Full};
                        let reply = |status: u16, body: String, ctype: &str| {
                            hyper::Response::builder()
                                .status(status)
                                .header("content-type", ctype)
                                .body(Full::new(hyper::body::Bytes::from(body)).boxed())
                                .unwrap()
                        };
                        let resp = match req.uri().path() {
                            "/instance" => reply(
                                200,
                                serde_json::json!({ "nonce": nonce }).to_string(),
                                "application/json",
                            ),
                            "/instance/stream" if hangs => {
                                let (tx, rx) = tokio::sync::mpsc::channel::<String>(1);
                                std::mem::forget(tx);
                                hyper::Response::builder()
                                    .status(200)
                                    .header("content-type", "text/event-stream")
                                    .body(
                                        super::super::SseBody {
                                            rx,
                                            ended: std::sync::Arc::new(
                                                std::sync::atomic::AtomicBool::new(false),
                                            ),
                                        }
                                        .boxed(),
                                    )
                                    .unwrap()
                            }
                            "/instance/stream" if streams => reply(
                                200,
                                format!("event: instance\ndata: {nonce}\n\n"),
                                "text/event-stream",
                            ),
                            _ => reply(404, "no".to_string(), "text/plain"),
                        };
                        Ok::<_, std::convert::Infallible>(resp)
                    },
                );
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, svc)
                        .await;
                });
            }
        });
        (url, task)
    }

    /// Up is the nonce read back AND the first event of the stream
    /// delivered: another nonce is another remote; a route without
    /// a stream, or a stream that never delivers, is a tunnel that
    /// buffers; nothing answering is nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_probe_proves_the_nonce_and_the_stream() {
        let grace = || tokio::time::Instant::now() + Duration::from_secs(3);
        let (url, _srv) = fake_public("n1", true, false).await;
        assert_eq!(probe(&url, "n1", grace()).await, Ok(()));
        assert_eq!(
            probe(&url, "n2", grace()).await,
            Err(NotUp::AnotherInstance),
            "another remote's nonce"
        );
        let (url, _srv) = fake_public("n1", false, false).await;
        assert!(
            matches!(probe(&url, "n1", grace()).await, Err(NotUp::NoStream(why)) if why.contains("404")),
            "a route without a stream"
        );
        let (url, _srv) = fake_public("n1", false, true).await;
        let started = std::time::Instant::now();
        assert!(
            matches!(probe(&url, "n1", grace()).await, Err(NotUp::NoStream(why)) if why.contains("grace")),
            "a stream that never delivers"
        );
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "bounded by the grace"
        );
        let dead = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let url = format!("http://127.0.0.1:{}", dead.local_addr().unwrap().port());
        drop(dead);
        let started = std::time::Instant::now();
        assert!(matches!(
            probe(&url, "n1", grace()).await,
            Err(NotUp::NoAnswer(_))
        ));
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "bounded by the grace"
        );
    }

    /// One endpoint, however it is written or which provider names
    /// it: the lease is keyed by host and port, not the spelling.
    #[test]
    fn the_endpoint_identity_is_the_host_whoever_names_it() {
        let ngrok = Ngrok {
            domain: "Clank.Example.com".into(),
            authtoken: "t".into(),
        };
        let command = Command {
            run: vec![],
            url: "https://clank.example.com/".into(),
        };
        let id = endpoint_identity(&ngrok.url()).unwrap();
        assert_eq!(id, "clank.example.com");
        assert_eq!(endpoint_identity(&command.url()).unwrap(), id);
        assert_eq!(
            endpoint_identity("https://clank.example.com/some/path").unwrap(),
            id
        );
        assert_eq!(
            endpoint_identity("https://clank.example.com:8443").unwrap(),
            "clank.example.com:8443",
            "a port is part of the endpoint"
        );
        assert!(endpoint_identity("not a url").is_err());
        let home = tempfile::tempdir().unwrap();
        let held = Lease::take(home.path(), &id).unwrap();
        assert!(Lease::take(home.path(), &endpoint_identity(&command.url()).unwrap()).is_err());
        drop(held);
    }

    /// One holder per endpoint, the holder named to the next.
    #[test]
    fn the_lease_names_its_holder() {
        let home = tempfile::tempdir().unwrap();
        let held = Lease::take(home.path(), "clank.example.com").unwrap();
        let why = err_of(Lease::take(home.path(), "clank.example.com"));
        assert!(why.contains("held by"), "{why}");
        assert!(
            why.contains(&format!("pid {}", std::process::id())),
            "{why}"
        );
        assert!(Lease::take(home.path(), "other.example.com").is_ok());
        drop(held);
        assert!(Lease::take(home.path(), "clank.example.com").is_ok());
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 checks existence and sends nothing.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn pid_from(file: &Path) -> i32 {
        for _ in 0..50 {
            if let Ok(s) = std::fs::read_to_string(file)
                && let Ok(pid) = s.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the child never wrote its pid");
    }

    fn start(provider: Command, grace: u64) -> Start {
        Start {
            provider: std::sync::Arc::new(provider),
            grace: Duration::from_secs(grace),
            abort: tokio::sync::watch::channel(false).1,
        }
    }

    /// The command provider's child lives as long as the tunnel and
    /// no longer — the whole process group, so a wrapper's forwarding
    /// child goes with it; when the tunnel does not come up, its
    /// stderr is the reason and the child is gone.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_command_provider_ends_with_the_tunnel_and_says_why() {
        let home = tempfile::tempdir().unwrap();
        let pidfile = home.path().join("pid");
        let run = |say: &str| {
            vec![
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "echo '{say}' >&2; echo $$ > '{}'; exec sleep 100",
                    pidfile.display()
                ),
            ]
        };
        let (url, _srv) = fake_public("n1", true, false).await;
        let mut up = start(
            Command {
                run: run("forwarding {port}"),
                url: url.clone(),
            },
            5,
        );
        let tunnel = Tunnel::up(&mut up, home.path(), 4242, "n1").await.unwrap();
        assert_eq!(tunnel.url, url);
        let pid = pid_from(&pidfile).await;
        assert!(alive(pid));
        tunnel.stop().await;
        assert!(!alive(pid), "the child ends with the tunnel");
        assert!(
            Lease::take(home.path(), &endpoint_identity(&url).unwrap()).is_ok(),
            "the endpoint is released"
        );

        // A wrapper that does not exec: the forwarding child is a
        // grandchild holding the stderr pipe, and stop ends it too —
        // the group, not the one process spawned (codex on 6f60efa).
        std::fs::remove_file(&pidfile).unwrap();
        let grandchild = home.path().join("pid2");
        let mut up = start(
            Command {
                run: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "echo $$ > '{}'; sleep 100 & echo $! > '{}'; wait",
                        pidfile.display(),
                        grandchild.display()
                    ),
                ],
                url: url.clone(),
            },
            5,
        );
        let tunnel = Tunnel::up(&mut up, home.path(), 4242, "n1").await.unwrap();
        let (pid, pid2) = (pid_from(&pidfile).await, pid_from(&grandchild).await);
        assert!(alive(pid) && alive(pid2));
        let stopping = std::time::Instant::now();
        tunnel.stop().await;
        assert!(
            stopping.elapsed() < Duration::from_secs(4),
            "stop is bounded"
        );
        assert!(!alive(pid), "the wrapper is gone");
        assert!(!alive(pid2), "its forwarding child is gone with it");

        // A wrapper whose forwarding child ignores TERM: the wrapper
        // leaves on TERM, the child does not, and stop ends the
        // group by name rather than taking the leader's exit for
        // the tunnel's (codex on f248156).
        std::fs::remove_file(&pidfile).unwrap();
        std::fs::remove_file(&grandchild).unwrap();
        let mut up = start(
            Command {
                run: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "echo $$ > '{}'; sh -c 'trap \"\" TERM; echo $$ > \"{}\"; exec sleep 100' & wait",
                        pidfile.display(),
                        grandchild.display()
                    ),
                ],
                url: url.clone(),
            },
            5,
        );
        let tunnel = Tunnel::up(&mut up, home.path(), 4242, "n1").await.unwrap();
        let (pid, pid2) = (pid_from(&pidfile).await, pid_from(&grandchild).await);
        assert!(alive(pid) && alive(pid2));
        let stopping = std::time::Instant::now();
        tunnel.stop().await;
        assert!(
            stopping.elapsed() < Duration::from_secs(5),
            "stop is bounded"
        );
        assert!(!alive(pid), "the wrapper is gone");
        assert!(!alive(pid2), "the TERM-ignoring survivor is ended too");

        // A tunnel that routes but buffers the stream: the failure
        // takes the grace, and the child's stderr is the reason's
        // second line.
        std::fs::remove_file(&pidfile).unwrap();
        let (buffered, _srv2) = fake_public("n1", false, true).await;
        let mut up = start(
            Command {
                run: run("warming up"),
                url: buffered,
            },
            2,
        );
        let why = err_of(Tunnel::up(&mut up, home.path(), 4242, "n1").await);
        assert!(why.contains("does not carry event streams"), "{why}");
        assert!(why.contains("warming up"), "the child's stderr: {why}");
        let pid = pid_from(&pidfile).await;
        assert!(!alive(pid), "nothing left running");
    }

    /// Counts a live task: up while the guard is held, down when the
    /// task's future is dropped.
    struct Counter(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Counter {
        fn new(count: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Counter(count)
        }
    }
    impl Drop for Counter {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The graceful close stalls, and the connector has already
    /// spawned a detached per-connection task with a nested worker —
    /// exactly the SDK's shape. Nothing aborts those tasks by name;
    /// dropping the owned runtime ends them, and shutdown returns
    /// within its bounds, so the lease is released after the tunnel
    /// is truly gone, not after a handle is dropped (codex on
    /// 22eed48).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_native_close_still_ends_the_whole_connector() {
        let workers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let building = workers.clone();
        let build = move || -> BoxFuture<'static, anyhow::Result<((), Close)>> {
            let workers = building.clone();
            Box::pin(async move {
                // Detached connection tasks, each spawning a nested
                // worker it never joins — as forward_tunnel does.
                for _ in 0..2 {
                    let count = workers.clone();
                    tokio::spawn(async move {
                        let _guard = Counter::new(count.clone());
                        let inner = count.clone();
                        tokio::spawn(async move {
                            let _guard = Counter::new(inner);
                            std::future::pending::<()>().await;
                        });
                        std::future::pending::<()>().await;
                    });
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                let close: Close = Box::new(|| Box::pin(std::future::pending()));
                Ok(((), close))
            })
        };
        let ((), isolated) = Isolated::start(build).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            workers.load(std::sync::atomic::Ordering::Relaxed) >= 4,
            "the connector's tasks and their nested workers are live"
        );
        let started = std::time::Instant::now();
        isolated.shutdown().await;
        assert!(
            started.elapsed() < CLOSE_BOUND + STOP_BOUND + Duration::from_secs(1),
            "bounded: {:?}",
            started.elapsed()
        );
        assert_eq!(
            workers.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "every task, detached or nested, is gone with the runtime"
        );
    }

    /// A start abandoned mid-connect — the future dropped while the
    /// build is still pending, with a nested worker already spawned
    /// on the owned runtime — tears the runtime and every task down
    /// before returning, so a caller that releases a lease next does
    /// so with nothing left running (codex on c776ad1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_isolated_start_abandoned_mid_connect_tears_down() {
        let workers = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let building = workers.clone();
        let build = move || -> BoxFuture<'static, anyhow::Result<((), Close)>> {
            let workers = building.clone();
            Box::pin(async move {
                let count = workers.clone();
                tokio::spawn(async move {
                    let _guard = Counter::new(count.clone());
                    let inner = count.clone();
                    tokio::spawn(async move {
                        let _guard = Counter::new(inner);
                        std::future::pending::<()>().await;
                    });
                    std::future::pending::<()>().await;
                });
                tokio::time::sleep(Duration::from_millis(50)).await;
                // The connect never finishes.
                std::future::pending::<()>().await;
                unreachable!()
            })
        };
        let started = std::time::Instant::now();
        // Abandon: the timeout drops the pending `start` future.
        let r = tokio::time::timeout(Duration::from_millis(400), Isolated::start(build)).await;
        assert!(
            r.is_err(),
            "the start never completes; its future is dropped"
        );
        assert!(
            started.elapsed() < CLOSE_BOUND + STOP_BOUND + Duration::from_secs(1),
            "the drop's teardown is bounded: {:?}",
            started.elapsed()
        );
        assert_eq!(
            workers.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the runtime and its nested tasks are gone before the caller proceeds"
        );
    }

    /// A build that fails is surfaced, and its thread is joined.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_isolated_start_that_fails_says_why() {
        let build = || -> BoxFuture<'static, anyhow::Result<((), Close)>> {
            Box::pin(async { anyhow::bail!("no route to host") })
        };
        let why = err_of(Isolated::start(build).await);
        assert!(why.contains("no route to host"), "{why}");
    }

    /// The provider comes from the user config; ngrok's token from
    /// The provider comes from the user config; ngrok's token from
    /// the agent's own file, never clank's.
    #[test]
    fn the_config_names_the_provider_and_the_token_is_the_agents() {
        let section: TunnelSection =
            serde_json::from_str(r#"{"provider":"ngrok","domain":"clank.ngrok.app"}"#).unwrap();
        assert_eq!(
            section,
            TunnelSection::Ngrok {
                domain: "clank.ngrok.app".into()
            }
        );
        let section: TunnelSection = serde_json::from_str(
            r#"{"provider":"command","run":["cloudflared","--url","http://localhost:{port}"],"url":"https://c.example.com/"}"#,
        )
        .unwrap();
        assert!(matches!(section, TunnelSection::Command { .. }));

        assert_eq!(
            authtoken_in("version: 2\nauthtoken: \"abc\"\n").as_deref(),
            Some("abc")
        );
        assert_eq!(authtoken_in("version: 2\n"), None);

        let home = tempfile::tempdir().unwrap();
        assert!(configured(Some(home.path())).unwrap().is_none());
        let write = |tunnel: TunnelSection| {
            let mut cfg = crate::cli::team::read_user_config(home.path()).unwrap();
            cfg.remote.get_or_insert_with(Default::default).tunnel = Some(tunnel);
            crate::cli::team::write_user_config(home.path(), &cfg).unwrap();
        };
        write(TunnelSection::Command {
            run: vec!["true".into()],
            url: "https://c.example.com".into(),
        });
        let (p, grace) = configured(Some(home.path())).unwrap().unwrap();
        assert_eq!(p.url(), "https://c.example.com");
        assert_eq!(grace, TUNNEL_GRACE);

        write(TunnelSection::Ngrok {
            domain: "clank.ngrok.app".into(),
        });
        if std::env::var_os("NGROK_AUTHTOKEN").is_none() {
            let why = err_of(configured(Some(home.path())));
            assert!(why.contains("authtoken"), "{why}");
        }
        std::fs::create_dir_all(home.path().join(".config/ngrok")).unwrap();
        std::fs::write(
            home.path().join(".config/ngrok/ngrok.yml"),
            "version: \"3\"\nauthtoken: tok_123\n",
        )
        .unwrap();
        let (p, _) = configured(Some(home.path())).unwrap().unwrap();
        assert_eq!(p.url(), "https://clank.ngrok.app");
        let cfg = std::fs::read_to_string(home.path().join(".clank/config.json")).unwrap();
        assert!(
            !cfg.contains("tok_123"),
            "the token is not in clank's config"
        );
    }
}
