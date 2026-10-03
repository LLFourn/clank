//! The repo half of the separate web host. It owns the producers and
//! its child process; the child has no repo path or pane handle.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::watch;
use tokio::task::JoinSet;

use super::{Blocking, Feed, Panes, SiteBuilder, Startup, Subscription, Tails};

#[derive(Deserialize)]
struct Ready {
    local_url: Option<String>,
    connector_url: Option<String>,
    credential: Option<String>,
    admin_credential: Option<String>,
    public_url: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct AdminState {
    pub(crate) token: String,
    pub(crate) sessions: Vec<super::door::SessionRecord>,
}

struct OwnedHost {
    child: Child,
    input: ChildStdin,
}

#[cfg(test)]
struct FixtureHost {
    host: clank_web_cloudflare::host::Host,
    tunnel: Option<super::tunnel::Tunnel>,
}

#[cfg(test)]
impl FixtureHost {
    async fn stop(self) {
        if let Some(tunnel) = self.tunnel {
            tunnel.stop().await;
        }
        self.host.stop().await;
    }
}

impl OwnedHost {
    async fn stop(mut self) {
        let _ = self.input.write_all(b"shutdown\n").await;
        drop(self.input);
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
            let _ = self.child.wait().await;
        }
    }
}

/// One repo attachment to its owned local host child. All browser
/// traffic crosses the same `clank-web` connector as an external host.
pub(crate) struct BridgeInstance {
    pub(crate) url: String,
    pub(crate) public_url: Option<String>,
    admin_credential: Option<String>,
    admin_client: reqwest::Client,
    host: Option<OwnedHost>,
    #[cfg(test)]
    fixture: Option<FixtureHost>,
    cancel: watch::Sender<bool>,
    tasks: JoinSet<()>,
    blocking: Arc<Blocking>,
    grace: Duration,
    subscription: Arc<Mutex<Option<Subscription>>>,
    tails: Arc<Mutex<Tails>>,
    builder: Option<SiteBuilder>,
    feed: Feed,
    connection_problem: watch::Receiver<clank_web::bridge::ConnectionStatus>,
    repo: PathBuf,
    home: Option<PathBuf>,
}

impl BridgeInstance {
    #[cfg(not(test))]
    pub(crate) async fn start(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: Arc<dyn Panes>,
        how: Startup,
    ) -> anyhow::Result<Self> {
        let binary = std::env::current_exe()?.with_file_name("clank-web-cloudflare");
        Self::start_at(repo, home, panes, how, binary).await
    }

    /// Unit tests use an in-process host as a protocol fixture. They
    /// still exercise the same connector, handshake, snapshots and
    /// browser service; the process smoke exercises the child path.
    #[cfg(test)]
    pub(crate) async fn start(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: Arc<dyn Panes>,
        mut how: Startup,
    ) -> anyhow::Result<Self> {
        let table = panes
            .table()
            .ok_or_else(|| anyhow::anyhow!("zellij did not answer for `{}`", panes.session()))?;
        let port = crate::agent_store::web_port(&repo)?.unwrap_or(0);
        let host =
            clank_web_cloudflare::host::Host::start(port, panes.session().to_owned(), home.clone())
                .await?;
        let actual_port = url::Url::parse(&host.local_url)?
            .port()
            .ok_or_else(|| anyhow::anyhow!("host URL has no port"))?;
        if let Err(error) = crate::agent_store::record_web_port(&repo, actual_port) {
            host.stop().await;
            return Err(error);
        }
        let mut tunnel = None;
        if let Some(mut start) = how.tunnel.take() {
            let Some(home) = home.as_ref() else {
                host.stop().await;
                anyhow::bail!("a tunnel needs a home for its lease");
            };
            match super::tunnel::Tunnel::up(&mut start, home, actual_port, &host.nonce).await {
                Ok(active) => {
                    if let Err(error) = host.set_public_url(&active.url) {
                        active.stop().await;
                        host.stop().await;
                        return Err(error);
                    }
                    tunnel = Some(active);
                }
                Err(error) => {
                    host.stop().await;
                    return Err(error);
                }
            }
        }
        let ready = Ready {
            local_url: Some(host.local_url.clone()),
            connector_url: Some(host.connector_url.clone()),
            credential: Some(host.credential.clone()),
            admin_credential: Some(host.admin_credential.clone()),
            public_url: tunnel.as_ref().map(|tunnel| tunnel.url.clone()),
            error: None,
        };
        let session = panes.session().to_owned();
        match Self::start_ready(repo, home, panes, table, how, None, ready, session).await {
            Ok(mut instance) => {
                instance.fixture = Some(FixtureHost { host, tunnel });
                Ok(instance)
            }
            Err(error) => {
                FixtureHost { host, tunnel }.stop().await;
                Err(error)
            }
        }
    }

    pub(crate) async fn attach(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: Arc<dyn Panes>,
        how: Startup,
        target: crate::cli::config::WebAttach,
    ) -> anyhow::Result<Self> {
        target.validate()?;
        anyhow::ensure!(
            how.tunnel.is_none(),
            "an existing web host owns its own tunnel"
        );
        let table = panes
            .table()
            .ok_or_else(|| anyhow::anyhow!("zellij did not answer for `{}`", panes.session()))?;
        Self::start_ready(
            repo,
            home,
            panes,
            table,
            how,
            None,
            Ready {
                local_url: Some(target.browser_url),
                connector_url: Some(target.endpoint),
                credential: Some(target.credential),
                admin_credential: None,
                public_url: None,
                error: None,
            },
            target.session,
        )
        .await
    }

    async fn start_at(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: Arc<dyn Panes>,
        how: Startup,
        binary: PathBuf,
    ) -> anyhow::Result<Self> {
        let table = panes
            .table()
            .ok_or_else(|| anyhow::anyhow!("zellij did not answer for `{}`", panes.session()))?;
        let port = crate::agent_store::web_port(&repo)?.unwrap_or(0);
        let (host, ready) = start_host(
            binary,
            panes.session(),
            port,
            home.as_deref(),
            how.tunnel.as_ref().map(|start| start.grace),
            how.tunnel.as_ref().map(|start| start.abort.clone()),
        )
        .await?;
        let session = panes.session().to_owned();
        Self::start_ready(repo, home, panes, table, how, Some(host), ready, session).await
    }

    async fn start_ready(
        repo: PathBuf,
        home: Option<PathBuf>,
        panes: Arc<dyn Panes>,
        table: Vec<super::PaneMeta>,
        how: Startup,
        mut host: Option<OwnedHost>,
        ready: Ready,
        session: String,
    ) -> anyhow::Result<Self> {
        let Startup {
            tunnel: _,
            poll,
            grace,
            seed,
            reopen,
            wake,
        } = how;
        let url = ready
            .local_url
            .ok_or_else(|| anyhow::anyhow!("host did not return its local URL"))?;
        let endpoint = ready
            .connector_url
            .ok_or_else(|| anyhow::anyhow!("host did not return its connector endpoint"))?;
        let credential = ready
            .credential
            .ok_or_else(|| anyhow::anyhow!("host did not return its connector credential"))?;
        let admin_credential = ready.admin_credential;
        anyhow::ensure!(
            host.is_none() || admin_credential.is_some(),
            "host did not return its administration credential"
        );
        let admin_client = reqwest::Client::builder().no_proxy().build()?;
        if host.is_some() {
            let actual_port = url::Url::parse(&url)?
                .port()
                .ok_or_else(|| anyhow::anyhow!("host URL has no port"))?;
            if let Err(error) = crate::agent_store::record_web_port(&repo, actual_port) {
                if let Some(host) = host.take() {
                    host.stop().await;
                }
                return Err(error);
            }
        }

        let feed = Feed::new();
        feed.panes(table.clone());
        feed.status(seed);
        let subscription = Arc::new(Mutex::new(super::subscribe(&*panes, &table, &feed)));
        let (cancel, cancelled) = watch::channel(false);
        let blocking = Arc::new(Blocking::default());
        let backend = Arc::new(super::repo_session::RepoSession::new(
            repo.clone(),
            home.clone(),
            panes.clone(),
            feed.clone(),
            reopen,
            blocking.clone(),
            cancelled.clone(),
        ));
        let mut tasks = JoinSet::new();
        let (attached_tx, mut attached_rx) =
            watch::channel(clank_web::bridge::ConnectionStatus::Connecting);
        let connection_problem = attached_rx.clone();
        tasks.spawn(super::pane_poll(
            panes,
            feed.clone(),
            subscription.clone(),
            cancelled.clone(),
            poll,
            blocking.clone(),
        ));
        tasks.spawn(connector(
            endpoint,
            session,
            credential,
            backend,
            attached_tx,
            cancelled,
        ));
        let builder = SiteBuilder::start(repo.clone(), home.clone());
        let mut instance = Self {
            url,
            public_url: ready.public_url,
            admin_credential,
            admin_client,
            host,
            #[cfg(test)]
            fixture: None,
            cancel,
            tasks,
            blocking,
            grace,
            subscription,
            tails: Arc::new(Mutex::new(Tails::default())),
            builder: Some(builder),
            feed,
            connection_problem,
            repo,
            home,
        };
        // The first complete image may take any number of transport blocks.
        // Connection/handshake failures are reported by the owning connector;
        // a total image deadline would reject a valid progressing transfer.
        while *attached_rx.borrow_and_update() == clank_web::bridge::ConnectionStatus::Connecting {
            if attached_rx.changed().await.is_err() {
                break;
            }
        }
        let status = attached_rx.borrow().clone();
        match status {
            clank_web::bridge::ConnectionStatus::Ready => {
                instance.tasks.spawn(async move {
                    while attached_rx.changed().await.is_ok() {
                        if let Some(wake) = &wake {
                            wake();
                        }
                    }
                });
                Ok(instance)
            }
            failure => {
                instance.stop().await;
                let why = match failure {
                    clank_web::bridge::ConnectionStatus::Disconnected(why) => why,
                    _ => "connector ended before publishing its first image".into(),
                };
                anyhow::bail!("the web host started but the repo did not attach: {why}")
            }
        }
    }

    pub(crate) fn observe(&self, snap: &crate::cli::status::StatusSnapshot) {
        let chat = {
            let mut tails = self.tails.lock().unwrap_or_else(|error| error.into_inner());
            tails.reap();
            let super::Tails {
                handles,
                retired,
                next_owner,
            } = &mut *tails;
            super::reconcile_tails(
                &self.repo,
                self.home.as_deref(),
                snap,
                &self.feed,
                handles,
                retired,
                next_owner,
            )
        };
        self.feed
            .status(super::status_payload(&self.repo, snap, &chat));
        if let Some(builder) = &self.builder {
            builder.request();
        }
    }

    pub(crate) fn connection_problem(&self) -> Option<String> {
        match &*self.connection_problem.borrow() {
            clank_web::bridge::ConnectionStatus::Ready => None,
            clank_web::bridge::ConnectionStatus::Connecting => {
                Some("repo connector is connecting".into())
            }
            clank_web::bridge::ConnectionStatus::Disconnected(why) => Some(why.clone()),
        }
    }

    pub(crate) fn owns_host(&self) -> bool {
        self.host.is_some() || {
            #[cfg(test)]
            {
                self.fixture.is_some()
            }
            #[cfg(not(test))]
            {
                false
            }
        }
    }

    pub(crate) fn admin_available(&self) -> bool {
        self.owns_host() && self.admin_credential.is_some()
    }

    async fn admin_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        let capability = self
            .admin_credential
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("this host did not grant administration"))?;
        let mut request = self
            .admin_client
            .request(
                method,
                format!("{}/admin/{path}", self.url.trim_end_matches('/')),
            )
            .header("x-clank-admin", capability);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let (status, bytes) = tokio::time::timeout(Duration::from_secs(3), async {
            let answer = request.send().await?;
            let status = answer.status();
            let bytes = answer.bytes().await?;
            Ok::<_, reqwest::Error>((status, bytes))
        })
        .await??;
        anyhow::ensure!(
            status.is_success(),
            "host administration failed ({status}): {}",
            String::from_utf8_lossy(&bytes)
        );
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub(crate) async fn admin_state(&self) -> anyhow::Result<AdminState> {
        Ok(serde_json::from_value(
            self.admin_request(reqwest::Method::GET, "state", None)
                .await?,
        )?)
    }

    pub(crate) async fn admin_link(&self) -> anyhow::Result<String> {
        let answer = self
            .admin_request(reqwest::Method::POST, "link", None)
            .await?;
        Ok(answer["token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("host returned no login link"))?
            .to_owned())
    }

    pub(crate) async fn admin_rotate(&self) -> anyhow::Result<()> {
        self.admin_request(reqwest::Method::POST, "rotate", None)
            .await?;
        Ok(())
    }

    pub(crate) async fn admin_revoke(&self, id_hash: &str) -> anyhow::Result<()> {
        self.admin_request(
            reqwest::Method::POST,
            "revoke",
            Some(serde_json::json!({"id_hash":id_hash})),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn stop(mut self) {
        let _ = self.cancel.send(true);
        if let Some(host) = self.host.take() {
            host.stop().await;
        }
        #[cfg(test)]
        if let Some(fixture) = self.fixture.take() {
            fixture.stop().await;
        }
        let deadline = tokio::time::Instant::now() + self.grace;
        while !self.tasks.is_empty() {
            match tokio::time::timeout_at(deadline, self.tasks.join_next()).await {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    self.tasks.shutdown().await;
                    break;
                }
            }
        }
        self.blocking.join_all().await;
        let subscription = self
            .subscription
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        let tails = self.tails.clone();
        let builder = self.builder.take();
        let _ = tokio::task::spawn_blocking(move || {
            if let Some(subscription) = subscription {
                subscription.end();
            }
            let handles = {
                let mut tails = tails.lock().unwrap_or_else(|error| error.into_inner());
                let mut handles: Vec<_> = tails.handles.drain().map(|(_, handle)| handle).collect();
                handles.append(&mut tails.retired);
                handles
            };
            for handle in handles {
                handle.stop_and_join();
            }
            if let Some(builder) = builder {
                builder.stop();
            }
        })
        .await;
    }
}

impl Drop for BridgeInstance {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(host) = self.host.as_mut() {
            let _ = host.child.start_kill();
        }
        #[cfg(test)]
        if let Some(fixture) = self.fixture.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(fixture.stop());
        }
        drop(
            self.subscription
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take(),
        );
    }
}

async fn connector(
    endpoint: String,
    session: String,
    credential: String,
    backend: Arc<super::repo_session::RepoSession>,
    attached: watch::Sender<clank_web::bridge::ConnectionStatus>,
    mut cancelled: watch::Receiver<bool>,
) {
    loop {
        let result = tokio::select! {
            _ = cancelled.changed() => return,
            result = clank_web::bridge::connect_signaled(&endpoint, &session, &credential, backend.clone(), Some(attached.clone())) => result,
        };
        if let Err(error) = result {
            attached.send_replace(clank_web::bridge::ConnectionStatus::Disconnected(format!(
                "connector: {error:#}"
            )));
            if error
                .downcast_ref::<clank_web::bridge::ProtocolMismatch>()
                .is_some()
            {
                return;
            }
        } else {
            attached.send_replace(clank_web::bridge::ConnectionStatus::Disconnected(
                "repo connector disconnected".into(),
            ));
        }
        tokio::select! {
            _ = cancelled.changed() => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {},
        }
    }
}

async fn start_host(
    binary: PathBuf,
    session: &str,
    port: u16,
    home: Option<&std::path::Path>,
    tunnel_grace: Option<Duration>,
    abandoned: Option<watch::Receiver<bool>>,
) -> anyhow::Result<(OwnedHost, Ready)> {
    let mut command = Command::new(&binary);
    if let Some(home) = home {
        command.env("HOME", home);
    }
    let mut child = command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| anyhow::anyhow!("starting `{}`: {error}", binary.display()))?;
    let mut input = child.stdin.take().expect("piped child stdin");
    let output = child.stdout.take().expect("piped child stdout");
    let start = serde_json::json!({"session":session,"port":port,"tunnel":tunnel_grace.is_some()});
    let ready = async {
        input.write_all(format!("{start}\n").as_bytes()).await?;
        let mut line = String::new();
        let mut reader = BufReader::new(output);
        let deadline = tunnel_grace.unwrap_or(Duration::from_secs(10)) + Duration::from_secs(10);
        tokio::select! {
            _ = canceled(abandoned) => Err(anyhow::anyhow!("the remote start was abandoned")),
            answer = tokio::time::timeout(deadline, reader.read_line(&mut line)) => match answer {
                Ok(Ok(n)) if n > 0 => Ok(()),
                Ok(Ok(_)) => Err(anyhow::anyhow!("web host exited before readiness")),
                Ok(Err(error)) => Err(error.into()),
                Err(_) => Err(anyhow::anyhow!("web host did not become ready within {}s", deadline.as_secs())),
            },
        }?;
        let ready: Ready = serde_json::from_str(&line)
            .map_err(|error| anyhow::anyhow!("invalid web host readiness: {error}"))?;
        if let Some(error) = ready.error.as_deref() {
            anyhow::bail!("web host: {error}");
        }
        anyhow::ensure!(
            ready.local_url.is_some()
                && ready.connector_url.is_some()
                && ready.credential.is_some(),
            "web host readiness omitted its endpoint or credential"
        );
        let local_url = ready.local_url.as_deref().expect("checked above");
        anyhow::ensure!(
            url::Url::parse(local_url)
                .ok()
                .and_then(|url| url.port())
                .is_some(),
            "invalid web host readiness URL"
        );
        Ok::<Ready, anyhow::Error>(ready)
    }
    .await;
    let ready = match ready {
        Ok(ready) => ready,
        Err(error) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error);
        }
    };
    Ok((OwnedHost { child, input }, ready))
}

async fn canceled(mut signal: Option<watch::Receiver<bool>>) {
    let Some(signal) = signal.as_mut() else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *signal.borrow() {
            return;
        }
        if signal.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use super::{BridgeInstance, Panes, Startup, start_host};
    use crate::cli::config::WebAttach;
    use crate::cli::open_zellij::{PaneMeta, SubscribeChild};
    use crate::cli::web::door::Door;

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_child_readiness_reaps_the_process() {
        use std::os::unix::fs::PermissionsExt;

        for (name, line) in [
            ("malformed", "not-json"),
            ("refused", r#"{"error":"startup failed"}"#),
            ("incomplete", r#"{"local_url":"http://localhost:1"}"#),
            (
                "bad-url",
                r#"{"local_url":"oops","connector_url":"ws://localhost:1/connector","credential":"secret"}"#,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let binary = dir.path().join("host");
            let pid_file = dir.path().join("pid");
            std::fs::write(
                &binary,
                format!(
                    "#!/bin/sh\nread start\necho $$ > '{}'\nprintf '%s\\n' '{}'\nexec sleep 60\n",
                    pid_file.display(),
                    line
                ),
            )
            .unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
            let error = if name == "bad-url" {
                std::fs::create_dir_all(dir.path().join(".clank")).unwrap();
                std::fs::write(
                    dir.path().join(".clank/config.json"),
                    r#"{"agents":{"grok":{"tool":"grok","role":"master"}}}"#,
                )
                .unwrap();
                BridgeInstance::start_at(
                    dir.path().to_path_buf(),
                    None,
                    Arc::new(TestPanes {
                        sent: Arc::new(Mutex::new(Vec::new())),
                    }),
                    Startup {
                        tunnel: None,
                        poll: Duration::from_secs(3600),
                        grace: Duration::from_secs(2),
                        seed: serde_json::json!({"phase":"ready"}),
                        reopen: None,
                        wake: None,
                    },
                    binary,
                )
                .await
                .err()
                .expect("invalid readiness must fail")
            } else {
                start_host(binary, "team", 0, None, None, None)
                    .await
                    .err()
                    .expect("invalid readiness must fail")
            };
            assert!(
                error.to_string().contains(match name {
                    "malformed" => "invalid web host readiness",
                    "refused" => "startup failed",
                    "bad-url" => "invalid web host readiness URL",
                    _ => "omitted its endpoint or credential",
                }),
                "{name}: {error:#}"
            );
            let pid: i32 = std::fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "{name}: child survived");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }

    struct TestPanes {
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl Panes for TestPanes {
        fn session(&self) -> &str {
            "bridge-smoke"
        }
        fn table(&self) -> Option<Vec<PaneMeta>> {
            Some(vec![PaneMeta {
                id: "terminal_1".into(),
                label: "grok".into(),
                columns: 80,
                rows: 24,
                exited: false,
            }])
        }
        fn subscribe(&self, _: &[String]) -> Option<SubscribeChild> {
            None
        }
        fn say(&self, _: &str, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_owned());
            Ok(())
        }
        fn stop(&self, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn existing_host_stays_running_after_the_repo_detaches() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"grok":{"tool":"grok","role":"master"}}}"#,
        )
        .unwrap();
        let host = clank_web_cloudflare::host::Host::start(
            0,
            "existing-session".into(),
            Some(home.path().to_path_buf()),
        )
        .await
        .unwrap();
        let url = host.local_url.clone();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let instance = BridgeInstance::attach(
            repo.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            Arc::new(TestPanes { sent: sent.clone() }),
            Startup {
                tunnel: None,
                poll: Duration::from_secs(3600),
                grace: Duration::from_secs(2),
                seed: serde_json::json!({"phase":"existing"}),
                reopen: None,
                wake: None,
            },
            WebAttach {
                endpoint: host.connector_url.clone(),
                session: "existing-session".into(),
                credential: host.credential.clone(),
                browser_url: url.clone(),
            },
        )
        .await
        .unwrap();
        assert!(!instance.owns_host());
        assert!(host.session.connected().await);
        let result = host
            .session
            .call(
                clank_web::request::BrowserRequest::Say {
                    pane: "terminal_1".into(),
                    label: "grok".into(),
                    session: None,
                    text: "hello existing host".into(),
                }
                .into_action(),
            )
            .await
            .unwrap();
        let clank_web::bridge::ActionResult::Completed { value } = result else {
            panic!("the existing host did not dispatch input");
        };
        assert_eq!(
            clank_web::request::BrowserReply::from_value(value)
                .unwrap()
                .status,
            204
        );
        assert_eq!(sent.lock().unwrap().as_slice(), &["hello existing host"]);
        instance.stop().await;
        assert!(
            host.session
                .call(
                    clank_web::request::BrowserRequest::Say {
                        pane: "terminal_1".into(),
                        label: "grok".into(),
                        session: None,
                        text: "must not reach the pane".into(),
                    }
                    .into_action(),
                )
                .await
                .is_err(),
            "detaching must close the action path"
        );
        assert_eq!(sent.lock().unwrap().as_slice(), &["hello existing host"]);
        let response = reqwest::get(format!("{url}/instance")).await.unwrap();
        assert!(response.status().is_success());
        host.stop().await;
    }

    #[tokio::test]
    async fn startup_waits_for_a_large_image_without_a_total_deadline() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".clank")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(matches!(
                socket.next().await.unwrap().unwrap(),
                Message::Text(_)
            ));
            socket
                .send(Message::Text(r#"{"type":"ready","version":2}"#.into()))
                .await
                .unwrap();
            let mut sequence = None;
            let mut size = 0;
            while let Some(Ok(Message::Binary(bytes))) = socket.next().await {
                match bytes[0] {
                    1 => {
                        if sequence.is_none() {
                            sequence = Some(u64::from_le_bytes(bytes[18..26].try_into().unwrap()));
                        }
                        size += bytes.len() - 17;
                    }
                    2 => break,
                    _ => {}
                }
            }
            assert!(size > 5 * 1024 * 1024);
            // A slow host may take longer than the former 10-second CLI
            // deadline to commit an otherwise complete valid image.
            tokio::time::sleep(Duration::from_secs(11)).await;
            let header = |tag: u8, offset: u64| {
                let mut bytes = vec![tag];
                bytes.extend_from_slice(&1u64.to_le_bytes());
                bytes.extend_from_slice(&offset.to_le_bytes());
                bytes
            };
            socket
                .send(Message::Binary(header(0, 0).into()))
                .await
                .unwrap();
            let mut applied = header(1, 0);
            applied.push(2);
            applied.extend_from_slice(&sequence.unwrap().to_le_bytes());
            socket.send(Message::Binary(applied.into())).await.unwrap();
            socket
                .send(Message::Binary(header(2, 9).into()))
                .await
                .unwrap();
            while let Some(Ok(_)) = socket.next().await {}
        });
        let instance = tokio::time::timeout(
            Duration::from_secs(20),
            BridgeInstance::attach(
                repo.path().to_path_buf(),
                Some(home.path().to_path_buf()),
                Arc::new(TestPanes {
                    sent: Arc::new(Mutex::new(Vec::new())),
                }),
                Startup {
                    tunnel: None,
                    poll: Duration::from_secs(3600),
                    grace: Duration::from_secs(2),
                    seed: serde_json::json!({"text":"x".repeat(5*1024*1024)}),
                    reopen: None,
                    wake: None,
                },
                WebAttach {
                    endpoint: format!("ws://{address}"),
                    session: "slow".into(),
                    credential: "fixture".into(),
                    browser_url: format!("http://{address}"),
                },
            ),
        )
        .await
        .unwrap()
        .expect("a slow first image must complete");
        assert!(instance.connection_problem().is_none());
        instance.stop().await;
        peer.await.unwrap();
    }

    #[tokio::test]
    async fn large_harness_turns_reach_a_browser_and_survive_connector_outages() {
        use crate::cli::web::{
            Blocking, Feed,
            repo_session::RepoSession,
            transcript::{self, Parsed},
        };
        use clank_core::vocab::Tool;
        use clank_web::bridge::Backend;
        use tokio::net::TcpStream;
        use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

        async fn event(
            socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
            kind: &str,
        ) -> serde_json::Value {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let frame = socket.next().await.expect("socket remains open").unwrap();
                    let Ok(text) = frame.to_text() else {
                        continue;
                    };
                    let Some(data) = text.strip_prefix(&format!("event: {kind}\ndata: ")) else {
                        continue;
                    };
                    return serde_json::from_str(data.trim()).unwrap();
                }
            })
            .await
            .unwrap()
        }
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let host = clank_web_cloudflare::host::Host::start(
            0,
            "large-transcript".into(),
            Some(home.path().to_path_buf()),
        )
        .await
        .unwrap();
        let feed = Feed::new();
        feed.panes(vec![PaneMeta {
            id: "terminal_1".into(),
            label: "grok".into(),
            columns: 80,
            rows: 24,
            exited: false,
        }]);
        feed.status(serde_json::json!({"facts":{"agents":[{"label":"grok"}]}}));
        feed.claim_turns("grok", 1, Some("session-one"));
        let text = "🦀 \"quote\" \\\n".repeat(100_000);
        // Drive the shipping harness parser and markdown renderer, not a
        // transport-only JSON fixture. Text and rendered HTML both cross.
        let line = serde_json::json!({"type":"response_item","payload":{"type":"message","id":"large-turn","role":"user","content":[{"type":"input_text","text":text}]}}).to_string();
        let Parsed::Turn(turn) = transcript::parse(Tool::Codex, &line).pop().unwrap() else {
            panic!("expected parsed turn")
        };
        let expected = serde_json::to_value(&turn).unwrap();
        feed.turns_reset("grok", 1, "session-one", 0, vec![turn.clone()]);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let backend = Arc::new(RepoSession::new(
            repo.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            Arc::new(TestPanes {
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            feed.clone(),
            None,
            Arc::new(Blocking::default()),
            cancelled,
        ));
        let start = || {
            let endpoint = host.connector_url.clone();
            let credential = host.credential.clone();
            let backend = backend.clone();
            tokio::spawn(async move {
                clank_web::bridge::connect(&endpoint, "large-transcript", &credential, backend)
                    .await
            })
        };
        let connector = start();
        let (mut view, mut changes) = host.session.subscribe_with_snapshot().await;
        while !view.connected {
            clank_web::bridge::apply_event(
                &mut view,
                tokio::time::timeout(Duration::from_secs(10), changes.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        }
        let door = Door::new(Some(home.path().to_path_buf()));
        let cookie = format!(
            "{}={}",
            crate::cli::web::door::COOKIE,
            door.open_session("large").unwrap()
        );
        let mut request = format!("{}/events", host.local_url.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        request
            .headers_mut()
            .insert("origin", host.local_url.parse().unwrap());
        let mut browser = tokio_tungstenite::connect_async(request.clone())
            .await
            .unwrap()
            .0;
        let replay = event(&mut browser, "turns").await;
        assert_eq!(replay["turns"][0], expected);
        assert!(serde_json::to_vec(&replay).unwrap().len() > 1024 * 1024);
        assert_eq!(event(&mut browser, "connection").await["connected"], true);
        let mut live = turn;
        live.id = "second-large-turn".into();
        let expected_live = serde_json::to_value(&live).unwrap();
        feed.turn("grok", 1, "session-one", 0, live);
        assert_eq!(event(&mut browser, "turn").await["turn"], expected_live);
        connector.abort();
        let _ = connector.await;
        assert_eq!(event(&mut browser, "connection").await["connected"], false);
        let mut late = tokio_tungstenite::connect_async(request).await.unwrap().0;
        let retained = event(&mut late, "turns").await;
        assert_eq!(retained["turns"][0], expected);
        assert_eq!(retained["turns"][1], expected_live);
        assert_eq!(event(&mut late, "connection").await["connected"], false);
        let restarted = start();
        assert_eq!(
            event(&mut browser, "turns").await["turns"],
            retained["turns"]
        );
        assert_eq!(event(&mut browser, "connection").await["connected"], true);
        assert_eq!(event(&mut late, "turns").await["turns"], retained["turns"]);
        assert_eq!(event(&mut late, "connection").await["connected"], true);
        let snapshot = backend.subscribe_with_snapshot().await.0;
        assert_eq!(
            snapshot.sequence,
            host.session
                .subscribe_with_snapshot()
                .await
                .0
                .snapshot
                .unwrap()
                .sequence
        );
        restarted.abort();
        let _ = restarted.await;
        drop(cancel);
        host.stop().await;
    }

    #[tokio::test]
    async fn started_and_attached_instances_serve_the_same_browser_features() {
        let mut outcomes = Vec::new();
        for owned in [true, false] {
            let repo = tempfile::tempdir().unwrap();
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(repo.path().join(".clank/html")).unwrap();
            std::fs::write(
                repo.path().join(".clank/config.json"),
                r#"{"agents":{"grok":{"tool":"grok","role":"master"}}}"#,
            )
            .unwrap();
            std::fs::write(
                repo.path().join(".clank/html/index.html"),
                "<h1>repo page</h1>",
            )
            .unwrap();
            let sent = Arc::new(Mutex::new(Vec::new()));
            let panes = Arc::new(TestPanes { sent: sent.clone() });
            let door = Arc::new(Door::new(Some(home.path().to_path_buf())));
            let startup = Startup {
                tunnel: None,
                poll: Duration::from_secs(3600),
                grace: Duration::from_secs(2),
                seed: serde_json::json!({"phase":"ready"}),
                reopen: None,
                wake: None,
            };
            let external = if owned {
                None
            } else {
                Some(
                    clank_web_cloudflare::host::Host::start(
                        0,
                        "bridge-smoke".into(),
                        Some(home.path().to_path_buf()),
                    )
                    .await
                    .unwrap(),
                )
            };
            let instance = if let Some(host) = &external {
                BridgeInstance::attach(
                    repo.path().to_path_buf(),
                    Some(home.path().to_path_buf()),
                    panes,
                    startup,
                    WebAttach {
                        endpoint: host.connector_url.clone(),
                        session: "bridge-smoke".into(),
                        credential: host.credential.clone(),
                        browser_url: host.local_url.clone(),
                    },
                )
                .await
                .unwrap()
            } else {
                BridgeInstance::start(
                    repo.path().to_path_buf(),
                    Some(home.path().to_path_buf()),
                    panes,
                    startup,
                )
                .await
                .unwrap()
            };
            assert_eq!(instance.owns_host(), owned);
            outcomes.push(probe_browser(&instance.url, &door, &sent, repo.path()).await);
            assert_eq!(instance.admin_available(), owned);
            if owned {
                let before = instance.admin_state().await.unwrap();
                assert_eq!(before.sessions.len(), 1);
                let link = instance.admin_link().await.unwrap();
                assert!(
                    door.consume_shared(super::super::door::Link::Login, &link)
                        .unwrap()
                );
                instance
                    .admin_revoke(&before.sessions[0].id_hash)
                    .await
                    .unwrap();
                assert!(instance.admin_state().await.unwrap().sessions.is_empty());
                instance.admin_rotate().await.unwrap();
                assert_ne!(instance.admin_state().await.unwrap().token, before.token);
            } else {
                assert!(instance.admin_state().await.is_err());
            }
            let url = instance.url.clone();
            instance.stop().await;
            let reachable = reqwest::get(format!("{url}/instance")).await.is_ok();
            assert_eq!(reachable, !owned, "ownership controls server lifetime");
            if let Some(host) = external {
                host.stop().await;
            }
        }
        assert_eq!(
            outcomes[0], outcomes[1],
            "both constructors serve the same features"
        );
    }

    async fn probe_browser(
        url: &str,
        door: &Door,
        sent: &Arc<Mutex<Vec<String>>>,
        repo: &std::path::Path,
    ) -> Vec<(String, u16)> {
        let cookie = format!(
            "{}={}",
            crate::cli::web::door::COOKIE,
            door.open_session("parity").unwrap()
        );
        let client = reqwest::Client::new();
        let status: serde_json::Value = client
            .get(format!("{url}/bridge/status"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["connected"], true);
        let mut answers = Vec::new();
        for path in ["", "xterm.js", "html/index.html", "agents/candidates"] {
            let answer = client
                .get(format!("{url}/{path}"))
                .header("cookie", &cookie)
                .send()
                .await
                .unwrap();
            answers.push((path.to_owned(), answer.status().as_u16()));
        }
        let mut socket_request = format!("{}/events", url.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        socket_request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        socket_request
            .headers_mut()
            .insert("origin", url.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(socket_request)
            .await
            .unwrap();
        let mut initial = Vec::new();
        for _ in 0..2 {
            initial.push(
                tokio::time::timeout(Duration::from_secs(3), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .to_text()
                    .unwrap()
                    .to_owned(),
            );
        }
        assert!(initial.iter().any(|frame| frame.contains("event: panes")));
        assert!(
            initial
                .iter()
                .any(|frame| frame.contains("\"phase\":\"ready\""))
        );
        drop(socket);

        for (path, body) in [
            (
                "say",
                serde_json::json!({"pane":"terminal_1","label":"grok","session":null,"text":"parity"}),
            ),
            (
                "stop",
                serde_json::json!({"pane":"terminal_1","label":"grok","session":null}),
            ),
            ("queue", serde_json::json!({"do":"invalid"})),
            ("stash", serde_json::json!({"do":"invalid"})),
            ("agents", serde_json::json!({"do":"invalid"})),
            (
                "blocks/answer",
                serde_json::json!({"agent":"grok","name":"missing","question":"none","answer":"yes"}),
            ),
            ("agents/reopen", serde_json::json!({"label":"grok"})),
        ] {
            let answer = client
                .post(format!("{url}/{path}"))
                .header("cookie", &cookie)
                .header("origin", url)
                .json(&body)
                .send()
                .await
                .unwrap();
            answers.push((path.to_owned(), answer.status().as_u16()));
        }
        assert_eq!(sent.lock().unwrap().as_slice(), &["parity"]);
        let upload = client
            .post(format!("{url}/upload"))
            .header("cookie", &cookie)
            .header("origin", url)
            .header("content-type", "image/png")
            .body(b"parity bytes".to_vec())
            .send()
            .await
            .unwrap();
        answers.push(("upload".into(), upload.status().as_u16()));
        let body: serde_json::Value = upload.json().await.unwrap();
        let path = body["path"].as_str().unwrap();
        assert!(path.starts_with(repo.to_str().unwrap()));
        assert_eq!(std::fs::read(path).unwrap(), b"parity bytes");
        assert_eq!(answers[0].1, 200);
        assert_eq!(answers[1].1, 200);
        assert_eq!(answers[2].1, 200);
        assert_eq!(answers[4].1, 204, "input reaches the local pane");
        assert_eq!(answers[5].1, 204, "interrupt reaches the local pane");
        assert_eq!(answers.last().unwrap().1, 200);
        answers
    }

    #[tokio::test]
    async fn host_crash_reports_disconnection_without_stopping_local_panes() {
        let repo = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".clank")).unwrap();
        std::fs::write(
            repo.path().join(".clank/config.json"),
            r#"{"agents":{"grok":{"tool":"grok","role":"master"}}}"#,
        )
        .unwrap();
        let host = clank_web_cloudflare::host::Host::start(
            0,
            "bridge-smoke".into(),
            Some(home.path().to_path_buf()),
        )
        .await
        .unwrap();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let panes = Arc::new(TestPanes { sent: sent.clone() });
        let instance = BridgeInstance::attach(
            repo.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            panes.clone(),
            Startup {
                tunnel: None,
                poll: Duration::from_secs(3600),
                grace: Duration::from_secs(2),
                seed: serde_json::json!({"phase":"ready"}),
                reopen: None,
                wake: None,
            },
            WebAttach {
                endpoint: host.connector_url.clone(),
                session: "bridge-smoke".into(),
                credential: host.credential.clone(),
                browser_url: host.local_url.clone(),
            },
        )
        .await
        .unwrap();
        assert!(instance.connection_problem().is_none());
        host.stop().await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while instance.connection_problem().is_none() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the TUI must show the host disconnect");
        panes.say("terminal_1", "after host crash").unwrap();
        assert_eq!(sent.lock().unwrap().as_slice(), &["after host crash"]);
        instance.stop().await;
    }
}
