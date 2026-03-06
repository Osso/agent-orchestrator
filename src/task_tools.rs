//! Task DB tool handlers for the relay and bus_tools.
//!
//! These tools let agents interact with the task database:
//! - Manager: create_task
//! - Architect: approve_task, complete_task, reject_completion, list_tasks
//! - Developer: (signals via bus messages, runtime handles transitions)
//! - All: list_tasks

use agent_bus::Mailbox;
use llm_tasks::db::{Database, TaskUpdates};

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

pub async fn handle_approve_task(
    db: &Database,
    mailbox: &Mailbox,
    agent_name: &str,
    role: AgentRole,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role != AgentRole::Architect {
        return Err(format!("'{}' is not allowed to approve tasks", agent_name));
    }
    let id = args["id"].as_str().ok_or("missing 'id'")?;
    let task = db.get_task(id).await.map_err(|e| format!("DB error: {e}"))?;
    if task.status != "pending" {
        return Err(format!("Cannot approve task {}: status is {}", id, task.status));
    }
    let updates = TaskUpdates { status: Some("ready"), ..Default::default() };
    db.update_task(id, updates, agent_name)
        .await
        .map_err(|e| format!("DB error: {e}"))?;
    let _ = mailbox.send("runtime", "task_ready", serde_json::json!({"task_id": id}));
    Ok(serde_json::json!({"ok": true, "status": "ready"}))
}

pub async fn handle_complete_task(
    db: &Database,
    mailbox: &Mailbox,
    agent_name: &str,
    role: AgentRole,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role != AgentRole::Architect {
        return Err(format!("'{}' is not allowed to complete tasks", agent_name));
    }
    let id = args["id"].as_str().ok_or("missing 'id'")?;
    let task = db.get_task(id).await.map_err(|e| format!("DB error: {e}"))?;
    if task.status != "in_review" {
        return Err(format!("Cannot complete task {}: status is {}", id, task.status));
    }
    db.close_task(id, agent_name)
        .await
        .map_err(|e| format!("DB error: {e}"))?;
    let _ = mailbox.send("runtime", "task_done", serde_json::json!({"task_id": id}));
    let _ = mailbox.send(
        "manager",
        "task_done",
        serde_json::json!({"content": format!("Task {} completed: {}", id, task.title), "task_id": id}),
    );
    Ok(serde_json::json!({"ok": true, "status": "completed"}))
}

pub async fn handle_reject_completion(
    db: &Database,
    mailbox: &Mailbox,
    agent_name: &str,
    role: AgentRole,
    args: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    if role != AgentRole::Architect {
        return Err(format!("'{}' is not allowed to reject completions", agent_name));
    }
    let id = args["id"].as_str().ok_or("missing 'id'")?;
    let reason = args["reason"].as_str().unwrap_or("needs more work");
    let task = db.get_task(id).await.map_err(|e| format!("DB error: {e}"))?;
    if task.status != "in_review" {
        return Err(format!("Cannot reject task {}: status is {}", id, task.status));
    }
    let updates = TaskUpdates { status: Some("ready"), ..Default::default() };
    db.update_task(id, updates, agent_name)
        .await
        .map_err(|e| format!("DB error: {e}"))?;
    let _ = mailbox.send(
        "manager",
        "task_rejected",
        serde_json::json!({"content": format!("Task {} rejected: {}", id, reason), "task_id": id}),
    );
    Ok(serde_json::json!({"ok": true, "status": "ready"}))
}

