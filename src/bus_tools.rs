//! ToolDef implementations that mirror MCP relay tools for OpenRouter agents.
//!
//! Each tool holds a shared Mailbox and the agent's name, posting messages
//! directly to the in-process bus instead of going through the Unix socket relay.

use std::sync::Arc;

use agent_bus::Mailbox;
use llm_sdk::tools::{Tool, ToolDef};

use crate::types::AgentRole;

/// Build the ToolSet for an OpenRouter agent based on its role.
pub fn bus_tools_for_role(_role: AgentRole, mailbox: Arc<Mailbox>) -> llm_sdk::tools::ToolSet {
    let mut set = llm_sdk::tools::ToolSet::new();
    set = set.add(SendMessageTool { mailbox });
    set
}

struct SendMessageTool {
    mailbox: Arc<Mailbox>,
}

#[async_trait::async_trait]
impl ToolDef for SendMessageTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "send_message".into(),
            description: "Send a message to the runtime or another agent. \
                Use kind 'task_complete'/'task_blocked' for status updates."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Target agent name (e.g. 'runtime', 'merger')" },
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
        let payload = serde_json::json!({ "content": content });
        match self.mailbox.send(to, kind, payload.clone()) {
            Ok(_) => {
                tracing::info!("bus_tool send_message: -> {} kind={}", to, kind);
                if kind == "task_complete" || kind == "task_blocked" {
                    let _ = self.mailbox.send("runtime", kind, payload);
                }
                "Message sent".into()
            }
            Err(e) => {
                tracing::warn!("bus_tool send_message failed: -> {} kind={}: {}", to, kind, e);
                format!("Send failed: {e}")
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
