//! Relay server: bridges MCP tool calls from agent subprocesses to the in-process bus.
//!
//! Each agent's Claude session spawns `agent-orchestrator mcp-serve` as its MCP server.
//! mcp-serve connects to this relay via Unix socket to route tool calls.

use std::path::PathBuf;
use std::sync::Arc;

use agent_bus::{Bus, Mailbox};
use anyhow::Result;
use llm_tasks::db::Database;
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
pub fn relay_socket_path(project: &str) -> PathBuf {
    home_dir()
        .join(".claude/orchestrator")
        .join(project)
        .join("relay.sock")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub struct RelayServer {
    bus: Bus,
    db: Arc<Database>,
}

impl RelayServer {
    pub fn new(bus: Bus, db: Arc<Database>) -> Self {
        Self { bus, db }
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
            let db = self.db.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(bus, db, stream).await {
                    tracing::error!("Relay connection error: {}", e);
                }
            });
        }
    }
}

async fn handle_connection(bus: Bus, db: Arc<Database>, stream: UnixStream) -> Result<()> {
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
    process_requests(&mailbox, &db, &agent_name, &mut lines, &mut writer).await?;
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
    db: &Database,
    agent_name: &str,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            continue;
        }
        let response = response_for_line(mailbox, db, agent_name, &line).await;
        let mut resp_line = serde_json::to_string(&response)?;
        resp_line.push('\n');
        writer.write_all(resp_line.as_bytes()).await?;
    }
    Ok(())
}

async fn response_for_line(
    mailbox: &Mailbox,
    db: &Database,
    agent_name: &str,
    line: &str,
) -> RelayResponse {
    let request = match parse_relay_request(agent_name, line) {
        Ok(request) => request,
        Err(response) => return response,
    };
    handle_tool_call(mailbox, db, agent_name, &request).await
}

fn parse_relay_request(agent_name: &str, line: &str) -> Result<RelayRequest, RelayResponse> {
    serde_json::from_str(line).map_err(|error| {
        tracing::error!("Relay: bad request from {}: {}", agent_name, error);
        RelayResponse {
            id: "parse_error".to_string(),
            result: None,
            error: Some(format!("parse error: {}", error)),
        }
    })
}

async fn handle_tool_call(
    mailbox: &Mailbox,
    db: &Database,
    agent_name: &str,
    req: &RelayRequest,
) -> RelayResponse {
    // Notify runtime of task agent activity for idle timeout tracking
    if role_from_agent_name(agent_name) == Some(AgentRole::TaskAgent) {
        let payload = serde_json::json!({ "agent": agent_name });
        let _ = mailbox.send("runtime", "agent_heartbeat", payload);
    }

    let result = dispatch_tool(mailbox, db, agent_name, req).await;
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

async fn dispatch_tool(
    mailbox: &Mailbox,
    db: &Database,
    agent_name: &str,
    req: &RelayRequest,
) -> Result<serde_json::Value, String> {
    use crate::task_tools as tt;
    match req.tool.as_str() {
        "send_message" => handle_send_message(mailbox, agent_name, &req.args),
        "list_tasks" => tt::handle_list_tasks(db, &req.args).await,
        unknown => Err(format!("unknown tool: {}", unknown)),
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

    let payload = serde_json::json!({
        "content": content,
        "from_agent": agent_name,
    });

    let result = mailbox
        .send(to, kind, payload.clone())
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e));

    // CC runtime on task lifecycle events for DB recording
    if result.is_ok() && (kind == "task_complete" || kind == "task_blocked") {
        let _ = mailbox.send("runtime", kind, payload);
    }

    result
}

pub fn role_from_agent_name(name: &str) -> Option<AgentRole> {
    match name {
        "merger" => Some(AgentRole::Merger),
        n if n.starts_with("task-") => Some(AgentRole::TaskAgent),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus::Bus;

    async fn test_db() -> Database {
        let tmp = std::env::temp_dir().join(format!("relay-test-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&tmp);
        Database::open(&tmp.join("tasks.db")).await.unwrap()
    }

    #[test]
    fn relay_request_roundtrips_json() {
        let req = RelayRequest {
            id: "req-1".to_string(),
            from: "task-lt-abc".to_string(),
            tool: "send_message".to_string(),
            args: serde_json::json!({"to": "merger", "kind": "task_complete", "content": "done"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: RelayRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "req-1");
        assert_eq!(decoded.tool, "send_message");
    }

    #[test]
    fn relay_response_ok_roundtrips_json() {
        let resp = RelayResponse {
            id: "req-2".to_string(),
            result: Some(serde_json::json!({"ok": true})),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: RelayResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "req-2");
        assert!(decoded.result.is_some());
        assert!(decoded.error.is_none());
    }

    #[test]
    fn role_from_agent_name_known_names() {
        assert_eq!(role_from_agent_name("merger"), Some(AgentRole::Merger));
        assert_eq!(
            role_from_agent_name("task-lt-abc"),
            Some(AgentRole::TaskAgent)
        );
        assert_eq!(role_from_agent_name("task-123"), Some(AgentRole::TaskAgent));
    }

    #[test]
    fn role_from_agent_name_unknown_returns_none() {
        assert_eq!(role_from_agent_name(""), None);
        assert_eq!(role_from_agent_name("unknown"), None);
        assert_eq!(role_from_agent_name("runtime"), None);
        assert_eq!(role_from_agent_name("relay-task-abc"), None);
    }

    #[tokio::test]
    async fn handle_tool_call_unknown_tool_returns_error() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-task-abc").unwrap();
        let _runtime = bus.register("runtime").unwrap();
        let db = test_db().await;
        let req = RelayRequest {
            id: "r1".to_string(),
            from: "task-abc".to_string(),
            tool: "nonexistent_tool".to_string(),
            args: serde_json::json!({}),
        };
        let resp = handle_tool_call(&mailbox, &db, "task-abc", &req).await;
        assert_eq!(resp.id, "r1");
        assert!(resp.error.is_some());
        assert!(resp.error.unwrap().contains("unknown tool"));
    }

    #[test]
    fn send_message_routes_to_correct_target() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-task-abc").unwrap();
        let mut merger = bus.register("merger").unwrap();
        let args = serde_json::json!({"to": "merger", "kind": "task_complete", "content": "done"});
        handle_send_message(&mailbox, "task-abc", &args).unwrap();
        let msg = merger.try_recv().unwrap();
        assert_eq!(msg.kind, "task_complete");
        assert_eq!(msg.payload["content"], "done");
        assert_eq!(msg.payload["from_agent"], "task-abc");
    }

    #[test]
    fn send_message_fails_for_unknown_target() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-task-abc").unwrap();
        let args = serde_json::json!({"to": "ghost", "kind": "task_complete", "content": "hello"});
        let result = handle_send_message(&mailbox, "task-abc", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("send failed"));
    }

    async fn spawn_relay_server(socket_path: &std::path::Path) {
        let bus = Bus::new();
        let db = Arc::new(test_db().await);
        let server = RelayServer::new(bus.clone(), db);
        let path_clone = socket_path.to_path_buf();
        tokio::spawn(async move {
            let _runtime = bus.register("runtime").unwrap();
            server.run(&path_clone).await.ok()
        });
        for _ in 0..20 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn integration_relay_hello_and_request() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let socket_path =
            std::path::PathBuf::from(format!("/tmp/test-relay-{}.sock", uuid::Uuid::new_v4()));
        spawn_relay_server(&socket_path).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        writer
            .write_all(b"{\"agent\": \"task-lt-test\"}\n")
            .await
            .unwrap();

        let req = serde_json::json!({
            "id": "t1", "from": "task-lt-test",
            "tool": "list_tasks", "args": {}
        });
        let line = format!("{}\n", serde_json::to_string(&req).unwrap());
        writer.write_all(line.as_bytes()).await.unwrap();

        let resp_line = lines.next_line().await.unwrap().unwrap();
        let resp: RelayResponse = serde_json::from_str(&resp_line).unwrap();

        assert_eq!(resp.id, "t1");
        assert!(resp.error.is_none());
        let _ = std::fs::remove_file(&socket_path);
    }
}
