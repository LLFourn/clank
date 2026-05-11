use std::path::PathBuf;
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

use crate::domain::AgentRole;
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
    // We don't use a pidfile: the daemon's bound TCP port is a natural
    // atomic lock — only one process can hold 7777 at a time, and it's
    // freed automatically on exit. Stale pidfiles are no problem because
    // there are no pidfiles.
    if !probe_daemon(&client, &daemon_url).await {
        if is_loopback_url(&daemon_url) {
            if let Err(e) = spawn_daemon_detached(&daemon_url).await {
                tracing::warn!(error = %e, "could not auto-spawn daemon; tool calls will fail until the user runs `trinity serve`");
            } else if let Err(e) =
                wait_for_daemon(&client, &daemon_url, Duration::from_secs(10)).await
            {
                tracing::warn!(error = %e, "spawned daemon but it did not become healthy in time");
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
        binding: Arc::new(Mutex::new(None)),
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

/// Launch `trinity serve` so it survives the shim's exit. On Unix we put
/// the daemon in its own session via `setsid(2)` so a SIGHUP to the
/// shim's process group (e.g. terminal close) does not propagate.
async fn spawn_daemon_detached(daemon: &str) -> anyhow::Result<()> {
    use std::fs::OpenOptions;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let log_dir = trinity_home()?;
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join("daemon.log");
    let log_out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let log_err = log_out.try_clone()?;

    tracing::info!(exe = %exe.display(), log = %log_path.display(), "auto-spawning trinity serve");

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

    let _child = cmd.spawn()?;
    // Intentionally drop the Child handle: we never want to wait() on it.
    let _ = daemon; // (only used for logging context above)
    Ok(())
}

// Direct FFI to libc::setsid. Avoids pulling in the `libc` crate for a
// single call site.
unsafe extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
}

fn trinity_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set; cannot locate ~/.trinity"))?;
    Ok(PathBuf::from(home).join(".trinity"))
}

/// Identity the shim has cached after a successful register/join. The shim
/// refuses to forward tool calls whose `session_id` argument doesn't match
/// `session_id` here — clients can't accidentally cross-pollinate two
/// sessions from one shell.
#[derive(Debug, Clone)]
struct BoundAgent {
    session_id: String,
    label: String,
    role: AgentRole,
}

#[derive(Clone)]
struct ShimHandler {
    daemon: String,
    cwd: PathBuf,
    client: reqwest::Client,
    descriptors: Vec<ToolDescriptor>,
    binding: Arc<Mutex<Option<BoundAgent>>>,
}

#[derive(Deserialize)]
struct ToolCallEnvelope {
    result: Value,
}

impl ShimHandler {
    fn current_binding(&self) -> Option<BoundAgent> {
        self.binding.lock().unwrap().clone()
    }

    fn set_binding(&self, binding: BoundAgent) {
        *self.binding.lock().unwrap() = Some(binding);
    }

    /// Returns Err with a user-facing message if the tool argument's
    /// `session_id` (when present) conflicts with the shim's cached binding.
    /// The daemon also does its own `(session_id, label)` check; this is a
    /// client-side guard that prevents accidental cross-session calls.
    fn check_session_consistency(&self, tool: &str, arguments: &Value) -> Result<(), String> {
        // Tools that may legitimately be called before binding or that operate
        // without a session_id: list_sessions, echo_cwd. Bind-creating tools
        // (register_plan_file, join_session) require no pre-existing binding —
        // we keep the model 1 shell = 1 session.
        let arg_session_id = arguments.get("session_id").and_then(|v| v.as_str());
        let bound = self.current_binding();
        match (tool, &bound) {
            ("register_plan_file" | "join_session", Some(b)) => Err(format!(
                "this shell is already bound to session `{}` as {} `{}`; one shell binds to one \
                 (session, role). Open a new terminal to act on a different session.",
                b.session_id,
                b.role.as_str(),
                b.label,
            )),
            ("list_sessions" | "echo_cwd", _) => Ok(()),
            (_, None) => Ok(()),
            (_, Some(b)) => {
                if let Some(arg_id) = arg_session_id
                    && arg_id != b.session_id
                {
                    return Err(format!(
                        "tool `{tool}` was called with session_id `{arg_id}` but this shell is \
                         bound to `{}`; refusing to forward. Use the bound session or open a new \
                         terminal.",
                        b.session_id
                    ));
                }
                Ok(())
            }
        }
    }

    async fn forward(&self, tool: &str, arguments: Value) -> Result<Value, ShimError> {
        let label = self.current_binding().map(|b| b.label);
        let body = serde_json::json!({
            "cwd": self.cwd,
            "label": label,
            "tool": tool,
            "arguments": arguments,
        });
        let resp = self
            .client
            .post(format!("{}/internal/tool_call", self.daemon))
            .header("origin", "http://127.0.0.1")
            .json(&body)
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
                "Trinity coordinates multi-agent peer review around watched plan files \
                 and registered git commits. Start by calling `list_sessions` (reviewer) \
                 or `register_plan_file` (master). After register_plan_file or \
                 join_session, this shim is bound to a (session_id, label, role) for the \
                 lifetime of the MCP connection — calls with a different session_id are \
                 refused. To act on a different session, open a new terminal."
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

        if let Err(msg) = self.check_session_consistency(&tool, &arguments) {
            return Ok(CallToolResult::error(vec![Content::text(msg)]));
        }

        match self.forward(&tool, arguments.clone()).await {
            Ok(result) => {
                // Bind on successful register_plan_file / join_session.
                let role_for_tool = match tool.as_str() {
                    "register_plan_file" => Some(AgentRole::Master),
                    "join_session" => Some(AgentRole::Reviewer),
                    _ => None,
                };
                if let Some(role) = role_for_tool
                    && let (Some(label), Some(session_id)) = (
                        arguments.get("label").and_then(|v| v.as_str()),
                        arguments.get("session_id").and_then(|v| v.as_str()),
                    )
                    && result.get("session_id").is_some()
                {
                    self.set_binding(BoundAgent {
                        session_id: session_id.to_string(),
                        label: label.to_string(),
                        role,
                    });
                }
                Ok(CallToolResult::structured(result))
            }
            Err(err) => Ok(err.into_call_tool_result()),
        }
    }
}
