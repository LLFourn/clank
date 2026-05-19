use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Args;
use rmcp::ServerHandler;
use rmcp::ServiceExt;
use rmcp::model::{
    CallToolRequestParam, CallToolResult, Content, ErrorData as McpError, Implementation,
    InitializeRequestParam, InitializeResult, ListToolsResult, PaginatedRequestParam,
    ProtocolVersion, ServerCapabilities, ServerInfo, Tool, ToolsCapability,
};
use rmcp::service::RequestContext;
use rmcp::transport::io::stdio;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::tools::ToolDescriptor;

#[derive(Args, Debug, Clone)]
pub struct McpArgs {
    /// Daemon HTTP base URL.
    #[arg(long, default_value = "http://127.0.0.1:7777", env = "TRINITY_DAEMON")]
    pub daemon: String,
}

pub async fn run(args: McpArgs) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let daemon_url = args.daemon.trim_end_matches('/').to_string();
    tracing::info!(daemon = %daemon_url, cwd = %cwd.display(), "trinity mcp shim starting");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    // Tool catalog is statically known (shim and daemon ship from the same
    // crate), so the shim can answer `tools/list` even if the daemon
    // happens to be down. The daemon only needs to be up by the time a
    // tool call actually arrives.
    let descriptors: Vec<ToolDescriptor> = crate::tools::catalog();

    // If the daemon isn't reachable and the URL points at the local
    // machine, try to spawn `trinity serve` ourselves (fully detached).
    // The port is not enough as a spawn lock: several shims can all
    // observe "down" before any spawned child has bound the socket.
    if !probe_daemon(&client, &daemon_url).await {
        if is_loopback_url(&daemon_url) {
            if let Err(e) = ensure_loopback_daemon(&client, &daemon_url).await {
                tracing::warn!(error = %e, "could not auto-spawn daemon; tool calls will fail until the user runs `trinity serve`");
            }
        } else {
            tracing::warn!(
                daemon = %daemon_url,
                "daemon unreachable and URL is not loopback; not auto-spawning"
            );
        }
    }

    let handler = ShimHandler {
        daemon: daemon_url,
        cwd,
        client,
        descriptors,
        cached_label: Arc::new(Mutex::new(None)),
    };

    let service = handler.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

async fn probe_daemon(client: &reqwest::Client, daemon: &str) -> bool {
    let url = format!("{}/healthz", daemon);
    match tokio::time::timeout(Duration::from_millis(500), client.get(url).send()).await {
        Ok(Ok(resp)) => resp.status().is_success(),
        _ => false,
    }
}

async fn wait_for_daemon(
    client: &reqwest::Client,
    daemon: &str,
    max: Duration,
) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + max;
    while std::time::Instant::now() < deadline {
        if probe_daemon(client, daemon).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("daemon did not respond within {:?}", max)
}

fn is_loopback_url(daemon: &str) -> bool {
    let lower = daemon.to_ascii_lowercase();
    let stripped = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .unwrap_or(&lower);
    let host = stripped.split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "::1")
}

async fn ensure_loopback_daemon(client: &reqwest::Client, daemon: &str) -> anyhow::Result<()> {
    let log_dir = trinity_home()?;
    std::fs::create_dir_all(&log_dir)?;

    let lock_path = log_dir.join("daemon.spawn.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;

    if !try_flock_exclusive_nb(&lock_file)? {
        wait_for_daemon(client, daemon, Duration::from_secs(15))
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "another shim held {}; daemon still unhealthy: {e}",
                    lock_path.display()
                )
            })?;
        return Ok(());
    }

    if probe_daemon(client, daemon).await {
        return Ok(());
    }

    spawn_daemon_detached(daemon, &log_dir).await?;
    wait_for_daemon(client, daemon, Duration::from_secs(10))
        .await
        .map_err(|e| anyhow::anyhow!("spawned daemon but it did not become healthy in time: {e}"))
}

fn try_flock_exclusive_nb(file: &File) -> io::Result<bool> {
    let rc = unsafe { libc_flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }

    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Launch `trinity serve` so it survives the shim's exit. On Unix we put
/// the daemon in its own session via `setsid(2)` so a SIGHUP to the
/// shim's process group (e.g. terminal close) does not propagate.
async fn spawn_daemon_detached(daemon: &str, log_dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let log_path = log_dir.join("daemon.log");
    let log_out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_out.try_clone()?;

    tracing::info!(daemon = %daemon, exe = %exe.display(), log = %log_path.display(), "auto-spawning trinity serve");

    let mut cmd = Command::new(&exe);
    cmd.arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err));

    // Detach from the shim's session/process group. Stable Rust has no
    // safe API for this, so we call setsid(2) via pre_exec.
    unsafe {
        cmd.pre_exec(|| {
            // setsid is signal-safe and async-signal-safe; legal in pre_exec.
            let rc = libc_setsid();
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;
    let pid = child.id();
    // `setsid` does not change PID. If exec fails after fork this sidecar
    // may point at a short-lived process; it is informational only.
    if let Err(e) = std::fs::write(log_dir.join("daemon.spawn.last_pid"), format!("{pid}\n")) {
        tracing::warn!(error = %e, pid, "could not write daemon autospawn pid sidecar");
    }
    // Intentionally drop the Child handle: we never want to wait() on it.
    Ok(())
}

const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

// Direct FFI avoids pulling in the `libc` crate for two small Unix calls.
unsafe extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
    #[link_name = "flock"]
    fn libc_flock(fd: i32, operation: i32) -> i32;
}

fn trinity_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set; cannot locate ~/.trinity"))?;
    Ok(PathBuf::from(home).join(".trinity"))
}

#[derive(Clone)]
struct ShimHandler {
    daemon: String,
    cwd: PathBuf,
    client: reqwest::Client,
    descriptors: Vec<ToolDescriptor>,
    /// Last label observed flowing through any tool call (caller-supplied
    /// `label` or `author_label` argument). Used as a per-tool fallback so
    /// the agent doesn't have to repeat `label` on every call. Not a
    /// session binding — cross-session calls are not refused.
    cached_label: Arc<Mutex<Option<String>>>,
}

#[derive(Deserialize)]
struct ToolCallEnvelope {
    result: Value,
}

/// Which argument key (if any) holds the agent's label for this tool.
/// Used both for autofill (cache → arguments) and for cache update
/// (arguments → cache).
fn label_arg_for(tool: &str) -> Option<&'static str> {
    match tool {
        "start_plan" => Some("label"),
        "work_context" => Some("author_label"),
        "set_active_work" => Some("author_label"),
        "clear_active_work" => Some("author_label"),
        "wait_for_work" => Some("author_label"),
        _ => None,
    }
}

/// Trim a raw label and return it only if non-empty. Used by both the
/// cache-update path (don't poison the cache with blanks) and the
/// autofill path (don't propagate cached blanks). Keeps the cache from
/// becoming a vector for empty/whitespace strings that the daemon would
/// reject downstream.
///
/// Normalizes ASCII-style whitespace per `char::is_whitespace` (so
/// spaces, tabs, newlines, U+00A0, U+2007, etc. are stripped). It does
/// NOT strip zero-width or bidi formatting characters (U+200B, U+200E,
/// U+200F, …) — those would still produce a non-empty label. Trusted-
/// agent context makes that acceptable; documented here so future
/// reviewers can tighten if the threat model changes.
fn normalize_label(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

impl ShimHandler {
    fn cached_label(&self) -> Option<String> {
        self.cached_label.lock().unwrap().clone()
    }

    fn set_cached_label(&self, label: String) {
        *self.cached_label.lock().unwrap() = Some(label);
    }

    /// Merge the cached label into `arguments[key]` if the caller
    /// omitted it OR supplied a whitespace-only value. Caller-supplied
    /// non-blank values pass through unchanged. Tools with no label
    /// argument pass through unchanged. Blanks in the cache are not
    /// propagated — `normalize_label` rejects them.
    fn fill_label_from_cache(&self, tool: &str, mut arguments: Value) -> Value {
        let Some(key) = label_arg_for(tool) else {
            return arguments;
        };
        if let Some(s) = arguments.get(key).and_then(|v| v.as_str())
            && !s.trim().is_empty()
        {
            return arguments;
        }
        let Some(label) = self.cached_label().and_then(|s| normalize_label(&s)) else {
            return arguments;
        };
        if let Value::Object(ref mut map) = arguments {
            map.insert(key.to_string(), Value::String(label));
        }
        arguments
    }

    async fn forward(&self, tool: &str, arguments: Value) -> Result<Value, ShimError> {
        let body = serde_json::json!({
            "cwd": self.cwd,
            "tool": tool,
            "arguments": arguments,
        });
        let mut req = self
            .client
            .post(format!("{}/internal/tool_call", self.daemon))
            .header("origin", "http://127.0.0.1")
            .json(&body);
        // `wait_for_work` blocks up to `timeout_secs` (default 1800,
        // no upper cap) server-side. Override the per-request timeout
        // so the shim doesn't cut the connection before the daemon
        // answers. Server timeout + 30s grace.
        if tool == "wait_for_work" {
            let server_timeout_secs = arguments
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(1800)
                .max(1);
            req = req.timeout(Duration::from_secs(server_timeout_secs + 30));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ShimError::Transport(e.to_string()))?;

        if resp.status().is_success() {
            let env: ToolCallEnvelope = resp
                .json()
                .await
                .map_err(|e| ShimError::Transport(e.to_string()))?;
            Ok(env.result)
        } else {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            Err(ShimError::Daemon { status, text })
        }
    }
}

enum ShimError {
    Transport(String),
    Daemon {
        status: reqwest::StatusCode,
        text: String,
    },
}

impl ShimError {
    fn into_call_tool_result(self) -> CallToolResult {
        let msg = match self {
            ShimError::Transport(s) => format!("trinity daemon unreachable: {s}"),
            ShimError::Daemon { status, text } => format!("trinity daemon error {status}: {text}"),
        };
        CallToolResult::error(vec![Content::text(msg)])
    }
}

impl ServerHandler for ShimHandler {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::default(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: Some(false),
                }),
                ..Default::default()
            },
            server_info: Implementation {
                name: "trinity".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: Some("Trinity MCP shim".into()),
                website_url: None,
                icons: None,
            },
            instructions: Some(
                "Trinity coordinates multi-agent peer review around plan files committed \
                 to git and feedback files in the working tree. \n\n\
                 Discover plans with `list_plans`. Register a repo with `watch_repo` (or \
                 `start_plan` if you also want to create a new plan file at the same time). \
                 To drive an agent loop, call `wait_for_work({role: \"master\" | \"reviewers\"})` — \
                 it blocks until your role has work and returns minimal identifiers; for \
                 each match, follow up with `work_context()` for the narrow coordination \
                 view. This avoids polling. \n\n\
                 `plan_id` is the canonical `<repo_basename>/<stem>.md` identity (e.g. \
                 `trinity/leptos-frontend.md`) but is OPTIONAL on `wait_for_work` and \
                 `work_context`. Omit it when the cwd-repo has exactly one active visible \
                 plan; the daemon infers the target. When multiple plans are active, the \
                 daemon returns an `ambiguous_plan` error with candidates — call \
                 `set_active_work({plan_id, author_label})` once to pick one and subsequent \
                 inferred calls route to it. `clear_active_work({author_label})` drops the \
                 selection. Pass an explicit `plan_id` only when you intentionally want to \
                 override the inferred / selected target. \n\n\
                 The shim caches the last `label` / `author_label` you passed so subsequent \
                 calls don't need to repeat it. `plan_id` is NEVER cached."
                    .into(),
            ),
        }
    }

    async fn initialize(
        &self,
        request: InitializeRequestParam,
        context: RequestContext<rmcp::service::RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let tools = self
            .descriptors
            .iter()
            .map(|d| {
                let schema_obj: Map<String, Value> = match &d.input_schema {
                    Value::Object(m) => m.clone(),
                    _ => Map::new(),
                };
                Tool::new(d.name.clone(), d.description.clone(), Arc::new(schema_obj))
            })
            .collect();
        Ok(ListToolsResult {
            tools,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParam,
        _context: RequestContext<rmcp::service::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let tool = request.name.to_string();
        let arguments = request.arguments.map(Value::Object).unwrap_or(Value::Null);

        // Per-tool fallback: if the caller didn't supply `label` /
        // `author_label` for a tool that takes one, fill in the last value
        // we saw. No cross-session refusal.
        let filled = self.fill_label_from_cache(&tool, arguments);

        // Pull the label out before `forward` moves `filled`. `normalize_label`
        // keeps blanks / whitespace-only values out of the cache so they
        // can't poison later autofills.
        let label_to_cache = label_arg_for(&tool)
            .and_then(|key| filled.get(key).and_then(|v| v.as_str()))
            .and_then(normalize_label);

        match self.forward(&tool, filled).await {
            Ok(result) => {
                if let Some(label) = label_to_cache {
                    self.set_cached_label(label);
                }
                Ok(CallToolResult::structured(result))
            }
            Err(err) => Ok(err.into_call_tool_result()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_label_strips_whitespace() {
        assert_eq!(normalize_label("  codex  "), Some("codex".to_string()));
    }

    #[test]
    fn normalize_label_rejects_empty() {
        assert_eq!(normalize_label(""), None);
    }

    #[test]
    fn normalize_label_rejects_whitespace_only() {
        assert_eq!(normalize_label("   "), None);
        assert_eq!(normalize_label("\t\n"), None);
    }

    fn handler_with_cache(cached: Option<&str>) -> ShimHandler {
        ShimHandler {
            daemon: "http://127.0.0.1:0".to_string(),
            cwd: std::path::PathBuf::from("/tmp"),
            client: reqwest::Client::new(),
            descriptors: Vec::new(),
            cached_label: Arc::new(Mutex::new(cached.map(String::from))),
        }
    }

    #[test]
    fn fill_label_from_cache_ignores_blank_cache() {
        let h = handler_with_cache(Some("   "));
        let out = h.fill_label_from_cache(
            "wait_for_work",
            serde_json::json!({"role": "reviewers", "plan_id": "repo/s.md"}),
        );
        // Blank cache must not poison autofill.
        assert!(out.get("author_label").is_none());
    }

    #[test]
    fn fill_label_from_cache_treats_blank_caller_arg_as_missing() {
        let h = handler_with_cache(Some("codex"));
        let out = h.fill_label_from_cache(
            "wait_for_work",
            serde_json::json!({"role": "reviewers", "plan_id": "repo/s.md", "author_label": "   "}),
        );
        // Whitespace-only caller-supplied label is overwritten with cache.
        assert_eq!(out["author_label"], "codex");
    }

    #[test]
    fn fill_label_from_cache_passes_real_value_through() {
        let h = handler_with_cache(Some("codex"));
        let out = h.fill_label_from_cache(
            "wait_for_work",
            serde_json::json!({"role": "reviewers", "plan_id": "repo/s.md", "author_label": "alice"}),
        );
        // Caller-supplied non-blank label is preserved as-is.
        assert_eq!(out["author_label"], "alice");
    }

    #[test]
    fn fill_label_from_cache_skips_tools_without_label_arg() {
        let h = handler_with_cache(Some("codex"));
        let out = h.fill_label_from_cache("list_plans", serde_json::json!({"repo": "/r"}));
        assert!(out.get("author_label").is_none());
        assert!(out.get("label").is_none());
    }

    #[test]
    fn fill_label_from_cache_does_not_autofill_plan_id() {
        // plan_id is the target of every canonical call. The shim must
        // never cache or autofill it, even if a cached label exists for
        // the same tool — see plan-path-identity §4b.
        let h = handler_with_cache(Some("codex"));
        let out = h.fill_label_from_cache(
            "wait_for_work",
            serde_json::json!({"role": "reviewers", "author_label": "codex"}),
        );
        assert!(
            out.get("plan_id").is_none(),
            "shim must never autofill plan_id"
        );
    }
}
