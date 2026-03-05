//! Task dispatch logic: matches ready tasks to idle developers.
//!
//! The runtime calls into this module when task state changes.
//! It finds ready (unassigned) tasks, claims them, and sends
//! task_assignment messages to idle developers via the bus.

use std::collections::HashMap;
use std::sync::Arc;

use agent_bus::Mailbox;
use llm_tasks::db::{Database, TaskUpdates};

/// Tracks task assignments and handles dispatch + state transitions.
pub struct Dispatcher {
    db: Arc<Database>,
    mailbox: Mailbox,
    /// developer bus_name → task_id
    dev_tasks: HashMap<String, String>,
}

impl Dispatcher {
    pub fn new(db: Arc<Database>, mailbox: Mailbox) -> Self {
        Self { db, mailbox, dev_tasks: HashMap::new() }
    }

    /// Developer signals task complete → set in_review, notify architect.
    pub async fn handle_dev_complete(&mut self, from: &str, content: &str) {
        if let Some(task_id) = self.dev_tasks.remove(from) {
            self.transition_to_review(&task_id, from, content).await;
        }
    }

    /// Developer signals needs_info → set needs_info, notify manager.
    pub async fn handle_dev_needs_info(&mut self, from: &str, content: &str) {
        if let Some(task_id) = self.dev_tasks.remove(from) {
            self.transition_to_needs_info(&task_id, from, content).await;
        }
    }

    async fn transition_to_review(&self, task_id: &str, dev: &str, summary: &str) {
        let updates = TaskUpdates { status: Some("in_review"), ..Default::default() };
        if let Err(e) = self.db.update_task(task_id, updates, "runtime").await {
            tracing::error!("Failed to set task {} in_review: {}", task_id, e);
        }
        let payload = serde_json::json!({
            "content": format!("## Verify Completion\n\n**Task ID:** {task_id}\n**Developer:** {dev}\n**Output:**\n{summary}"),
            "task_id": task_id,
        });
        let _ = self.mailbox.send("architect", "verify_completion", payload);
    }

    async fn transition_to_needs_info(&self, task_id: &str, dev: &str, question: &str) {
        let updates = TaskUpdates { status: Some("needs_info"), ..Default::default() };
        if let Err(e) = self.db.update_task(task_id, updates, "runtime").await {
            tracing::error!("Failed to set task {} needs_info: {}", task_id, e);
        }
        let payload = serde_json::json!({
            "content": format!("Task {task_id} from {dev} needs info: {question}"),
            "task_id": task_id,
        });
        let _ = self.mailbox.send("manager", "task_needs_info", payload);
    }

    /// Find ready tasks and dispatch to idle developers.
    pub async fn try_dispatch(
        &mut self,
        developer_count: u8,
        active_handles: &HashMap<String, tokio::task::JoinHandle<()>>,
    ) {
        let idle_devs = find_idle_developers(developer_count, &self.dev_tasks, active_handles);
        if idle_devs.is_empty() {
            return;
        }
        let tasks = match self.db.ready_tasks().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to query ready tasks: {}", e);
                return;
            }
        };
        for (task, dev) in tasks.iter().zip(idle_devs.iter()) {
            self.dispatch_one(task, dev).await;
        }
    }

    async fn dispatch_one(&mut self, task: &llm_tasks::db::Task, dev: &str) {
        if let Err(e) = self.db.claim_task(&task.id, dev).await {
            tracing::warn!("Failed to claim task {} for {}: {}", task.id, dev, e);
            return;
        }
        self.dev_tasks.insert(dev.to_string(), task.id.clone());
        let desc = task.description.as_deref().unwrap_or("");
        let content = format!("## Task {}\n\n{}\n\n{}", task.id, task.title, desc);
        let payload = serde_json::json!({"content": content, "task_id": task.id});
        if let Err(e) = self.mailbox.send(dev, "task_assignment", payload) {
            tracing::error!("Failed to dispatch task {} to {}: {}", task.id, dev, e);
        } else {
            tracing::info!("Dispatched task {} to {}", task.id, dev);
        }
    }

    /// Remove a developer from tracking (e.g. when aborted).
    pub fn remove_developer(&mut self, name: &str) {
        self.dev_tasks.remove(name);
    }
}

fn find_idle_developers(
    developer_count: u8,
    dev_tasks: &HashMap<String, String>,
    active_handles: &HashMap<String, tokio::task::JoinHandle<()>>,
) -> Vec<String> {
    (0..developer_count)
        .map(|i| format!("developer-{}", i))
        .filter(|name| !dev_tasks.contains_key(name))
        .filter(|name| active_handles.contains_key(name))
        .collect()
}
