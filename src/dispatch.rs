//! Task dispatch logic: matches ready tasks to idle developers.
//!
//! The runtime calls into this module when task state changes.
//! It finds ready (unassigned) tasks, claims them, and sends
//! task_assignment messages to idle developers via the bus.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_bus::Mailbox;
use llm_tasks::db::{Database, TaskUpdates};

/// How long a developer can be idle before its task is reclaimed.
pub const DEV_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

struct DevAssignment {
    task_id: String,
    last_activity: Instant,
}

/// Tracks task assignments and handles dispatch + state transitions.
pub struct Dispatcher {
    db: Arc<Database>,
    mailbox: Mailbox,
    /// developer bus_name → assignment (task_id + last activity)
    dev_tasks: HashMap<String, DevAssignment>,
}

impl Dispatcher {
    pub fn new(db: Arc<Database>, mailbox: Mailbox) -> Self {
        Self { db, mailbox, dev_tasks: HashMap::new() }
    }

    /// Developer signals task complete → set in_review, notify architect.
    pub async fn handle_dev_complete(&mut self, from: &str, content: &str) {
        if let Some(assignment) = self.dev_tasks.remove(from) {
            self.transition_to_review(&assignment.task_id, from, content).await;
        }
    }

    /// Developer signals needs_info → set needs_info, notify manager.
    pub async fn handle_dev_needs_info(&mut self, from: &str, content: &str) {
        if let Some(assignment) = self.dev_tasks.remove(from) {
            self.transition_to_needs_info(&assignment.task_id, from, content).await;
        }
    }

    /// Record activity from a developer (called on every relay tool call).
    pub fn record_activity(&mut self, dev_name: &str) {
        if let Some(assignment) = self.dev_tasks.get_mut(dev_name) {
            assignment.last_activity = Instant::now();
        }
    }

    /// Check for timed-out developers. Returns names of developers that timed out
    /// so the runtime can abort their agents.
    pub async fn check_timeouts(&mut self) -> Vec<String> {
        let timed_out: Vec<(String, String)> = self
            .dev_tasks
            .iter()
            .filter(|(_, a)| a.last_activity.elapsed() > DEV_IDLE_TIMEOUT)
            .map(|(name, a)| (name.clone(), a.task_id.clone()))
            .collect();

        let mut aborted = Vec::new();
        for (dev_name, task_id) in timed_out {
            tracing::warn!(
                "Developer {} timed out on task {} (idle {:?})",
                dev_name,
                task_id,
                self.dev_tasks[&dev_name].last_activity.elapsed(),
            );
            let updates = TaskUpdates { status: Some("ready"), ..Default::default() };
            if let Err(e) = self.db.update_task(&task_id, updates, "runtime").await {
                tracing::error!("Failed to reclaim task {} from {}: {}", task_id, dev_name, e);
            }
            let _ = self.db.clear_assignee(&task_id, "runtime").await;
            self.dev_tasks.remove(&dev_name);
            aborted.push(dev_name);
        }
        aborted
    }

    /// Watchdog: fix stuck tasks. Returns number of tasks fixed.
    ///
    /// - `in_progress` with no tracked developer → reset to `ready`
    /// - `ready` with non-empty assignee → clear assignee so dispatch finds them
    pub async fn watchdog(&mut self) -> usize {
        let mut fixed = 0;
        fixed += self.reclaim_orphaned_in_progress().await;
        fixed += self.clear_stale_ready_assignees().await;
        if fixed > 0 {
            tracing::info!("Watchdog fixed {} stuck tasks", fixed);
        }
        fixed
    }

    async fn reclaim_orphaned_in_progress(&mut self) -> usize {
        let tasks = match self.db.list_tasks(Some("in_progress"), None).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to query in_progress tasks: {}", e);
                return 0;
            }
        };
        let tracked: Vec<&str> =
            self.dev_tasks.values().map(|a| a.task_id.as_str()).collect();
        let mut count = 0;
        for task in &tasks {
            if tracked.contains(&task.id.as_str()) {
                continue;
            }
            tracing::warn!("Reclaiming orphaned task {} (assignee: {:?})", task.id, task.assignee);
            let updates = TaskUpdates { status: Some("ready"), ..Default::default() };
            if let Err(e) = self.db.update_task(&task.id, updates, "runtime").await {
                tracing::error!("Failed to reclaim task {}: {}", task.id, e);
                continue;
            }
            let _ = self.db.clear_assignee(&task.id, "runtime").await;
            count += 1;
        }
        count
    }

    async fn clear_stale_ready_assignees(&self) -> usize {
        let tasks = match self.db.list_tasks(Some("ready"), None).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to query ready tasks: {}", e);
                return 0;
            }
        };
        let mut count = 0;
        for task in &tasks {
            if task.assignee.is_none() {
                continue;
            }
            tracing::warn!("Clearing stale assignee on ready task {} ({:?})", task.id, task.assignee);
            if let Err(e) = self.db.clear_assignee(&task.id, "runtime").await {
                tracing::error!("Failed to clear assignee on task {}: {}", task.id, e);
            } else {
                count += 1;
            }
        }
        count
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
        if tasks.is_empty() {
            return;
        }
        tracing::info!("Dispatching {} tasks to {} idle devs", tasks.len(), idle_devs.len());
        for (task, dev) in tasks.iter().zip(idle_devs.iter()) {
            self.dispatch_one(task, dev).await;
        }
    }

    async fn dispatch_one(&mut self, task: &llm_tasks::db::Task, dev: &str) {
        if let Err(e) = self.db.claim_task(&task.id, dev).await {
            tracing::warn!("Failed to claim task {} for {}: {}", task.id, dev, e);
            return;
        }
        self.dev_tasks.insert(dev.to_string(), DevAssignment {
            task_id: task.id.clone(),
            last_activity: Instant::now(),
        });
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
    dev_tasks: &HashMap<String, DevAssignment>,
    active_handles: &HashMap<String, tokio::task::JoinHandle<()>>,
) -> Vec<String> {
    (0..developer_count)
        .map(|i| format!("developer-{}", i))
        .filter(|name| !dev_tasks.contains_key(name))
        .filter(|name| active_handles.contains_key(name))
        .collect()
}
