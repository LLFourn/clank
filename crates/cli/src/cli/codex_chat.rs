//! Submit remote chat directly to a live Codex TUI's app-server thread.
//!
//! Codex's synchronous Stop hook leaves a turn in progress even after
//! its final answer. Terminal Enter queues behind that hook. The
//! app-server gives us the last turn's phase, so we can interrupt only
//! a turn that has already produced its final answer, then start the
//! user's next turn. An actual working turn receives a steer instead.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::{Message, WebSocket, client};

type Socket = WebSocket<UnixStream>;

fn control_socket() -> Option<PathBuf> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
    Some(home.join("app-server-control/app-server-control.sock"))
}

fn connect() -> Option<Socket> {
    let stream = UnixStream::connect(control_socket()?).ok()?;
    let timeout = Some(Duration::from_secs(3));
    stream.set_read_timeout(timeout).ok()?;
    stream.set_write_timeout(timeout).ok()?;
    client("ws://localhost/", stream)
        .ok()
        .map(|(socket, _)| socket)
}

struct Rpc {
    socket: Socket,
    next_id: u64,
}

impl Rpc {
    fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.socket
            .send(Message::Text(
                json!({"id": id, "method": method, "params": params})
                    .to_string()
                    .into(),
            ))
            .with_context(|| format!("sending Codex {method}"))?;
        loop {
            let frame = self
                .socket
                .read()
                .with_context(|| format!("reading Codex {method}"))?;
            let Message::Text(text) = frame else { continue };
            let reply: Value = serde_json::from_str(&text)?;
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                bail!("Codex {method}: {error}");
            }
            return reply
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("Codex {method} returned no result"));
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        self.socket.send(Message::Text(
            json!({"method": method, "params": params})
                .to_string()
                .into(),
        ))?;
        Ok(())
    }

    fn latest_turn(&mut self, thread: &str) -> anyhow::Result<Option<Value>> {
        let result = self.call(
            "thread/turns/list",
            json!({"threadId": thread, "limit": 1, "sortDirection": "desc", "itemsView": "summary"}),
        )?;
        Ok(result
            .get("data")
            .and_then(Value::as_array)
            .and_then(|turns| turns.first())
            .cloned())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Route {
    Start,
    Steer(String),
    Release(String),
}

fn route(last: Option<&Value>) -> anyhow::Result<Route> {
    let Some(turn) = last else {
        return Ok(Route::Start);
    };
    if turn.get("status").and_then(Value::as_str) != Some("inProgress") {
        return Ok(Route::Start);
    }
    let id = turn
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Codex active turn has no id"))?
        .to_owned();
    let finished_answer = turn
        .get("items")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("agentMessage")
                    && item.get("phase").and_then(Value::as_str) == Some("final_answer")
            })
        });
    Ok(if finished_answer {
        Route::Release(id)
    } else {
        Route::Steer(id)
    })
}

/// True when the app-server accepted the message. False when this
/// Codex installation has no reachable local app-server, so the
/// caller should use the terminal fallback. Once a mutation is sent,
/// an error is reported instead of risking a duplicate fallback send.
pub(crate) fn send(thread: &str, text: &super::pane_input::Admitted) -> anyhow::Result<bool> {
    let text = text.as_str();
    if text.is_empty() {
        return Ok(true);
    }
    let Some(socket) = connect() else {
        return Ok(false);
    };
    let mut rpc = Rpc { socket, next_id: 1 };
    if rpc
        .call(
            "initialize",
            json!({
                "clientInfo": {"name": "clank", "title": "Clank", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true}
            }),
        )
        .is_err()
    {
        return Ok(false);
    }
    rpc.notify("initialized", json!({}))?;
    // A control socket can belong to a different app-server than the
    // pane's TUI. Never load a persisted thread into that other server.
    let loaded = match rpc.call("thread/loaded/list", json!({})) {
        Ok(loaded) => loaded,
        Err(_) => return Ok(false),
    };
    if !loaded
        .get("data")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(thread)))
    {
        return Ok(false);
    }
    let last = match rpc.latest_turn(thread) {
        Ok(last) => last,
        Err(_) => return Ok(false),
    };
    let input = json!([{"type": "text", "text": text}]);
    match route(last.as_ref())? {
        Route::Start => {
            rpc.call("turn/start", json!({"threadId": thread, "input": input}))?;
        }
        Route::Steer(turn) => {
            rpc.call(
                "turn/steer",
                json!({"threadId": thread, "input": input, "expectedTurnId": turn}),
            )?;
        }
        Route::Release(turn) => {
            rpc.call(
                "turn/interrupt",
                json!({"threadId": thread, "turnId": turn}),
            )?;
            // The interrupt RPC acknowledges a request, not the
            // completed turn. Starting immediately can silently attach
            // the prompt to the turn being cancelled.
            let mut released = false;
            let waiting_since = std::time::Instant::now();
            while waiting_since.elapsed() < Duration::from_secs(3) {
                if rpc.latest_turn(thread)?.as_ref().is_some_and(|last| {
                    last.get("id").and_then(Value::as_str) == Some(turn.as_str())
                        && last.get("status").and_then(Value::as_str) != Some("inProgress")
                }) {
                    released = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !released {
                bail!("Codex did not release its completed Stop-hook turn; prompt was not sent");
            }
            rpc.call("turn/start", json!({"threadId": thread, "input": input}))?;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_final_answer_in_an_active_turn_is_safe_to_release() {
        let last = json!({"id":"old", "status":"inProgress", "items":[
            {"type":"userMessage"}, {"type":"agentMessage", "phase":"final_answer"}
        ]});
        assert_eq!(route(Some(&last)).unwrap(), Route::Release("old".into()));
    }

    #[test]
    fn an_unfinished_turn_is_steered_and_an_idle_thread_is_started() {
        let working = json!({"id":"current", "status":"inProgress", "items":[
            {"type":"agentMessage", "phase":"commentary"}
        ]});
        assert_eq!(
            route(Some(&working)).unwrap(),
            Route::Steer("current".into())
        );
        assert_eq!(route(None).unwrap(), Route::Start);
        assert_eq!(
            route(Some(&json!({"status":"completed"}))).unwrap(),
            Route::Start
        );
    }
}
