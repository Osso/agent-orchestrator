//! ToolDef implementations that mirror MCP relay tools for OpenRouter agents.
//!
//! Each tool holds a shared Mailbox and the agent's name, posting messages
//! directly to the in-process bus instead of going through the Unix socket relay.

use std::sync::Arc;

use agent_bus::Mailbox;
use llm_sdk::tools::{Tool, ToolDef};

use crate::types::AgentRole;

/// Build the ToolSet for an OpenRouter agent based on its role.
/// Mirrors the MCP tools from mcp.rs, role-gated the same way as relay.rs.
pub fn bus_tools_for_role(role: AgentRole, mailbox: Arc<Mailbox>) -> llm_sdk::tools::ToolSet {
    let mut set = llm_sdk::tools::ToolSet::new();

    // All roles can send messages
    set = set.add(SendMessageTool {
        mailbox: mailbox.clone(),
        role,
    });

    match role {
        AgentRole::Manager => {
            set = set.add(SetCrewTool {
                mailbox: mailbox.clone(),
            });
        }
        AgentRole::Auditor => {
            set = set.add(RelieveManagerTool {
                mailbox: mailbox.clone(),
            });
            set = set.add(ReportTool { mailbox });
        }
        AgentRole::Developer => {
            set = set.add(MergeRequestTool { mailbox });
        }
        _ => {}
    }

    set
}

// ---------------------------------------------------------------------------
// send_message
// ---------------------------------------------------------------------------

struct SendMessageTool {
    mailbox: Arc<Mailbox>,
    role: AgentRole,
}

#[async_trait::async_trait]
impl ToolDef for SendMessageTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "send_message".into(),
            description: "Send a message to another agent. Use kind 'task_assignment' for tasks, \
                'architect_review' for reviews, 'task_complete'/'task_blocked' for status updates, \
                'interrupt' to stop an agent."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "enum": ["architect", "developer-0", "developer-1", "developer-2", "merger", "auditor", "manager"], "description": "Target agent name (use exact names)" },
                    "kind": { "type": "string", "description": "Message kind" },
                    "content": { "type": "string", "description": "Message content" }
                },
                "required": ["to", "kind", "content"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> String {
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return format!("Invalid arguments: {e}"),
        };
        let (to, kind, content) = match parse_send_args(&args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        if kind == "interrupt" && self.role != AgentRole::Architect {
            return "Only architect can send interrupt".into();
        }
        let payload = serde_json::json!({ "content": content });
        match self.mailbox.send(to, kind, payload.clone()) {
            Ok(_) => {
                tracing::info!("bus_tool send_message: -> {} kind={}", to, kind);
                // CC runtime on task lifecycle events for DB recording
                if kind == "task_complete" || kind == "task_blocked" {
                    let _ = self.mailbox.send("runtime", kind, payload);
                }
                "Message sent".into()
            }
            Err(e) => {
                tracing::error!("bus_tool send_message failed: -> {} kind={}: {}", to, kind, e);
                format!(
                    "Send failed: {e}. Valid agent names: manager, architect, \
                     developer-0, developer-1, developer-2, merger, auditor"
                )
            }
        }
    }
}

fn parse_send_args(args: &serde_json::Value) -> Result<(&str, &str, &str), String> {
    let to = args["to"].as_str().ok_or("missing 'to'")?;
    let kind = args["kind"].as_str().ok_or("missing 'kind'")?;
    let content = args["content"].as_str().ok_or("missing 'content'")?;
    Ok((to, kind, content))
}

// ---------------------------------------------------------------------------
// set_crew
// ---------------------------------------------------------------------------

struct SetCrewTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait::async_trait]
impl ToolDef for SetCrewTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "set_crew".into(),
            description: "Set the number of developer agents (1-3). Manager only.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer", "description": "Number of developers (1-3)" }
                },
                "required": ["count"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> String {
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return format!("Invalid arguments: {e}"),
        };
        let count = args["count"].as_u64().unwrap_or(1) as u8;
        let payload = serde_json::json!({ "count": count });
        match self.mailbox.send("runtime", "set_crew", payload) {
            Ok(_) => "Crew size updated".into(),
            Err(e) => format!("Send failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// relieve_manager
// ---------------------------------------------------------------------------

struct RelieveManagerTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait::async_trait]
impl ToolDef for RelieveManagerTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "relieve_manager".into(),
            description: "Relieve the current manager and spawn a replacement. Auditor only.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string", "description": "Reason for relieving" }
                },
                "required": ["reason"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> String {
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return format!("Invalid arguments: {e}"),
        };
        let reason = args["reason"].as_str().unwrap_or("no reason");
        let payload = serde_json::json!({ "reason": reason });
        match self.mailbox.send("runtime", "relieve_manager", payload) {
            Ok(_) => "Manager relieved".into(),
            Err(e) => format!("Send failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// report
// ---------------------------------------------------------------------------

struct ReportTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait::async_trait]
impl ToolDef for ReportTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "report".into(),
            description: "Submit a progress evaluation or observation. Auditor only.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "report_type": { "type": "string", "description": "evaluation or observation" },
                    "content": { "type": "string", "description": "Report content" }
                },
                "required": ["report_type", "content"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> String {
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return format!("Invalid arguments: {e}"),
        };
        let report_type = args["report_type"].as_str().unwrap_or("evaluation");
        let content = args["content"].as_str().unwrap_or("");
        let payload = serde_json::json!({
            "report_type": report_type,
            "content": content,
        });
        match self.mailbox.send("runtime", "auditor_report", payload) {
            Ok(_) => "Report submitted".into(),
            Err(e) => format!("Send failed: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// merge_request
// ---------------------------------------------------------------------------

struct MergeRequestTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait::async_trait]
impl ToolDef for MergeRequestTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "merge_request".into(),
            description: "Request that your branch be merged into main. Developer only.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "branch": { "type": "string", "description": "Branch name to merge" },
                    "description": { "type": "string", "description": "Changes description" }
                },
                "required": ["branch", "description"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> String {
        let args: serde_json::Value = match serde_json::from_str(arguments) {
            Ok(v) => v,
            Err(e) => return format!("Invalid arguments: {e}"),
        };
        let branch = args["branch"].as_str().unwrap_or("");
        let description = args["description"].as_str().unwrap_or("");
        let payload = serde_json::json!({
            "branch": branch,
            "description": description,
        });
        match self.mailbox.send("merger", "merge_request", payload) {
            Ok(_) => "Merge request submitted".into(),
            Err(e) => format!("Send failed: {e}"),
        }
    }
}
