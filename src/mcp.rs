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
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::relay::{RelayRequest, RelayResponse};

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SendMessageParams {
    /// Target agent name (e.g. "architect", "manager", "developer-0")
    to: String,
    /// Message kind (e.g. "task_assignment", "architect_review", "task_complete", "task_blocked", "interrupt")
    kind: String,
    /// Message content
    content: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct SetCrewParams {
    /// Number of developer agents (1-6)
    count: u8,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct RelieveManagerParams {
    /// Reason for relieving the manager
    reason: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ReportParams {
    /// Report type: "evaluation" or "observation"
    report_type: String,
    /// Report content
    content: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct GoalCompleteParams {
    /// Summary of what was accomplished
    summary: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct CreateTaskParams {
    /// Task title (short, actionable)
    title: String,
    /// Detailed task description with context and acceptance criteria
    description: Option<String>,
    /// Priority (0=none, 1=low, 2=medium, 3=high)
    priority: Option<u8>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ListTasksParams {
    /// Filter by status: pending, ready, in_progress, needs_info, in_review, completed
    status: Option<String>,
    /// Filter by assignee (e.g. "developer-0")
    assignee: Option<String>,
}


struct BufStream {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

/// Client that communicates with the orchestrator relay via Unix socket.
struct RelayClient {
    stream: Mutex<BufStream>,
}

impl RelayClient {
    async fn connect(socket_path: &std::path::Path, agent_name: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket_path).await?;
        let (read_half, mut write_half) = stream.into_split();

        // Send hello to identify this agent
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
    #[tool(description = "Send a message to another agent. Use kind 'task_complete'/'task_blocked' for status updates, 'interrupt' to stop an agent.")]
    async fn send_message(&self, Parameters(params): Parameters<SendMessageParams>) -> String {
        relay_call(&self.client, "send_message", &params, "Message sent").await
    }

    #[tool(description = "Declare the user's goal fully achieved. Triggers orchestrator shutdown. Manager only.")]
    async fn goal_complete(&self, Parameters(params): Parameters<GoalCompleteParams>) -> String {
        relay_call(&self.client, "goal_complete", &params, "Goal marked complete. Orchestrator shutting down.").await
    }

    #[tool(description = "Set the number of developer agents (1-6). Manager only.")]
    async fn set_crew(&self, Parameters(params): Parameters<SetCrewParams>) -> String {
        relay_call(&self.client, "set_crew", &params, "Crew size updated").await
    }

    #[tool(description = "Relieve the current manager and spawn a replacement. Auditor only.")]
    async fn relieve_manager(&self, Parameters(params): Parameters<RelieveManagerParams>) -> String {
        relay_call(&self.client, "relieve_manager", &params, "Manager relieved").await
    }

    #[tool(description = "Submit a progress evaluation or observation. Auditor only.")]
    async fn report(&self, Parameters(params): Parameters<ReportParams>) -> String {
        relay_call(&self.client, "report", &params, "Report submitted").await
    }

    #[tool(description = "Create a new task in the task database. Manager only. Tasks start as 'pending' and must be approved by the architect before dispatch.")]
    async fn create_task(&self, Parameters(params): Parameters<CreateTaskParams>) -> String {
        relay_call_json(&self.client, "create_task", &params).await
    }

    #[tool(description = "List tasks from the database. Optionally filter by status or assignee. All roles.")]
    async fn list_tasks(&self, Parameters(params): Parameters<ListTasksParams>) -> String {
        relay_call_json(&self.client, "list_tasks", &params).await
    }

}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for OrchestratorMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Agent orchestrator tools for inter-agent communication. \
                 Use these tools instead of free-form text to coordinate with other agents."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Call a relay tool and return a fixed success message or the error.
async fn relay_call<T: Serialize>(client: &RelayClient, tool: &str, params: &T, ok_msg: &str) -> String {
    match serde_json::to_value(params) {
        Ok(args) => match client.call(tool, args).await {
            Ok(_) => ok_msg.to_string(),
            Err(e) => format!("Error: {}", e),
        },
        Err(e) => format!("Error serializing params: {}", e),
    }
}

/// Call a relay tool and return the JSON result or the error.
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
