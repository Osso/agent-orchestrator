//! Task DB tool handlers for the relay.
//!
//! - All agents: list_tasks

use llm_tasks::db::Database;

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
