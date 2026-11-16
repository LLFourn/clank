//! The default Clank web host. Its parent supplies one startup line
//! on a private pipe and keeps that pipe open for the host's lifetime.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{dns, host::Host, tunnel};

#[derive(Deserialize)]
struct Start {
    session: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    tunnel: bool,
}

#[derive(Serialize)]
struct Ready<'a> {
    local_url: &'a str,
    connector_url: &'a str,
    credential: &'a str,
    admin_credential: &'a str,
    public_url: Option<&'a str>,
}

/// What a [`StoreWatch`] tells its host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreEvent {
    /// The store was written.
    Changed,
    /// The watch failed, and says nothing more from now on.
    Lost(String),
}

/// Tells the host when another process writes the browser sessions
/// store, which this crate has no way to watch itself.
pub trait StoreWatch: Send + Sync {
    /// Call `heard` after each write to `file` from now on, and once with
    /// [`StoreEvent::Lost`] if the watch fails, for as long as the
    /// returned guard lives.
    fn watch(
        &self,
        file: &std::path::Path,
        heard: Box<dyn Fn(StoreEvent) + Send + Sync>,
    ) -> anyhow::Result<Box<dyn Send>>;
}

/// The host process, starting any tunnel command through `spawn`,
/// hearing other hosts' session changes through `store`, and holding the
/// user config to `check` whenever it writes it.
pub fn main(
    spawn: std::sync::Arc<dyn tunnel::Spawn>,
    store: std::sync::Arc<dyn StoreWatch>,
    check: clank_user_config::Check,
) {
    let outcome = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run(spawn, store, check)));
    if let Err(error) = outcome {
        let _ = write_line(&serde_json::json!({"error":format!("{error:#}")}));
        std::process::exit(1);
    }
}

async fn run(
    spawn: std::sync::Arc<dyn tunnel::Spawn>,
    store: std::sync::Arc<dyn StoreWatch>,
    check: clank_user_config::Check,
) -> anyhow::Result<()> {
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let mut line = String::new();
    anyhow::ensure!(stdin.read_line(&mut line)? > 0, "startup pipe closed");
    anyhow::ensure!(line.len() <= 4096, "startup request too long");
    let start: Start = serde_json::from_str(&line)?;
    anyhow::ensure!(
        !start.session.is_empty() && start.session.len() <= 128,
        "invalid session handle"
    );
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let (stop, mut stopped) = tokio::sync::watch::channel(false);
    std::thread::spawn(move || {
        line.clear();
        let _ = stdin.read_line(&mut line);
        let _ = stop.send(true);
    });

    let host = Host::start(start.port, start.session, home.clone(), check).await?;
    let _store_watch = match &home {
        Some(home) => Some(watch_sessions(&*store, home, host.door.clone())?),
        None => None,
    };
    let mut active_tunnel = None;
    let outcome = async {
        let port = url::Url::parse(&host.local_url)?
            .port()
            .ok_or_else(|| anyhow::anyhow!("host port missing"))?;
        if start.tunnel {
            let home = home
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("a tunnel needs HOME"))?;
            let (provider, grace) = tunnel::configured(Some(home), spawn)?
                .ok_or_else(|| anyhow::anyhow!("no remote tunnel configured"))?;
            let mut setup = tunnel::Start {
                provider,
                grace,
                abort: stopped.clone(),
                ask: std::sync::Arc::new(dns::Net::default()),
            };
            active_tunnel = Some(tunnel::Tunnel::up(&mut setup, home, port, &host.nonce).await?);
            host.set_public_url(&active_tunnel.as_ref().unwrap().url)?;
        }
        write_line(&Ready {
            local_url: &host.local_url,
            connector_url: &host.connector_url,
            credential: &host.credential,
            admin_credential: &host.admin_credential,
            public_url: active_tunnel.as_ref().map(|tunnel| tunnel.url.as_str()),
        })?;
        while !*stopped.borrow() {
            if stopped.changed().await.is_err() {
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Some(tunnel) = active_tunnel {
        tunnel.stop().await;
    }
    host.stop().await;
    outcome
}

/// Refresh `door` on every write to its sessions store, and once more
/// after arming: a write between the door's first read and the watch
/// would otherwise go unheard. A lost watch blinds the door
/// ([`crate::door::Door::blind`]): a session another host revokes after
/// it could stay open here until it expires.
pub fn watch_sessions(
    store: &dyn StoreWatch,
    home: &std::path::Path,
    door: std::sync::Arc<crate::door::Door>,
) -> anyhow::Result<Box<dyn Send>> {
    let refreshed = door.clone();
    let guard = store.watch(
        &crate::door::sessions_path(home),
        Box::new(move |event| match event {
            StoreEvent::Changed => refreshed.refresh(),
            StoreEvent::Lost(why) => refreshed.blind(&format!(
                "the sessions store's watch failed ({why}), so another host's revocation \
                 cannot reach this one"
            )),
        }),
    )?;
    door.refresh();
    Ok(guard)
}

fn write_line(value: &impl Serialize) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
