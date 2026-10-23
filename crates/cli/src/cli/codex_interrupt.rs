//! One-shot native control of an already loaded, currently bound Codex thread.
//! No thread resume, history subscription, periodic probe or detached reader.
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clank_core::{Tool, ids::AgentLabel};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

/// False permits terminal fallback only before any native mutation. After an
/// interrupt/start may have been sent, errors forbid another transport attempt.
pub(crate) async fn try_wake(
    repo: &Path,
    label: &AgentLabel,
    hint: &super::pane_input::Admitted,
) -> anyhow::Result<bool> {
    let hint = hint.as_str();
    let Some((thread, socket)) = super::event_interrupt::blocking(|| target(repo, label)) else {
        return Ok(false);
    };
    let Ok(stream) = tokio::net::UnixStream::connect(socket).await else {
        return Ok(false);
    };
    let Ok((socket, _)) = tokio_tungstenite::client_async("ws://localhost/", stream).await else {
        return Ok(false);
    };
    let mut rpc = Rpc { socket, next_id: 1 };
    wake(&mut rpc, repo, &thread, hint, || {
        super::event_interrupt::blocking(|| {
            crate::agent_store::resolve_role(repo, label).is_ok()
                && crate::agent_store::binding_matches(repo, label, Tool::Codex, &thread)
        })
    })
    .await
}

fn target(repo: &Path, label: &AgentLabel) -> Option<(String, PathBuf)> {
    let session = crate::agent_store::load_agent_config(repo, label)
        .ok()??
        .session?;
    if session.tool != Tool::Codex {
        return None;
    }
    let roster = crate::agent_store::try_resolve_via_team(repo).ok()??;
    let desc = if &roster.master == label {
        &roster.master_desc
    } else {
        &roster.members.iter().find(|m| &m.label == label)?.desc
    };
    // Never borrow the sender/monitor's CODEX_HOME. A profile path is only a
    // candidate; loaded-thread, cwd and binding checks authorize its use below.
    let default_home = std::env::var_os("HOME").map(PathBuf::from)?;
    let socket = profile_socket(desc.launch.as_ref(), &default_home)?;
    let socket = own_socket(&socket, unsafe { libc::geteuid() })?;
    Some((session.id.as_str().to_owned(), socket))
}

/// The socket `path` leads to, if it is one and `owner` owns it. Codex
/// publishes its control socket as a link into its daemon's directory, so
/// the checks are made on where the link ends, and that is what is used.
fn own_socket(path: &Path, owner: u32) -> Option<PathBuf> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let socket = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::symlink_metadata(&socket).ok()?;
    (metadata.file_type().is_socket() && metadata.uid() == owner).then_some(socket)
}

fn profile_socket(
    launch: Option<&clank_core::agent_config::LaunchConfig>,
    default_home: &Path,
) -> Option<PathBuf> {
    let env = |key| launch.and_then(|l| l.env.get(key));
    let root = if let Some(root) = env("CODEX_HOME") {
        PathBuf::from(root)
    } else {
        env("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_home.to_owned())
            .join(".codex")
    };
    // Relative overrides depend on the original launch cwd, which is not in a
    // legacy binding. Refuse to guess it from this controller's cwd.
    root.is_absolute()
        .then(|| root.join("app-server-control/app-server-control.sock"))
}

struct Rpc<S> {
    socket: WebSocketStream<S>,
    next_id: u64,
}
impl<S: AsyncRead + AsyncWrite + Unpin> Rpc<S> {
    async fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        self.socket
            .send(Message::Text(
                json!({"method":method,"params":params}).to_string().into(),
            ))
            .await?;
        Ok(())
    }
    async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.socket
            .send(Message::Text(
                json!({"id":id,"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .with_context(|| format!("sending Codex {method}"))?;
        while let Some(frame) = self.socket.next().await {
            let Message::Text(text) = frame? else {
                continue;
            };
            let response: Value = serde_json::from_str(&text)?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if response.get("error").is_some() {
                bail!("Codex {method} rejected control request");
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("Codex {method} returned no result"));
        }
        bail!("Codex control connection closed during {method}")
    }
    async fn latest(&mut self, thread: &str) -> anyhow::Result<Option<Value>> {
        let result = self
            .call(
                "thread/turns/list",
                json!({"threadId":thread,"limit":1,"sortDirection":"desc","itemsView":"notLoaded"}),
            )
            .await?;
        let rows = result
            .get("data")
            .and_then(Value::as_array)
            .context("Codex turn list missing data")?;
        Ok(rows.first().cloned())
    }
}

fn terminal(turn: &Value) -> bool {
    matches!(
        turn.get("status").and_then(Value::as_str),
        Some("completed" | "interrupted" | "failed")
    )
}

/// The read-only checks every control action makes first: the thread is
/// loaded, belongs to this repo, and its latest turn has a known status.
async fn preflight<S: AsyncRead + AsyncWrite + Unpin>(
    rpc: &mut Rpc<S>,
    repo: &Path,
    thread: &str,
) -> anyhow::Result<Option<Value>> {
    rpc.call("initialize", json!({"clientInfo":{"name":"clank","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
    rpc.notify("initialized", json!({})).await?;
    let loaded = rpc.call("thread/loaded/list", json!({})).await?;
    if !loaded
        .get("data")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(thread)))
    {
        bail!("bound Codex thread is not loaded");
    }
    let summary = rpc
        .call(
            "thread/read",
            json!({"threadId":thread,"includeTurns":false}),
        )
        .await?;
    let summary = summary
        .get("thread")
        .context("Codex thread summary missing")?;
    let cwd = summary
        .get("cwd")
        .and_then(Value::as_str)
        .context("Codex thread cwd missing")?;
    if summary.get("id").and_then(Value::as_str) != Some(thread)
        || dunce::canonicalize(cwd)? != dunce::canonicalize(repo)?
    {
        bail!("Codex thread does not belong to target repo");
    }
    let latest = rpc.latest(thread).await?;
    if latest.as_ref().is_some_and(|turn| {
        !terminal(turn) && turn.get("status").and_then(Value::as_str) != Some("inProgress")
    }) {
        bail!("unknown Codex turn status");
    }
    Ok(latest)
}

/// Start one recovery turn on a bound Codex thread whose latest turn is
/// `failed_turn`, failed, once `authorize` (the owning monitor's last look,
/// asked only then) allows it. A newer or running turn is refused, a veto
/// sends nothing, and nothing is ever interrupted.
///
/// A failure before `turn/start` is sent leaves nothing done
/// (`Unavailable`); a failure after it may not have (`Indeterminate`).
pub(crate) async fn start_if_failed(
    repo: &Path,
    label: &AgentLabel,
    failed_turn: &str,
    wake: &super::pane_input::Admitted,
    authorize: impl std::future::Future<Output = bool>,
) -> crate::cli::recovery::Answer {
    use crate::cli::recovery::Answer;
    let wake = wake.as_str();
    let Some((thread, socket)) = super::event_interrupt::blocking(|| target(repo, label)) else {
        return Answer::Unavailable;
    };
    let connect = async {
        let stream = tokio::net::UnixStream::connect(socket).await.ok()?;
        let (socket, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .ok()?;
        Some(socket)
    };
    let Ok(Some(socket)) = tokio::time::timeout(BEFORE_SENDING, connect).await else {
        return Answer::Unavailable;
    };
    let mut rpc = Rpc { socket, next_id: 1 };
    start_after(&mut rpc, repo, &thread, failed_turn, wake, authorize).await
}

/// How long a recovery start may take before anything is sent, and after.
/// Both well inside `recovery::ANSWER_WITHIN`, so the transport's own
/// answer, not the episode's deadline, says which side of the send it was.
const BEFORE_SENDING: Duration = Duration::from_secs(15);
const AFTER_SENDING: Duration = Duration::from_secs(30);

async fn start_after<S: AsyncRead + AsyncWrite + Unpin>(
    rpc: &mut Rpc<S>,
    repo: &Path,
    thread: &str,
    failed_turn: &str,
    wake: &str,
    authorize: impl std::future::Future<Output = bool>,
) -> crate::cli::recovery::Answer {
    use crate::cli::recovery::Answer;
    let Ok(Ok(latest)) = tokio::time::timeout(BEFORE_SENDING, preflight(rpc, repo, thread)).await
    else {
        return Answer::Unavailable;
    };
    let failed_is_latest = latest.as_ref().is_some_and(|turn| {
        turn.get("id").and_then(Value::as_str) == Some(failed_turn)
            && turn.get("status").and_then(Value::as_str) == Some("failed")
    });
    if !failed_is_latest {
        return Answer::Refused;
    }
    if !authorize.await {
        return Answer::Unavailable;
    }
    let start = rpc.call(
        "turn/start",
        json!({"threadId":thread,"input":[{"type":"text","text":wake}]}),
    );
    match tokio::time::timeout(AFTER_SENDING, start)
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("no answer to turn/start")))
    {
        Ok(result) => Answer::Submitted {
            started: result
                .pointer("/turn/id")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        Err(_) => Answer::Indeterminate,
    }
}

async fn wake<S: AsyncRead + AsyncWrite + Unpin>(
    rpc: &mut Rpc<S>,
    repo: &Path,
    thread: &str,
    hint: &str,
    still_bound: impl Fn() -> bool,
) -> anyhow::Result<bool> {
    // Everything in this preflight is read-only. Unsupported versions, absent
    // live threads, and foreign profiles can still use verified pane targeting.
    let Ok(latest) = preflight(rpc, repo, thread).await else {
        return Ok(false);
    };
    if !still_bound() {
        return Ok(false);
    }
    if let Some(turn) = latest.filter(|turn| !terminal(turn)) {
        let id = turn
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .context("active Codex turn has no id")?;
        // Current upstream defers this *correlated RPC response* until
        // TurnAborted. Older/immediate-ack implementations are safe too: one
        // confirming read must show this exact turn ended. Never poll/resume.
        rpc.call("turn/interrupt", json!({"threadId":thread,"turnId":id}))
            .await?;
        let ended = rpc.latest(thread).await?;
        if !ended.as_ref().is_some_and(|turn| {
            turn.get("id").and_then(Value::as_str) == Some(id) && terminal(turn)
        }) {
            bail!("Codex turn end unconfirmed; wake prompt was not sent");
        }
    }
    if !still_bound() {
        bail!("Codex binding changed; wake prompt was not sent");
    }
    rpc.call(
        "turn/start",
        json!({"threadId":thread,"input":[{"type":"text","text":hint}]}),
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HINT: &str = "Interrupted for new voice message: command-voice-7f9bc9f9@15";
    use tokio_tungstenite::tungstenite::protocol::Role;

    async fn pair() -> (
        Rpc<tokio::io::DuplexStream>,
        WebSocketStream<tokio::io::DuplexStream>,
    ) {
        let (client, server) = tokio::io::duplex(16384);
        (
            Rpc {
                socket: WebSocketStream::from_raw_socket(client, Role::Client, None).await,
                next_id: 1,
            },
            WebSocketStream::from_raw_socket(server, Role::Server, None).await,
        )
    }
    async fn request(
        server: &mut WebSocketStream<tokio::io::DuplexStream>,
        method: &str,
        result: Value,
    ) -> Value {
        let Message::Text(text) = server.next().await.unwrap().unwrap() else {
            panic!("text request expected");
        };
        let req: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(req["method"], method);
        if let Some(id) = req.get("id") {
            server
                .send(Message::Text(
                    json!({"id":id,"result":result}).to_string().into(),
                ))
                .await
                .unwrap();
        }
        req
    }
    async fn preflight(
        server: &mut WebSocketStream<tokio::io::DuplexStream>,
        cwd: &Path,
        status: &str,
    ) {
        request(server, "initialize", json!({})).await;
        request(server, "initialized", Value::Null).await;
        request(server, "thread/loaded/list", json!({"data":["thread"]})).await;
        request(
            server,
            "thread/read",
            json!({"thread":{"id":"thread","cwd":cwd}}),
        )
        .await;
        let req = request(
            server,
            "thread/turns/list",
            json!({"data":[{"id":"old","status":status}]}),
        )
        .await;
        assert_eq!(req["params"]["limit"], 1);
        assert_eq!(req["params"]["itemsView"], "notLoaded");
    }
    #[tokio::test]
    async fn busy_waits_for_correlated_reply_confirms_once_then_starts_hint() {
        let repo = tempfile::tempdir().unwrap();
        let (mut client, mut server) = pair().await;
        let control = wake(&mut client, repo.path(), "thread", HINT, || true);
        let script = async {
            preflight(&mut server, repo.path(), "inProgress").await;
            // Notifications are not correlated acknowledgement and are ignored.
            server
                .send(Message::Text(
                    json!({"method":"turn/completed","params":{"threadId":"foreign"}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let req = request(&mut server, "turn/interrupt", json!({})).await;
            assert_eq!(req["params"], json!({"threadId":"thread","turnId":"old"}));
            request(
                &mut server,
                "thread/turns/list",
                json!({"data":[{"id":"old","status":"interrupted"}]}),
            )
            .await;
            let req = request(&mut server, "turn/start", json!({"turn":{"id":"new"}})).await;
            assert_eq!(req["params"]["input"], json!([{"type":"text","text":HINT}]));
        };
        let (result, ()) = tokio::join!(control, script);
        assert!(result.unwrap());
    }
    /// Nothing more reaches the server once the client is done.
    async fn assert_silent(server: &mut WebSocketStream<tokio::io::DuplexStream>) {
        match server.next().await {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {}
            Some(Ok(other)) => panic!("unexpected request: {other:?}"),
        }
    }

    #[tokio::test]
    async fn recovery_starts_only_after_the_failed_turn_and_never_interrupts() {
        use crate::cli::recovery::Answer;
        let repo = tempfile::tempdir().unwrap();
        let (mut client, mut server) = pair().await;
        let (answer, ()) = tokio::join!(
            start_after(
                &mut client,
                repo.path(),
                "thread",
                "old",
                "retry wake",
                async { true }
            ),
            async {
                preflight(&mut server, repo.path(), "failed").await;
                let req = request(&mut server, "turn/start", json!({"turn":{"id":"new"}})).await;
                assert_eq!(
                    req["params"]["input"],
                    json!([{"type":"text","text":"retry wake"}])
                );
            }
        );
        assert_eq!(
            answer,
            Answer::Submitted {
                started: Some("new".into())
            }
        );
    }

    #[tokio::test]
    async fn recovery_refuses_anything_but_the_failed_turn_and_sends_nothing() {
        use crate::cli::recovery::Answer;
        for (failed_turn, status, bound) in [
            ("old", "inProgress", true),
            ("old", "completed", true),
            ("old", "interrupted", true),
            ("other", "failed", true),
            ("old", "failed", false),
        ] {
            let want = match bound {
                true => Answer::Refused,
                false => Answer::Unavailable,
            };
            let repo = tempfile::tempdir().unwrap();
            let (mut client, mut server) = pair().await;
            let (answer, ()) = tokio::join!(
                start_after(
                    &mut client,
                    repo.path(),
                    "thread",
                    failed_turn,
                    "wake",
                    async move { bound }
                ),
                preflight(&mut server, repo.path(), status)
            );
            assert_eq!(answer, want, "{failed_turn} {status} bound={bound}");
            drop(client);
            assert_silent(&mut server).await;
        }
    }

    #[tokio::test]
    async fn a_start_that_may_have_landed_is_indeterminate_and_one_that_never_began_is_unavailable()
    {
        use crate::cli::recovery::Answer;
        let repo = tempfile::tempdir().unwrap();
        let (mut client, mut server) = pair().await;
        let (answer, ()) = tokio::join!(
            start_after(&mut client, repo.path(), "thread", "old", "wake", async {
                true
            }),
            async {
                preflight(&mut server, repo.path(), "failed").await;
                let Message::Text(text) = server.next().await.unwrap().unwrap() else {
                    panic!("text request expected");
                };
                let req: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(req["method"], "turn/start");
                server
                    .send(Message::Text(
                        json!({"id":req["id"],"error":{"code":-32000,"message":"busy"}})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        );
        assert_eq!(answer, Answer::Indeterminate);

        let (mut client, mut server) = pair().await;
        let (answer, ()) = tokio::join!(
            start_after(&mut client, repo.path(), "thread", "old", "wake", async {
                true
            }),
            async {
                request(&mut server, "initialize", json!({})).await;
                request(&mut server, "initialized", Value::Null).await;
                request(&mut server, "thread/loaded/list", json!({"data":[]})).await;
            }
        );
        assert_eq!(
            answer,
            Answer::Unavailable,
            "a thread that is not loaded was never touched"
        );
    }

    /// A server that stops answering cannot hold the attempt past its
    /// bound, and which side of the send it went quiet on decides whether
    /// anything may have happened.
    #[tokio::test(start_paused = true)]
    async fn a_silent_server_is_bounded_on_both_sides_of_the_send() {
        use crate::cli::recovery::Answer;
        let repo = tempfile::tempdir().unwrap();
        let (mut client, mut server) = pair().await;
        let (answer, _server) = tokio::join!(
            start_after(&mut client, repo.path(), "thread", "old", "wake", async {
                true
            }),
            async {
                let _ = server.next().await;
                server
            }
        );
        assert_eq!(answer, Answer::Unavailable, "quiet before turn/start");

        let (mut client, mut server) = pair().await;
        let (answer, _server) = tokio::join!(
            start_after(&mut client, repo.path(), "thread", "old", "wake", async {
                true
            }),
            async {
                preflight(&mut server, repo.path(), "failed").await;
                let _ = server.next().await;
                server
            }
        );
        assert_eq!(answer, Answer::Indeterminate, "quiet after turn/start");
    }

    #[tokio::test]
    async fn idle_starts_without_interrupt() {
        let repo = tempfile::tempdir().unwrap();
        let (mut client, mut server) = pair().await;
        let (result, ()) = tokio::join!(
            wake(&mut client, repo.path(), "thread", HINT, || true),
            async {
                preflight(&mut server, repo.path(), "completed").await;
                request(&mut server, "turn/start", json!({})).await;
            }
        );
        assert!(result.unwrap());
    }
    #[tokio::test]
    async fn unconfirmed_or_replaced_turn_never_submits_or_polls() {
        for (id, status) in [
            ("old", "inProgress"),
            ("new", "completed"),
            ("old", "futureStatus"),
        ] {
            let repo = tempfile::tempdir().unwrap();
            let (mut client, mut server) = pair().await;
            let (result, ()) = tokio::join!(
                wake(&mut client, repo.path(), "thread", HINT, || true),
                async {
                    preflight(&mut server, repo.path(), "inProgress").await;
                    request(&mut server, "turn/interrupt", json!({})).await;
                    request(
                        &mut server,
                        "thread/turns/list",
                        json!({"data":[{"id":id,"status":status}]}),
                    )
                    .await;
                }
            );
            assert!(result.unwrap_err().to_string().contains("unconfirmed"));
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), server.next())
                    .await
                    .is_err(),
                "unexpected extra request"
            );
        }
    }
    #[tokio::test]
    async fn foreign_repo_or_unloaded_thread_allows_only_read_only_fallback() {
        for unloaded in [true, false] {
            let repo = tempfile::tempdir().unwrap();
            let foreign = tempfile::tempdir().unwrap();
            let (mut client, mut server) = pair().await;
            let (result, ()) = tokio::join!(
                wake(&mut client, repo.path(), "thread", HINT, || true),
                async {
                    request(&mut server, "initialize", json!({})).await;
                    request(&mut server, "initialized", Value::Null).await;
                    request(
                        &mut server,
                        "thread/loaded/list",
                        json!({"data":if unloaded {vec!["foreign"]} else {vec!["thread"]}}),
                    )
                    .await;
                    if !unloaded {
                        request(
                            &mut server,
                            "thread/read",
                            json!({"thread":{"id":"thread","cwd":foreign.path()}}),
                        )
                        .await;
                    }
                }
            );
            assert!(!result.unwrap());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), server.next())
                    .await
                    .is_err()
            );
        }
    }
    #[tokio::test]
    async fn rebind_before_mutation_falls_back_but_rebind_after_interrupt_is_indeterminate() {
        for after in [false, true] {
            let checks = std::cell::Cell::new(0);
            let repo = tempfile::tempdir().unwrap();
            let (mut client, mut server) = pair().await;
            let bound = || {
                let n = checks.get();
                checks.set(n + 1);
                after && n == 0
            };
            let (result, ()) = tokio::join!(
                wake(&mut client, repo.path(), "thread", HINT, bound),
                async {
                    preflight(&mut server, repo.path(), "inProgress").await;
                    if after {
                        request(&mut server, "turn/interrupt", json!({})).await;
                        request(
                            &mut server,
                            "thread/turns/list",
                            json!({"data":[{"id":"old","status":"interrupted"}]}),
                        )
                        .await;
                    }
                }
            );
            if after {
                assert!(result.unwrap_err().to_string().contains("binding changed"));
            } else {
                assert!(!result.unwrap());
            }
        }
    }
    #[tokio::test]
    async fn native_mutation_error_never_authorizes_terminal_fallback() {
        for busy in [false, true] {
            let repo = tempfile::tempdir().unwrap();
            let (mut client, mut server) = pair().await;
            let (result, ()) = tokio::join!(
                wake(&mut client, repo.path(), "thread", HINT, || true),
                async {
                    preflight(
                        &mut server,
                        repo.path(),
                        if busy { "inProgress" } else { "completed" },
                    )
                    .await;
                    let Message::Text(text) = server.next().await.unwrap().unwrap() else {
                        panic!();
                    };
                    let req: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(
                        req["method"],
                        if busy { "turn/interrupt" } else { "turn/start" }
                    );
                    server
                        .send(Message::Text(
                            json!({"id":req["id"],"error":{"code":-32000,"message":"refused"}})
                                .to_string()
                                .into(),
                        ))
                        .await
                        .unwrap();
                }
            );
            assert!(result.unwrap_err().to_string().contains("rejected"));
        }
    }

    #[tokio::test]
    async fn hanging_interrupt_is_owned_and_drop_closes_connection() {
        let repo = tempfile::tempdir().unwrap();
        let (mut client, mut server) = pair().await;
        let control = async {
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                wake(&mut client, repo.path(), "thread", HINT, || true),
            )
            .await;
            assert!(result.is_err());
            drop(client);
        };
        let script = async {
            preflight(&mut server, repo.path(), "inProgress").await;
            let Message::Text(text) = server.next().await.unwrap().unwrap() else {
                panic!();
            };
            let req: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(req["method"], "turn/interrupt");
            // No reply: cancellation must drop the owned transport, not leave a
            // reader/request running or send a second prompt after timeout.
            assert!(server.next().await.is_none_or(|frame| frame.is_err()));
        };
        tokio::join!(control, script);
    }
    /// Through the published path, whether it is the socket itself or a
    /// link to one elsewhere, as Codex 0.160 publishes it.
    #[tokio::test]
    async fn bound_profile_socket_drives_real_transport_without_zellij() {
        bound_profile_socket_drives(false).await;
        bound_profile_socket_drives(true).await;
    }

    async fn bound_profile_socket_drives(linked: bool) {
        use super::super::teams_config::{AgentDescription, RosterRole};
        use clank_core::{agent_config::LaunchConfig, ids::SessionId};
        let repo = tempfile::tempdir_in("/tmp").unwrap();
        let profile = tempfile::tempdir_in("/tmp").unwrap();
        let mut launch = LaunchConfig::default();
        launch
            .env
            .insert("CODEX_HOME".into(), profile.path().to_str().unwrap().into());
        let label = AgentLabel::parse("native-test").unwrap();
        super::super::agent::add_repo_roster_agent(
            repo.path(),
            &label,
            AgentDescription {
                tool: Tool::Codex,
                launch: Some(launch.clone()),
                initial_prompt: None,
            },
            RosterRole::Master,
        )
        .unwrap();
        crate::agent_store::bind_session_to_agent(
            repo.path(),
            &label,
            Tool::Codex,
            &SessionId::parse("thread-a1").unwrap(),
        )
        .unwrap();
        let socket = profile_socket(Some(&launch), Path::new("/irrelevant")).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let daemon = tempfile::tempdir_in("/tmp").unwrap();
        let listener = if linked {
            let real = daemon.path().join("control.sock");
            let listener = tokio::net::UnixListener::bind(&real).unwrap();
            std::os::unix::fs::symlink(&real, &socket).unwrap();
            listener
        } else {
            tokio::net::UnixListener::bind(socket).unwrap()
        };
        let script = async {
            let (stream, _) = listener.accept().await.unwrap();
            let mut server = tokio_tungstenite::accept_async(stream).await.unwrap();
            // The typed script below has a DuplexStream parameter; the actual
            // socket path exercises handshake, profile and binding selection.
            for (method, result) in [
                ("initialize", json!({})),
                ("initialized", Value::Null),
                ("thread/loaded/list", json!({"data":["thread-a1"]})),
                (
                    "thread/read",
                    json!({"thread":{"id":"thread-a1","cwd":repo.path()}}),
                ),
                (
                    "thread/turns/list",
                    json!({"data":[{"id":"old","status":"completed"}]}),
                ),
                ("turn/start", json!({})),
            ] {
                let Message::Text(text) = server.next().await.unwrap().unwrap() else {
                    panic!();
                };
                let req: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(req["method"], method);
                if let Some(id) = req.get("id") {
                    server
                        .send(Message::Text(
                            json!({"id":id,"result":result}).to_string().into(),
                        ))
                        .await
                        .unwrap();
                }
            }
        };
        // A refused target never connects, and the script would wait for
        // it forever.
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let hint = crate::cli::pane_input::Admitted::new(Tool::Codex, None, HINT);
            tokio::join!(try_wake(repo.path(), &label, &hint), script)
        })
        .await
        .unwrap_or_else(|_| panic!("the wake never reached the socket (linked: {linked})"));
        assert!(result.unwrap());
    }

    #[test]
    fn only_a_socket_this_user_owns_is_used_wherever_its_link_leads() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let me = unsafe { libc::geteuid() };
        let real = dir.path().join("real.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&real).unwrap();
        let real = std::fs::canonicalize(&real).unwrap();
        let link = dir.path().join("published.sock");
        symlink(&real, &link).unwrap();
        let chained = dir.path().join("chained.sock");
        symlink(&link, &chained).unwrap();
        assert_eq!(own_socket(&real, me), Some(real.clone()));
        assert_eq!(own_socket(&link, me), Some(real.clone()), "a link to it");
        assert_eq!(
            own_socket(&chained, me),
            Some(real.clone()),
            "a link to a link"
        );

        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let to_file = dir.path().join("to-file.sock");
        symlink(&file, &to_file).unwrap();
        assert_eq!(own_socket(&to_file, me), None, "a link to a regular file");
        let dangling = dir.path().join("dangling.sock");
        symlink(dir.path().join("gone"), &dangling).unwrap();
        assert_eq!(own_socket(&dangling, me), None, "a dangling link");
        assert_eq!(own_socket(&link, me + 1), None, "another user's socket");
    }

    #[test]
    fn profile_is_target_selected_and_relative_roots_are_not_guessed() {
        use clank_core::agent_config::LaunchConfig;
        let mut launch = LaunchConfig::default();
        launch
            .env
            .insert("CODEX_HOME".into(), "/target/codex".into());
        assert_eq!(
            profile_socket(Some(&launch), Path::new("/sender/home")).unwrap(),
            Path::new("/target/codex/app-server-control/app-server-control.sock")
        );
        launch.env.insert("CODEX_HOME".into(), "relative".into());
        assert!(profile_socket(Some(&launch), Path::new("/sender/home")).is_none());
        launch.env.remove("CODEX_HOME");
        launch.env.insert("HOME".into(), "/target/home".into());
        assert!(
            profile_socket(Some(&launch), Path::new("/sender/home"))
                .unwrap()
                .starts_with("/target/home/.codex")
        );
    }
}
