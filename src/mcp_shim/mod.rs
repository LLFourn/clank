use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    tracing::info!(daemon = %args.daemon, cwd = %cwd.display(), "trinity mcp shim starting");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    // Snapshot the tool catalog from the daemon at startup.
    let descriptors: Vec<ToolDescriptor> = client
        .get(format!(
            "{}/internal/tools",
            args.daemon.trim_end_matches('/')
        ))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let handler = ShimHandler {
        daemon: args.daemon.trim_end_matches('/').to_string(),
        cwd,
        client,
        descriptors,
        binding: Arc::new(Mutex::new(None)),
    };

    let service = handler.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// Identity that the shim has cached after a successful register/join. The
/// shim refuses to forward tool calls whose `plan_id` argument doesn't match
/// `plan_id` here — clients can't accidentally cross-pollinate two plans from
/// one shell.
#[derive(Debug, Clone)]
struct BoundAgent {
    plan_id: String,
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

    /// Returns Err with a user-facing message if the tool argument's `plan_id`
    /// (when present) conflicts with the shim's cached binding. Identification
    /// of the bound plan_id is the shim's job; the daemon does its own
    /// (plan_id, label) check independently.
    fn check_plan_consistency(&self, tool: &str, arguments: &Value) -> Result<(), String> {
        // Tools that may legitimately be called before binding or that
        // operate without a plan_id arg: list_plans, echo_cwd. Bind-creating
        // tools (register_plan_file, join_plan) take plan_id_or_path, not
        // plan_id, so they're allowed to specify a different plan than any
        // pre-existing (stale) binding — but we still reject if a binding
        // already exists, to keep the model 1 shell = 1 plan.
        let arg_plan_id = arguments.get("plan_id").and_then(|v| v.as_str());
        let bound = self.current_binding();
        match (tool, &bound) {
            ("register_plan_file" | "join_plan", Some(b)) => Err(format!(
                "this shell is already bound to plan {} as {} `{}`; one shell binds to one (plan, role). \
                     Open a new terminal to act on a different plan.",
                b.plan_id,
                b.role.as_str(),
                b.label,
            )),
            ("list_plans" | "echo_cwd", _) => Ok(()),
            (_, None) => {
                // Most tools require a binding. Let the daemon return the
                // canonical "register/join first" error; the shim won't try
                // to second-guess it here.
                Ok(())
            }
            (_, Some(b)) => {
                if let Some(arg_id) = arg_plan_id
                    && arg_id != b.plan_id
                {
                    return Err(format!(
                        "tool `{tool}` was called with plan_id `{arg_id}` but this shell is bound \
                         to plan `{}`; refusing to forward. Use the bound plan or open a new terminal.",
                        b.plan_id
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
                 and registered git commits. Start by calling `list_plans` (reviewer) \
                 or `register_plan_file` (master). Once you have called register/join \
                 you are bound to a (plan, label, role) for the lifetime of this MCP \
                 connection. To act on a different plan, open a new terminal."
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

        if let Err(msg) = self.check_plan_consistency(&tool, &arguments) {
            return Ok(CallToolResult::error(vec![Content::text(msg)]));
        }

        match self.forward(&tool, arguments.clone()).await {
            Ok(result) => {
                // Bind on successful register/join.
                let role_for_tool = match tool.as_str() {
                    "register_plan_file" => Some(AgentRole::Master),
                    "join_plan" => Some(AgentRole::Reviewer),
                    _ => None,
                };
                if let Some(role) = role_for_tool
                    && let (Some(label), Some(plan_id)) = (
                        arguments.get("label").and_then(|v| v.as_str()),
                        result.get("plan_id").and_then(|v| v.as_str()),
                    )
                {
                    self.set_binding(BoundAgent {
                        plan_id: plan_id.to_string(),
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
