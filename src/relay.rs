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
/// Uses ~/.claude/orchestrator/ so the socket is visible inside bwrap sandboxes
/// (which bind-mount ~/.claude writable but overlay /tmp with tmpfs).
pub fn relay_socket_path() -> PathBuf {
    home_dir().join(".claude/orchestrator/relay.sock")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
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
        "goal_complete" => handle_goal_complete(mailbox, agent_name, &req.args),
        "relieve_manager" => handle_relieve_manager(mailbox, agent_name, &req.args),
        "report" => handle_report(mailbox, agent_name, &req.args),
        "merge_request" => handle_merge_request(mailbox, agent_name, &req.args),
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

fn handle_goal_complete(
    mailbox: &Mailbox,
    agent_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role_from_agent_name(agent_name) != Some(AgentRole::Manager) {
        return Err(format!(
            "'{}' is not allowed to declare goal complete",
            agent_name
        ));
    }

    let summary = args["summary"].as_str().ok_or("missing 'summary'")?;
    let payload = serde_json::json!({ "summary": summary, "from_agent": agent_name });

    mailbox
        .send("runtime", "goal_complete", payload)
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e))
}

fn handle_relieve_manager(
    mailbox: &Mailbox,
    agent_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role_from_agent_name(agent_name) != Some(AgentRole::Auditor) {
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
    if role_from_agent_name(agent_name) != Some(AgentRole::Auditor) {
        return Err(format!("'{}' is not allowed to submit reports", agent_name));
    }

    let report_type = args["report_type"].as_str().ok_or("missing 'report_type'")?;
    let content = args["content"].as_str().ok_or("missing 'content'")?;

    tracing::info!(
        "[AUDITOR {}] {}",
        report_type.to_uppercase(),
        first_line(content)
    );

    let payload = serde_json::json!({
        "report_type": report_type,
        "content": content,
        "from_agent": agent_name,
    });

    mailbox
        .send("runtime", "auditor_report", payload)
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e))
}

fn handle_merge_request(
    mailbox: &Mailbox,
    agent_name: &str,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role_from_agent_name(agent_name) != Some(AgentRole::Developer) {
        return Err(format!("'{}' is not allowed to request merges", agent_name));
    }

    let branch = args["branch"].as_str().ok_or("missing 'branch'")?;
    let description = args["description"].as_str().ok_or("missing 'description'")?;

    let payload = serde_json::json!({
        "branch": branch,
        "description": description,
        "from_developer": agent_name,
    });

    mailbox
        .send("merger", "merge_request", payload)
        .map(|_| serde_json::json!({"ok": true}))
        .map_err(|e| format!("send failed: {}", e))
}

fn role_from_agent_name(name: &str) -> Option<AgentRole> {
    match name {
        "manager" => Some(AgentRole::Manager),
        "architect" => Some(AgentRole::Architect),
        "auditor" => Some(AgentRole::Auditor),
        "merger" => Some(AgentRole::Merger),
        n if n.starts_with("developer-") => Some(AgentRole::Developer),
        _ => None,
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus::Bus;

    // --- Wire protocol serialization ---

    #[test]
    fn relay_request_roundtrips_json() {
        let req = RelayRequest {
            id: "req-1".to_string(),
            from: "manager".to_string(),
            tool: "send_message".to_string(),
            args: serde_json::json!({"to": "architect", "kind": "task_assignment", "content": "do stuff"}),
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: RelayRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "req-1");
        assert_eq!(decoded.tool, "send_message");
        assert_eq!(decoded.args["to"], "architect");
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
    fn relay_response_error_roundtrips_json() {
        let resp = RelayResponse {
            id: "req-3".to_string(),
            result: None,
            error: Some("something went wrong".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let decoded: RelayResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.id, "req-3");
        assert!(decoded.result.is_none());
        assert_eq!(decoded.error.as_deref(), Some("something went wrong"));
    }

    // --- role_from_agent_name ---

    #[test]
    fn role_from_agent_name_known_names() {
        assert_eq!(role_from_agent_name("manager"), Some(AgentRole::Manager));
        assert_eq!(role_from_agent_name("architect"), Some(AgentRole::Architect));
        assert_eq!(role_from_agent_name("auditor"), Some(AgentRole::Auditor));
        assert_eq!(role_from_agent_name("merger"), Some(AgentRole::Merger));
        assert_eq!(role_from_agent_name("developer-0"), Some(AgentRole::Developer));
        assert_eq!(role_from_agent_name("developer-2"), Some(AgentRole::Developer));
    }

    #[test]
    fn role_from_agent_name_unknown_returns_none() {
        assert_eq!(role_from_agent_name(""), None);
        assert_eq!(role_from_agent_name("unknown"), None);
        assert_eq!(role_from_agent_name("runtime"), None);
        assert_eq!(role_from_agent_name("relay-manager"), None);
    }

    // --- handle_tool_call dispatch ---

    #[test]
    fn handle_tool_call_unknown_tool_returns_error() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let _runtime = bus.register("runtime").unwrap();
        let req = RelayRequest {
            id: "r1".to_string(),
            from: "manager".to_string(),
            tool: "nonexistent_tool".to_string(),
            args: serde_json::json!({}),
        };
        let resp = handle_tool_call(&mailbox, "manager", &req);
        assert_eq!(resp.id, "r1");
        assert!(resp.error.is_some());
        assert!(resp.error.unwrap().contains("unknown tool"));
    }

    #[test]
    fn handle_tool_call_known_tool_dispatches() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let mut runtime = bus.register("runtime").unwrap();
        let req = RelayRequest {
            id: "r2".to_string(),
            from: "manager".to_string(),
            tool: "set_crew".to_string(),
            args: serde_json::json!({"count": 2}),
        };
        let resp = handle_tool_call(&mailbox, "manager", &req);
        assert_eq!(resp.id, "r2");
        assert!(resp.error.is_none());
        assert!(runtime.try_recv().is_some());
    }

    // --- Role validation: send_message interrupt ---

    #[test]
    fn send_message_interrupt_rejected_from_non_architect() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let _target = bus.register("developer-0").unwrap();
        let args = serde_json::json!({"to": "developer-0", "kind": "interrupt", "content": "stop"});
        let result = handle_send_message(&mailbox, "manager", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed to send interrupt"));
    }

    #[test]
    fn send_message_interrupt_allowed_from_architect() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-architect").unwrap();
        let mut target = bus.register("developer-0").unwrap();
        let args = serde_json::json!({"to": "developer-0", "kind": "interrupt", "content": "stop"});
        let result = handle_send_message(&mailbox, "architect", &args);
        assert!(result.is_ok());
        assert!(target.try_recv().is_some());
    }

    // --- Role validation: set_crew ---

    #[test]
    fn set_crew_rejected_from_non_manager() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-architect").unwrap();
        let _runtime = bus.register("runtime").unwrap();
        let args = serde_json::json!({"count": 3});
        let result = handle_set_crew(&mailbox, "architect", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed to set crew size"));
    }

    #[test]
    fn set_crew_allowed_from_manager() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let mut runtime = bus.register("runtime").unwrap();
        let args = serde_json::json!({"count": 2});
        let result = handle_set_crew(&mailbox, "manager", &args);
        assert!(result.is_ok());
        let msg = runtime.try_recv().unwrap();
        assert_eq!(msg.kind, "set_crew");
        assert_eq!(msg.payload["count"], 2);
    }

    // --- Role validation: relieve_manager ---

    #[test]
    fn relieve_manager_rejected_from_non_auditor() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let _runtime = bus.register("runtime").unwrap();
        let args = serde_json::json!({"reason": "poor performance"});
        let result = handle_relieve_manager(&mailbox, "manager", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed to relieve"));
    }

    #[test]
    fn relieve_manager_allowed_from_auditor() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-auditor").unwrap();
        let mut runtime = bus.register("runtime").unwrap();
        let args = serde_json::json!({"reason": "poor performance"});
        let result = handle_relieve_manager(&mailbox, "auditor", &args);
        assert!(result.is_ok());
        let msg = runtime.try_recv().unwrap();
        assert_eq!(msg.kind, "relieve_manager");
        assert_eq!(msg.payload["reason"], "poor performance");
    }

    // --- Role validation: report ---

    #[test]
    fn report_rejected_from_non_auditor() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let _runtime = bus.register("runtime").unwrap();
        let args = serde_json::json!({"report_type": "observation", "content": "things look fine"});
        let result = handle_report(&mailbox, "manager", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed to submit reports"));
    }

    #[test]
    fn report_allowed_from_auditor() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-auditor").unwrap();
        let mut runtime = bus.register("runtime").unwrap();
        let args = serde_json::json!({"report_type": "evaluation", "content": "progress is good"});
        let result = handle_report(&mailbox, "auditor", &args);
        assert!(result.is_ok());
        let msg = runtime.try_recv().unwrap();
        assert_eq!(msg.kind, "auditor_report");
        assert_eq!(msg.payload["report_type"], "evaluation");
    }

    // --- send_message routing ---

    #[test]
    fn send_message_routes_to_correct_target() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let mut architect = bus.register("architect").unwrap();
        let args = serde_json::json!({"to": "architect", "kind": "task_assignment", "content": "implement feature X"});
        handle_send_message(&mailbox, "manager", &args).unwrap();
        let msg = architect.try_recv().unwrap();
        assert_eq!(msg.kind, "task_assignment");
        assert_eq!(msg.payload["content"], "implement feature X");
        assert_eq!(msg.payload["from_agent"], "manager");
    }

    #[test]
    fn send_message_fails_for_unknown_target() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let args = serde_json::json!({"to": "ghost", "kind": "task_assignment", "content": "hello"});
        let result = handle_send_message(&mailbox, "manager", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("send failed"));
    }

    // --- Role validation: merge_request ---

    #[test]
    fn merge_request_rejected_from_non_developer() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let _merger = bus.register("merger").unwrap();
        let args = serde_json::json!({"branch": "agent/developer-0", "description": "add feature"});
        let result = handle_merge_request(&mailbox, "manager", &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not allowed to request merges"));
    }

    #[test]
    fn merge_request_allowed_from_developer() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-developer-0").unwrap();
        let mut merger = bus.register("merger").unwrap();
        let args = serde_json::json!({"branch": "agent/developer-0", "description": "add login button"});
        let result = handle_merge_request(&mailbox, "developer-0", &args);
        assert!(result.is_ok());
        let msg = merger.try_recv().unwrap();
        assert_eq!(msg.kind, "merge_request");
        assert_eq!(msg.payload["branch"], "agent/developer-0");
        assert_eq!(msg.payload["description"], "add login button");
        assert_eq!(msg.payload["from_developer"], "developer-0");
    }

    // --- set_crew routing ---

    #[test]
    fn set_crew_sends_to_runtime_with_count() {
        let bus = Bus::new();
        let mailbox = bus.register("relay-manager").unwrap();
        let mut runtime = bus.register("runtime").unwrap();
        handle_set_crew(&mailbox, "manager", &serde_json::json!({"count": 3})).unwrap();
        let msg = runtime.try_recv().unwrap();
        assert_eq!(msg.to, "runtime");
        assert_eq!(msg.payload["count"], 3);
        assert_eq!(msg.payload["from_agent"], "manager");
    }

    // --- Integration: full relay connection via Unix socket ---

    async fn spawn_relay_server(socket_path: &std::path::Path) {
        let bus = Bus::new();
        let server = RelayServer::new(bus.clone());
        let path_clone = socket_path.to_path_buf();
        // Register runtime inside the task so the Mailbox stays alive with the task.
        tokio::spawn(async move {
            let _runtime = bus.register("runtime").unwrap();
            server.run(&path_clone).await.ok()
        });
        for _ in 0..20 {
            if socket_path.exists() { break; }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    }

    async fn relay_request(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>>,
        req: serde_json::Value,
    ) -> RelayResponse {
        use tokio::io::AsyncWriteExt;
        let line = format!("{}\n", serde_json::to_string(&req).unwrap());
        writer.write_all(line.as_bytes()).await.unwrap();
        let resp_line = lines.next_line().await.unwrap().unwrap();
        serde_json::from_str(&resp_line).unwrap()
    }

    #[tokio::test]
    async fn integration_relay_hello_and_request() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixStream;

        let socket_path = std::path::PathBuf::from(
            format!("/tmp/test-relay-{}.sock", uuid::Uuid::new_v4()),
        );
        spawn_relay_server(&socket_path).await;

        let stream = UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        writer.write_all(b"{\"agent\": \"manager\"}\n").await.unwrap();

        let resp = relay_request(
            &mut writer,
            &mut lines,
            serde_json::json!({"id": "t1", "from": "manager", "tool": "set_crew", "args": {"count": 1}}),
        ).await;

        assert_eq!(resp.id, "t1");
        assert!(resp.error.is_none());
        assert_eq!(resp.result.unwrap()["ok"], true);
        let _ = std::fs::remove_file(&socket_path);
    }
}
