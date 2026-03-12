//! MCP stdio server for agent-orchestrator tool calls.
//!
//! Spawned as a subprocess by Claude CLI via --mcp-config.
//! Connects to the orchestrator relay via Unix socket and exposes
//! structured communication tools to agents.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::relay::{RelayRequest, RelayResponse};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SendMessageParams {
    /// Target agent name (e.g. "merger", "runtime")
    to: String,
    /// Message kind (e.g. "task_complete", "task_blocked")
    kind: String,
    /// Message content
    content: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ListTasksParams {
    /// Filter by status: pending, ready, in_progress, needs_info, in_review, completed
    status: Option<String>,
    /// Filter by assignee
    assignee: Option<String>,
}

struct BufStream {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

struct RelayClient {
    stream: Mutex<BufStream>,
}

impl RelayClient {
    async fn connect(socket_path: &std::path::Path, agent_name: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        let (read_half, mut write_half) = stream.into_split();

        let hello = serde_json::json!({ "agent": agent_name });
        let mut hello_line = serde_json::to_string(&hello)?;
        hello_line.push('\n');
        write_half.write_all(hello_line.as_bytes()).await?;

        Ok(Self {
            stream: Mutex::new(BufStream {
                reader: BufReader::new(read_half),
                writer: write_half,
            }),
        })
    }

    async fn call(&self, tool: &str, args: serde_json::Value) -> Result<serde_json::Value> {
        let req = RelayRequest {
            id: uuid::Uuid::new_v4().to_string(),
            from: String::new(),
            tool: tool.to_string(),
            args,
        };

        let mut stream = self.stream.lock().await;
        let mut req_line = serde_json::to_string(&req)?;
        req_line.push('\n');
        stream.writer.write_all(req_line.as_bytes()).await?;

        let mut resp_line = String::new();
        stream.reader.read_line(&mut resp_line).await?;
        let resp: RelayResponse = serde_json::from_str(&resp_line)?;

        if let Some(err) = resp.error {
            anyhow::bail!("{}", err);
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}

#[derive(Clone)]
struct OrchestratorMcp {
    client: Arc<RelayClient>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl OrchestratorMcp {
    #[tool(
        description = "Send a message to the runtime or another agent. Use kind 'task_complete'/'task_blocked' for status updates."
    )]
    async fn send_message(&self, Parameters(params): Parameters<SendMessageParams>) -> String {
        relay_call(&self.client, "send_message", &params, "Message sent").await
    }

    #[tool(description = "List tasks from the database. Optionally filter by status or assignee.")]
    async fn list_tasks(&self, Parameters(params): Parameters<ListTasksParams>) -> String {
        relay_call_json(&self.client, "list_tasks", &params).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OrchestratorMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Agent orchestrator tools for task agents. \
                 Use send_message for status updates and list_tasks to query the task database."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

async fn relay_call<T: Serialize>(
    client: &RelayClient,
    tool: &str,
    params: &T,
    ok_msg: &str,
) -> String {
    match serde_json::to_value(params) {
        Ok(args) => match client.call(tool, args).await {
            Ok(_) => ok_msg.to_string(),
            Err(e) => format!("Error: {}", e),
        },
        Err(e) => format!("Error serializing params: {}", e),
    }
}

async fn relay_call_json<T: Serialize>(client: &RelayClient, tool: &str, params: &T) -> String {
    match serde_json::to_value(params) {
        Ok(args) => match client.call(tool, args).await {
            Ok(val) => serde_json::to_string_pretty(&val).unwrap_or_else(|_| "ok".into()),
            Err(e) => format!("Error: {}", e),
        },
        Err(e) => format!("Error serializing params: {}", e),
    }
}

pub async fn run_mcp_server(socket_path: PathBuf, agent_name: String) -> Result<()> {
    let client = RelayClient::connect(&socket_path, &agent_name).await?;
    let service = OrchestratorMcp {
        client: Arc::new(client),
        tool_router: OrchestratorMcp::tool_router(),
    };
    let server = service.serve(rmcp::transport::io::stdio()).await?;
    server.waiting().await?;
    Ok(())
}
