//! The authorized session every GitHub read goes through: one
//! credential, one client, and the deadlines that keep a hung request
//! from owning a source task forever.
//!
//! Moved here whole (`the-github-watcher-is-a-directory`). A test
//! that exercises the session TOGETHER with the feed's fetcher stays
//! with the fetcher — a test belongs to the seam it exercises, and
//! these three exercise this one alone.

/// The GitHub token, reusing gh's credential without shelling gh per
/// poll: `GH_TOKEN` / `GITHUB_TOKEN` env first (the standard
/// overrides gh itself honors), else ONE `gh auth token` subprocess —
/// the last place gh is invoked on the polling path
/// (github-http-client-and-realtime M1).
async fn acquire_github_token() -> anyhow::Result<String> {
    if let Some(t) = env_token(|k| std::env::var(k).ok()) {
        return Ok(t);
    }
    let out = tokio::process::Command::new("gh")
        .args(["auth", "token"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "no GH_TOKEN/GITHUB_TOKEN and `gh auth token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if token.is_empty() {
        anyhow::bail!("`gh auth token` returned nothing");
    }
    Ok(token)
}

/// Env-token precedence, injected for tests: GH_TOKEN beats
/// GITHUB_TOKEN, blanks don't count.
pub(super) fn env_token(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["GH_TOKEN", "GITHUB_TOKEN"]
        .iter()
        .filter_map(|k| get(k))
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
}

/// A cached token that can be invalidated (a 401 means it rotated) and
/// re-acquired on the next use — acquisition injected for tests.
struct TokenCell(tokio::sync::Mutex<Option<String>>);

impl TokenCell {
    fn new() -> Self {
        Self(tokio::sync::Mutex::new(None))
    }
    async fn invalidate(&self) {
        *self.0.lock().await = None;
    }
    async fn get_or_acquire<F, Fut>(&self, acquire: F) -> anyhow::Result<String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<String>>,
    {
        let mut slot = self.0.lock().await;
        if let Some(t) = slot.as_ref() {
            return Ok(t.clone());
        }
        let fresh = acquire().await?;
        *slot = Some(fresh.clone());
        Ok(fresh)
    }
}

/// A minimal response the session yields — our own shape so the
/// transport is injectable in tests without fabricating reqwest types.
#[derive(Debug)]
pub(crate) struct MiniResponse {
    pub(crate) status: u16,
    pub(crate) headers: std::collections::BTreeMap<String, String>,
    pub(crate) body: String,
}

impl MiniResponse {
    pub(super) fn header_u64(&self, name: &str) -> Option<u64> {
        self.headers.get(name)?.parse().ok()
    }
}

/// The raw HTTP GET, injected so the session's auth behavior is
/// testable without a network.
pub(crate) trait HttpSend: Send + Sync {
    fn send(
        &self,
        url: &str,
        bearer: &str,
        etag: Option<&str>,
    ) -> impl std::future::Future<Output = anyhow::Result<MiniResponse>> + Send;
}

/// Production sender: reqwest (rustls), typed statuses and headers —
/// the gh-CLI text parsing this transport replaced produced the
/// 304-noise/CRLF/substring-status bug class.
pub(crate) struct ReqwestSend(reqwest::Client);

impl HttpSend for ReqwestSend {
    async fn send(
        &self,
        url: &str,
        bearer: &str,
        etag: Option<&str>,
    ) -> anyhow::Result<MiniResponse> {
        let mut req = self
            .0
            .get(url)
            .bearer_auth(bearer)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(tag) = etag {
            req = req.header("If-None-Match", tag);
        }
        let resp = req.send().await?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                Some((
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().ok()?.to_string(),
                ))
            })
            .collect();
        let body = resp.text().await.unwrap_or_default();
        Ok(MiniResponse {
            status,
            headers,
            body,
        })
    }
}

/// A token acquirer: the boxed-future fn shape both production
/// (`acquire_github_token`) and the tests' scripted acquirers share.
type AcquireFn =
    fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>;

/// ONE host-scoped authenticated session shared by every github
/// source in the process (codex f9807c3: per-source cells shelled gh
/// once per source and let a rotated token gate /user forever). Every
/// endpoint goes through [`GithubSession::get`], which has the ONE
/// uniform 401 story: invalidate, re-acquire, retry ONCE; a second
/// 401 is the error.
pub(crate) struct GithubSession<S> {
    http: S,
    token: TokenCell,
    acquire: AcquireFn,
    /// Application-level deadlines at the OWNERSHIP boundary
    /// (github-watch-resilience): they bound ANY `HttpSend` and ANY
    /// acquire — not just the production reqwest/gh ones — so a
    /// wedged connection or hung subprocess becomes a tick error the
    /// poll loop retries, never a forever-await that silently kills
    /// one source while its siblings tick on.
    send_timeout: std::time::Duration,
    acquire_timeout: std::time::Duration,
}

/// Default deadline for one HTTP send. Ticks are 60s+; this can never
/// overlap the next tick.
pub(super) const SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Default deadline for one token acquisition (`gh auth token`).
pub(super) const ACQUIRE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The production client: transport-level belt UNDER the session's
/// application deadline (connect + total request bounds).
fn github_client_with(connect: std::time::Duration, total: std::time::Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("clank")
        .connect_timeout(connect)
        .timeout(total)
        .build()
        .expect("static client config")
}

/// The github.com session, created on first use (host-scoped: one per
/// process today; a hosts map when enterprise arrives).
pub(crate) fn shared_session() -> std::sync::Arc<GithubSession<ReqwestSend>> {
    static SESSION: std::sync::OnceLock<std::sync::Arc<GithubSession<ReqwestSend>>> =
        std::sync::OnceLock::new();
    SESSION
        .get_or_init(|| {
            std::sync::Arc::new(GithubSession {
                http: ReqwestSend(github_client_with(
                    std::time::Duration::from_secs(10),
                    SEND_TIMEOUT,
                )),
                token: TokenCell::new(),
                acquire: || Box::pin(acquire_github_token()),
                send_timeout: SEND_TIMEOUT,
                acquire_timeout: ACQUIRE_TIMEOUT,
            })
        })
        .clone()
}

#[cfg(test)]
type AcquireForTests =
    fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>;

impl<S: HttpSend> GithubSession<S> {
    // `pub(crate)` so the CI reader's tests script this same session
    // rather than opening a second, differently-behaved one.
    #[cfg(test)]
    pub(crate) fn for_tests(http: S, acquire: AcquireForTests) -> Self {
        Self {
            http,
            token: TokenCell::new(),
            acquire,
            send_timeout: SEND_TIMEOUT,
            acquire_timeout: ACQUIRE_TIMEOUT,
        }
    }

    /// Test knob for paused-clock deadline proofs.
    #[cfg(test)]
    pub(super) fn with_timeouts(
        mut self,
        send: std::time::Duration,
        acquire: std::time::Duration,
    ) -> Self {
        self.send_timeout = send;
        self.acquire_timeout = acquire;
        self
    }

    /// One acquisition under the deadline. Timing out DROPS the
    /// acquire future — the `gh auth token` child dies by its
    /// `kill_on_drop` config, never outliving the deadline.
    async fn token_within_deadline(&self) -> anyhow::Result<String> {
        match tokio::time::timeout(
            self.acquire_timeout,
            self.token.get_or_acquire(self.acquire),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => anyhow::bail!(
                "github token acquisition timed out after {}s",
                self.acquire_timeout.as_secs()
            ),
        }
    }

    /// One send under the deadline.
    async fn send_within_deadline(
        &self,
        url: &str,
        token: &str,
        etag: Option<&str>,
    ) -> anyhow::Result<MiniResponse> {
        match tokio::time::timeout(self.send_timeout, self.http.send(url, token, etag)).await {
            Ok(r) => r,
            Err(_) => anyhow::bail!(
                "github {url}: request timed out after {}s",
                self.send_timeout.as_secs()
            ),
        }
    }

    /// Authorized GET with the uniform 401 handling, every await
    /// bounded (github-watch-resilience). `pub(crate)` so the event
    /// page's content fetch shares this ONE authorized path rather
    /// than opening a second one (tui-github-event-content).
    pub(crate) async fn get(&self, url: &str, etag: Option<&str>) -> anyhow::Result<MiniResponse> {
        let token = self.token_within_deadline().await?;
        let resp = self.send_within_deadline(url, &token, etag).await?;
        if resp.status != 401 {
            return Ok(resp);
        }
        // The token rotated: re-acquire and retry exactly once.
        self.token.invalidate().await;
        let token = self.token_within_deadline().await?;
        let resp = self.send_within_deadline(url, &token, etag).await?;
        if resp.status == 401 {
            anyhow::bail!("github {url}: 401 after token re-acquisition");
        }
        Ok(resp)
    }
}

/// The authenticated login for the own-action filter — `GET /user`
/// over the SAME session and 401 story as the events endpoint.
pub(super) async fn fetch_authenticated_login<S: HttpSend>(
    session: &GithubSession<S>,
) -> anyhow::Result<String> {
    let resp = session.get("https://api.github.com/user", None).await?;
    if !(200..300).contains(&resp.status) {
        anyhow::bail!("github /user: {}", resp.status);
    }
    let v: serde_json::Value = serde_json::from_str(&resp.body)?;
    v.get("login")
        .and_then(|l| l.as_str())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("github /user returned no login"))
}

/// Resolves the authenticated login for the own-action filter —
/// injected so a login failure-then-success is testable.
pub(crate) trait LoginResolver {
    fn resolve(&self) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// Production resolver: `GET /user` over the shared session.
pub(super) struct GhLogin<S: HttpSend>(pub(crate) std::sync::Arc<GithubSession<S>>);

impl<S: HttpSend> LoginResolver for GhLogin<S> {
    async fn resolve(&self) -> anyhow::Result<String> {
        fetch_authenticated_login(&self.0).await
    }
}

/// Items per page. GitHub's maximum, so the common repo is one page.
pub(super) const PER_PAGE: usize = 100;

/// Pages a collection may span before the reader refuses it. At 100
/// an item that is 2000 checks or open PRs, past which the assumption
/// is wrong somewhere and guessing is worse than saying so.
pub(super) const MAX_COLLECTION_PAGES: u32 = 20;

/// Every element of a paginated collection, or an error — never a
/// prefix of one.
///
/// Completeness is the contract both readers are built on. The CI
/// reduction calls a PR green when it has seen no failure, so a page
/// left unread is a green light nobody earned; the draft reader treats
/// every remembered comment the snapshot omits as deleted, so half a
/// page is the operator appearing to throw the review away. A later page that fails fails the whole
/// read, a body whose collection is not where it should be is an
/// error rather than an empty list — silence and "nothing failed"
/// must not be the same answer — and a collection past [`MAX_COLLECTION_PAGES`]
/// is refused rather than truncated (codex 3707a88).
///
/// `field` names the array inside the body, or `None` when the body
/// IS the array.
pub(super) async fn read_every_page<S: HttpSend>(
    session: &GithubSession<S>,
    url_for_page: impl Fn(u32) -> String,
    field: Option<&str>,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let mut all: Vec<serde_json::Value> = Vec::new();
    for page in 1..=MAX_COLLECTION_PAGES {
        let url = url_for_page(page);
        let resp = session.get(&url, None).await?;
        if !(200..300).contains(&resp.status) {
            anyhow::bail!(
                "github {url}: {}: {}",
                resp.status,
                resp.body.chars().take(200).collect::<String>()
            );
        }
        let body: serde_json::Value = serde_json::from_str(&resp.body)
            .map_err(|e| anyhow::anyhow!("github {url}: body is not json: {e}"))?;
        let items = match field {
            None => body.as_array(),
            Some(name) => body.get(name).and_then(|v| v.as_array()),
        }
        .ok_or_else(|| {
            anyhow::anyhow!(
                "github {url}: no `{}` array in the response",
                field.unwrap_or("top-level")
            )
        })?;
        let len = items.len();
        all.extend(items.iter().cloned());
        if len < PER_PAGE {
            return Ok(all);
        }
    }
    anyhow::bail!(
        "github {}: more than {MAX_COLLECTION_PAGES} pages — refusing to report a partial collection",
        url_for_page(1),
    )
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    pub(in crate::cli::github) struct ScriptHttp {
        /// Scripted statuses per request, in order; exhausted → 200.
        statuses: std::sync::Mutex<std::collections::VecDeque<u16>>,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl ScriptHttp {
        pub(in crate::cli::github) fn new(statuses: &[u16]) -> Self {
            Self {
                statuses: std::sync::Mutex::new(statuses.iter().copied().collect()),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    impl HttpSend for ScriptHttp {
        async fn send(
            &self,
            url: &str,
            _bearer: &str,
            _etag: Option<&str>,
        ) -> anyhow::Result<MiniResponse> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let status = self.statuses.lock().unwrap().pop_front().unwrap_or(200);
            let body = if url.ends_with("/user") {
                r#"{"login":"me"}"#.to_string()
            } else {
                "[]".to_string()
            };
            Ok(MiniResponse {
                status,
                headers: Default::default(),
                body,
            })
        }
    }

    pub(in crate::cli::github) static ACQUIRES: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    pub(in crate::cli::github) fn counting_acquire()
    -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>> {
        ACQUIRES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok("tok".to_string()) })
    }

    #[tokio::test(start_paused = true)]
    async fn hung_acquire_fails_within_its_bound() {
        let session = GithubSession::for_tests(ScriptHttp::new(&[]), || {
            Box::pin(async {
                std::future::pending::<()>().await;
                unreachable!()
            })
        })
        .with_timeouts(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(15),
        );
        let start = tokio::time::Instant::now();
        let err = session
            .get("https://api.github.com/x", None)
            .await
            .expect_err("hung acquire must not hang the get");
        assert!(
            err.to_string().contains("token acquisition timed out"),
            "{err}"
        );
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_secs(15),
            "bounded at exactly the acquire deadline (paused clock)"
        );
    }

    /// Set-on-drop probe: the deadline must CANCEL (drop) the owned
    /// acquire future — production's `kill_on_drop` on the gh child
    /// rides exactly that drop, so the drop IS the invariant to pin
    /// (codex 78d6592: no external binaries, no unix-only kill).
    static ACQUIRE_DROPPED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    fn hanging_probed_acquire()
    -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>> {
        struct Probe;
        impl Drop for Probe {
            fn drop(&mut self) {
                ACQUIRE_DROPPED.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        Box::pin(async {
            let _probe = Probe;
            std::future::pending::<()>().await;
            unreachable!()
        })
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_acquire_drops_the_owned_future() {
        let session = GithubSession::for_tests(ScriptHttp::new(&[]), hanging_probed_acquire)
            .with_timeouts(
                std::time::Duration::from_secs(30),
                std::time::Duration::from_secs(15),
            );
        let err = session
            .get("https://api.github.com/x", None)
            .await
            .expect_err("hung acquire must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            ACQUIRE_DROPPED.load(std::sync::atomic::Ordering::SeqCst),
            "the deadline must drop the acquire future (kill_on_drop rides this)"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_client_config_bounds_a_tarpit() {
        // The transport belt: the SAME builder wiring production uses
        // (github_client_with) must error against a listener that
        // accepts and never responds — proven with short bounds so the
        // test runs in real time. The tarpit is an OWNED task, aborted
        // and awaited afterward so no thread, socket, or descriptor
        // outlives the test (codex 78d6592).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            // Accept one connection and hold it open, never respond.
            let _conn = listener.accept().await;
            std::future::pending::<()>().await
        });
        let client = github_client_with(
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(400),
        );
        let start = std::time::Instant::now();
        let err = client
            .get(format!("http://{addr}/events"))
            .send()
            .await
            .expect_err("tarpit must not hang the configured client");
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "{err}");
        hold.abort();
        let _ = hold.await;
    }

    #[test]
    fn env_token_precedence_gh_token_beats_github_token_blanks_skipped() {
        // github-http-client-and-realtime M1: env beats shelling gh,
        // GH_TOKEN beats GITHUB_TOKEN, and blanks don't count.
        let get = |m: &[(&str, &str)]| {
            let m: Vec<(String, String)> = m
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            move |k: &str| m.iter().find(|(mk, _)| mk == k).map(|(_, v)| v.clone())
        };
        assert_eq!(
            env_token(get(&[("GH_TOKEN", "a"), ("GITHUB_TOKEN", "b")])),
            Some("a".into())
        );
        assert_eq!(env_token(get(&[("GITHUB_TOKEN", "b")])), Some("b".into()));
        assert_eq!(env_token(get(&[("GH_TOKEN", "  ")])), None, "blank skipped");
        assert_eq!(env_token(get(&[])), None);
    }

    #[tokio::test]
    async fn token_cell_reacquires_only_after_invalidation() {
        // A 401 invalidates; the next use re-acquires — pinned with a
        // scripted acquirer, no network.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let acquire = || {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Ok(format!("t{}", calls.load(Ordering::SeqCst))) }
        };
        let cell = TokenCell::new();
        assert_eq!(cell.get_or_acquire(acquire).await.unwrap(), "t1");
        assert_eq!(cell.get_or_acquire(acquire).await.unwrap(), "t1", "cached");
        cell.invalidate().await;
        assert_eq!(
            cell.get_or_acquire(acquire).await.unwrap(),
            "t2",
            "re-acquired"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A scripted login resolver: a queue of results (Err then Ok) so
    /// a failure-then-success is testable.
    pub(in crate::cli::github) struct ScriptLogin(
        pub(in crate::cli::github) Mutex<VecDeque<anyhow::Result<String>>>,
    );

    impl LoginResolver for ScriptLogin {
        async fn resolve(&self) -> anyhow::Result<String> {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok("someone-else".to_string()))
        }
    }

    /// A login resolver that always succeeds with `login`.
    pub(in crate::cli::github) struct FixedLogin(pub(in crate::cli::github) &'static str);

    impl LoginResolver for FixedLogin {
        async fn resolve(&self) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }
}
