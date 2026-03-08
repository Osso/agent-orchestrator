//! Task dispatch logic: spawns one agent per ready task.
//!
//! The runtime calls into this module when task state changes.
//! It finds ready (unassigned) tasks and returns task IDs to spawn agents for.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_bus::Mailbox;
use llm_tasks::db::{Database, TaskUpdates};

/// How long a task agent can be idle before its task is reclaimed.
pub const AGENT_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

struct TaskAssignment {
    agent_name: String,
    last_activity: Instant,
}

/// Tracks active task agents and handles dispatch + state transitions.
pub struct Dispatcher {
    db: Arc<Database>,
    mailbox: Mailbox,
    /// task_id → assignment (agent_name + last activity)
    active_tasks: HashMap<String, TaskAssignment>,
}

impl Dispatcher {
    pub fn new(db: Arc<Database>, mailbox: Mailbox) -> Self {
        Self { db, mailbox, active_tasks: HashMap::new() }
    }

    /// Agent signals task complete → set in_review. Returns task_id for review.
    /// If the task is `pending_delete`, skips the review and closes it immediately.
    pub async fn handle_agent_complete(&mut self, task_id: &str, content: &str) -> bool {
        if self.active_tasks.remove(task_id).is_none() {
            return false;
        }
        if self.is_pending_delete(task_id).await {
            tracing::info!("Task {} is pending_delete, skipping review", task_id);
            let _ = self.db.close_task(task_id, "runtime").await;
            return false;
        }
        // Add agent output as comment
        let short = if content.len() > 2000 { &content[..2000] } else { content };
        let _ = self.db.add_comment(task_id, "agent", short).await;
        self.transition_to_review(task_id).await;
        true
    }

    /// Agent signals blocked/needs_info.
    pub async fn handle_agent_blocked(&mut self, task_id: &str, content: &str) {
        if self.active_tasks.remove(task_id).is_none() {
            return;
        }
        self.transition_to_needs_info(task_id, content).await;
    }

    /// Record activity from a task agent (called on every relay tool call).
    pub fn record_activity(&mut self, agent_name: &str) {
        if let Some(task_id) = self.task_id_for_agent(agent_name) {
            if let Some(assignment) = self.active_tasks.get_mut(&task_id) {
                assignment.last_activity = Instant::now();
            }
        }
    }

    /// Check for timed-out agents. Returns agent names that timed out.
    pub async fn check_timeouts(&mut self) -> Vec<String> {
        let timed_out: Vec<(String, String)> = self
            .active_tasks
            .iter()
            .filter(|(_, a)| a.last_activity.elapsed() > AGENT_IDLE_TIMEOUT)
            .map(|(tid, a)| (tid.clone(), a.agent_name.clone()))
            .collect();

        let mut aborted = Vec::new();
        for (task_id, agent_name) in timed_out {
            tracing::warn!(
                "Agent {} timed out on task {} (idle {:?})",
                agent_name,
                task_id,
                self.active_tasks[&task_id].last_activity.elapsed(),
            );
            let updates = TaskUpdates { status: Some("ready"), ..Default::default() };
            if let Err(e) = self.db.update_task(&task_id, updates, "runtime").await {
                tracing::error!("Failed to reclaim task {} from {}: {}", task_id, agent_name, e);
            }
            let _ = self.db.clear_assignee(&task_id, "runtime").await;
            self.active_tasks.remove(&task_id);
            aborted.push(agent_name);
        }
        aborted
    }

    /// Watchdog: fix stuck tasks. Returns number of tasks fixed.
    pub async fn watchdog(&mut self) -> usize {
        let mut fixed = 0;
        fixed += self.reclaim_orphaned_in_progress().await;
        fixed += self.clear_stale_assignees().await;
        fixed += self.close_pending_deletes().await;
        if fixed > 0 {
            tracing::info!("Watchdog fixed {} stuck tasks", fixed);
        }
        fixed
    }

    /// Find ready tasks that can be dispatched. Returns task IDs.
    pub async fn tasks_to_dispatch(&self, max_concurrent: usize) -> Vec<String> {
        let available = max_concurrent.saturating_sub(self.active_tasks.len());
        if available == 0 {
            return Vec::new();
        }
        let tasks = match self.db.ready_tasks().await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to query ready tasks: {}", e);
                return Vec::new();
            }
        };
        tasks.iter().take(available).map(|t| t.id.clone()).collect()
    }

    /// Claim a task in the DB and register it as active.
    pub async fn claim_and_register(&mut self, task_id: &str, agent_name: &str) -> bool {
        if let Err(e) = self.db.claim_task(task_id, agent_name).await {
            tracing::warn!("Failed to claim task {} for {}: {}", task_id, agent_name, e);
            return false;
        }
        self.active_tasks.insert(task_id.to_string(), TaskAssignment {
            agent_name: agent_name.to_string(),
            last_activity: Instant::now(),
        });
        true
    }

    /// Register a resumed task agent (from a previous session).
    pub fn register_active(&mut self, task_id: String, agent_name: String) {
        self.active_tasks.insert(task_id, TaskAssignment {
            agent_name,
            last_activity: Instant::now(),
        });
    }

    /// Remove a task from tracking (e.g. when agent is aborted).
    pub fn remove_task_by_agent(&mut self, agent_name: &str) {
        if let Some(task_id) = self.task_id_for_agent(agent_name) {
            self.active_tasks.remove(&task_id);
        }
    }

    /// Send a message via the dispatcher's mailbox.
    pub fn notify(&self, to: &str, kind: &str, payload: serde_json::Value) -> Result<(), String> {
        self.mailbox.send(to, kind, payload).map(|_| ()).map_err(|e| e.to_string())
    }

    /// Look up task_id from agent bus name.
    fn task_id_for_agent(&self, agent_name: &str) -> Option<String> {
        self.active_tasks
            .iter()
            .find(|(_, a)| a.agent_name == agent_name)
            .map(|(tid, _)| tid.clone())
    }

    /// Look up task_id from agent bus name (public for runtime).
    pub fn task_id_for_agent_name(&self, agent_name: &str) -> Option<String> {
        self.task_id_for_agent(agent_name)
    }

    async fn reclaim_orphaned_in_progress(&mut self) -> usize {
        let tasks = match self.db.list_tasks(Some("in_progress"), None).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to query in_progress tasks: {}", e);
                return 0;
            }
        };
        let mut count = 0;
        for task in &tasks {
            if self.active_tasks.contains_key(&task.id) {
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

    async fn clear_stale_assignees(&self) -> usize {
        let mut count = 0;
        for status in &["pending", "ready"] {
            count += self.clear_assignees_for_status(status).await;
        }
        count
    }

    async fn clear_assignees_for_status(&self, status: &str) -> usize {
        let tasks = match self.db.list_tasks(Some(status), None).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to query {} tasks: {}", status, e);
                return 0;
            }
        };
        let mut count = 0;
        for task in &tasks {
            if task.assignee.is_none() {
                continue;
            }
            tracing::warn!("Clearing stale assignee on {} task {} ({:?})", status, task.id, task.assignee);
            if let Err(e) = self.db.clear_assignee(&task.id, "runtime").await {
                tracing::error!("Failed to clear assignee on task {}: {}", task.id, e);
            } else {
                count += 1;
            }
        }
        count
    }

    async fn is_pending_delete(&self, task_id: &str) -> bool {
        matches!(self.db.get_task(task_id).await, Ok(t) if t.status == "pending_delete")
    }

    async fn close_pending_deletes(&mut self) -> usize {
        let tasks = match self.db.list_tasks(Some("pending_delete"), None).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to query pending_delete tasks: {}", e);
                return 0;
            }
        };
        let mut count = 0;
        for task in &tasks {
            if self.active_tasks.contains_key(&task.id) {
                continue;
            }
            tracing::info!("Closing pending_delete task {} (no active agent)", task.id);
            if let Err(e) = self.db.close_task(&task.id, "runtime").await {
                tracing::error!("Failed to close pending_delete task {}: {}", task.id, e);
                continue;
            }
            count += 1;
        }
        count
    }

    async fn transition_to_review(&self, task_id: &str) {
        let updates = TaskUpdates { status: Some("in_review"), ..Default::default() };
        if let Err(e) = self.db.update_task(task_id, updates, "runtime").await {
            tracing::error!("Failed to set task {} in_review: {}", task_id, e);
        }
    }

    async fn transition_to_needs_info(&self, task_id: &str, question: &str) {
        let updates = TaskUpdates { status: Some("needs_info"), ..Default::default() };
        if let Err(e) = self.db.update_task(task_id, updates, "runtime").await {
            tracing::error!("Failed to set task {} needs_info: {}", task_id, e);
        }
        if let Err(e) = self.db.add_comment(task_id, "agent", question).await {
            tracing::error!("Failed to add needs_info comment on {}: {}", task_id, e);
        }
    }
}
