//! Task DB tool handlers for the relay and bus_tools.
//!
//! These tools let agents interact with the task database:
//! - Manager: create_task
//! - All: list_tasks
//!
//! Architect review (approve/complete/reject) is handled by the external
//! claude-architect daemon via architect_client, not by agent tools.

use agent_bus::Mailbox;
use llm_tasks::db::Database;

use crate::types::AgentRole;

pub async fn handle_create_task(
    db: &Database,
    mailbox: &Mailbox,
    agent_name: &str,
    role: AgentRole,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role != AgentRole::Manager {
        return Err(format!("'{}' is not allowed to create tasks", agent_name));
    }
    let title = args["title"].as_str().ok_or("missing 'title'")?;
    let desc = args["description"].as_str();
    let priority = args["priority"].as_u64().unwrap_or(0) as u8;
    let task = db
        .create_task(title, desc, priority, agent_name)
        .await
        .map_err(|e| format!("DB error: {e}"))?;
    let _ = mailbox.send("runtime", "task_created", serde_json::json!({"task_id": task.id}));
    Ok(serde_json::to_value(&task).unwrap_or_default())
}

pub async fn handle_list_tasks(
    db: &Database,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let status = args["status"].as_str();
    let assignee = args["assignee"].as_str();
    let tasks = db
        .list_tasks(status, assignee)
        .await
        .map_err(|e| format!("DB error: {e}"))?;
    Ok(serde_json::to_value(&tasks).unwrap_or_default())
}
