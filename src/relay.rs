//! Relay server: bridges MCP tool calls from agent subprocesses to the in-process bus.
//!
//! Each agent's Claude session spawns `agent-orchestrator mcp-serve` as its MCP server.
//! mcp-serve connects to this relay via Unix socket to route tool calls.

use std::path::PathBuf;

use agent_bus::{Bus, Mailbox};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::types::AgentRole;

/// Wire protocol: request from mcp-serve to relay.
#[derive(Debug, Serialize, Deserialize)]
pub struct RelayRequest {
    pub id: String,
    /// Unused in relay (agent name comes from hello), kept for symmetry.
    pub from: String,
    pub tool: String,
    pub args: serde_json::Value,
}

/// Wire protocol: response from relay to mcp-serve.
#[derive(Debug, Serialize, Deserialize)]
pub struct RelayResponse {
    pub id: String,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Returns the canonical path for the relay Unix socket.
pub fn relay_socket_path() -> PathBuf {
    PathBuf::from("/tmp/claude/orchestrator/relay.sock")
}

pub struct RelayServer {
    bus: Bus,
}

impl RelayServer {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn run(self, socket_path: &std::path::Path) -> Result<()> {
        let _ = std::fs::remove_file(socket_path);
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(socket_path)?;
        tracing::info!("Relay listening on {}", socket_path.display());

        loop {
            let (stream, _) = listener.accept().await?;
            let bus = self.bus.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(bus, stream).await {
                    tracing::error!("Relay connection error: {}", e);
                }
            });
        }
    }
}

async fn handle_connection(bus: Bus, stream: UnixStream) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let agent_name = read_hello(&mut lines).await?;
    if role_from_agent_name(&agent_name).is_none() {
        anyhow::bail!("unknown agent name: {}", agent_name);
    }

    let relay_name = format!("relay-{}", agent_name);
    let mailbox = bus
        .register(&relay_name)
        .map_err(|e| anyhow::anyhow!("Failed to register relay mailbox {}: {}", relay_name, e))?;

    tracing::info!("Relay: agent '{}' connected", agent_name);
    process_requests(&mailbox, &agent_name, &mut lines, &mut writer).await?;
    tracing::info!("Relay: agent '{}' disconnected", agent_name);
    Ok(())
}

async fn read_hello(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<String> {
    let hello_line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow::anyhow!("connection closed before hello"))?;
    let hello: serde_json::Value = serde_json::from_str(&hello_line)?;
    hello["agent"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("missing 'agent' in hello"))
}

async fn process_requests(
    mailbox: &Mailbox,
    agent_name: &str,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<RelayRequest>(&line) {
            Ok(request) => handle_tool_call(mailbox, agent_name, &request),
            Err(e) => {
                tracing::error!("Relay: bad request from {}: {}", agent_name, e);
                RelayResponse {
                    id: "parse_error".to_string(),
                    result: None,
                    error: Some(format!("parse error: {}", e)),
                }
            }
        };
        let mut resp_line = serde_json::to_string(&response)?;
        resp_line.push('\n');
        writer.write_all(resp_line.as_bytes()).await?;
    }
    Ok(())
}

fn handle_tool_call(mailbox: &Mailbox, agent_name: &str, req: &RelayRequest) -> RelayResponse {
    let result = match req.tool.as_str() {
        "send_message" => handle_send_message(mailbox, agent_name, &req.args),
        "set_crew" => handle_set_crew(mailbox, agent_name, &req.args),
        "relieve_manager" => handle_relieve_manager(mailbox, agent_name, &req.args),
        "report" => handle_report(mailbox, agent_name, &req.args),
        unknown => Err(format!("unknown tool: {}", unknown)),
    };

    match result {
        Ok(val) => RelayResponse {
            id: req.id.clone(),
            result: Some(val),
            error: None,
        },
        Err(msg) => RelayResponse {
            id: req.id.clone(),
            result: None,
            error: Some(msg),
        },
    }
}

fn handle_send_message(
    mailbox: &Mailbox,
    agent_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let to = args["to"].as_str().ok_or("missing 'to'")?;
    let kind = args["kind"].as_str().ok_or("missing 'kind'")?;
    let content = args["content"].as_str().ok_or("missing 'content'")?;

    // Validate INTERRUPT is only from architect
    if kind == "interrupt" && role_from_agent_name(agent_name) != Some(AgentRole::Architect) {
        return Err(format!("'{}' is not allowed to send interrupt", agent_name));
    }

    let payload = serde_json::json!({
        "content": content,
        "from_agent": agent_name,
    });

    mailbox
        .send(to, kind, payload)
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e))
}

fn handle_set_crew(
    mailbox: &Mailbox,
    agent_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role_from_agent_name(agent_name) != Some(AgentRole::Manager) {
        return Err(format!("'{}' is not allowed to set crew size", agent_name));
    }

    let count = args["count"].as_u64().ok_or("missing 'count'")? as u8;
    let payload = serde_json::json!({ "count": count, "from_agent": agent_name });

    mailbox
        .send("runtime", "set_crew", payload)
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e))
}

fn handle_relieve_manager(
    mailbox: &Mailbox,
    agent_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role_from_agent_name(agent_name) != Some(AgentRole::Scorer) {
        return Err(format!(
            "'{}' is not allowed to relieve the manager",
            agent_name
        ));
    }

    let reason = args["reason"].as_str().ok_or("missing 'reason'")?;
    let payload = serde_json::json!({ "reason": reason, "from_agent": agent_name });

    mailbox
        .send("runtime", "relieve_manager", payload)
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e))
}

fn handle_report(
    mailbox: &Mailbox,
    agent_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role_from_agent_name(agent_name) != Some(AgentRole::Scorer) {
        return Err(format!("'{}' is not allowed to submit reports", agent_name));
    }

    let report_type = args["report_type"].as_str().ok_or("missing 'report_type'")?;
    let content = args["content"].as_str().ok_or("missing 'content'")?;

    tracing::info!(
        "[SCORER {}] {}",
        report_type.to_uppercase(),
        first_line(content)
    );

    let payload = serde_json::json!({
        "report_type": report_type,
        "content": content,
        "from_agent": agent_name,
    });

    mailbox
        .send("runtime", "scorer_report", payload)
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e))
}

fn role_from_agent_name(name: &str) -> Option<AgentRole> {
    match name {
        "manager" => Some(AgentRole::Manager),
        "architect" => Some(AgentRole::Architect),
        "scorer" => Some(AgentRole::Scorer),
        n if n.starts_with("developer-") => Some(AgentRole::Developer),
        _ => None,
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}
