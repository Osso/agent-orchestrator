//! Orchestrator runtime that manages agent lifecycle and state
//!
//! The runtime:
//! - Creates the in-process message bus
//! - Spawns and tracks agent processes (one per task)
//! - Listens for runtime commands via its mailbox
//! - Persists task history via llm-tasks

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_bus::Bus;
use anyhow::{Context, Result};
use llm_sdk::session::SessionStore;
use llm_tasks::db::Database;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentConfig, BackendKind};
use crate::architect_client;
use crate::control;
use crate::dispatch::Dispatcher;
use crate::relay::{self, RelayServer};
use crate::runtime_support::{self as support, CommandTimers};
use crate::types::{AgentId, AgentRole};
use crate::worktree::{self, WorktreeConfig};

pub struct RuntimeState {
    pub max_concurrent: usize,
}

/// Factory function that creates an Agent from config + mailbox.
/// Tests inject a factory that uses FakeCompleter instead of real Claude.
pub type AgentFactory = Arc<dyn Fn(AgentConfig, agent_bus::Mailbox) -> Result<Agent> + Send + Sync>;

/// Default factory: creates a real Agent backed by the configured backend.
fn default_agent_factory() -> AgentFactory {
    Arc::new(Agent::new)
}

pub struct OrchestratorRuntime {
    pub state: RuntimeState,
    pub bus: Bus,
    pub(crate) db: Arc<Database>,
    pub(crate) session_store: SessionStore,
    pub(crate) working_dir: String,
    pub(crate) project: String,
    agent_handles: HashMap<String, JoinHandle<()>>,
    agent_factory: AgentFactory,
    pub backend: BackendKind,
    pub(crate) no_sandbox: bool,
    pub(crate) dispatcher: Dispatcher,
}

impl OrchestratorRuntime {
    pub async fn new(
        db_path: &Path,
        working_dir: String,
        backend: BackendKind,
        no_sandbox: bool,
    ) -> Result<Self> {
        let db = Database::open(db_path)
            .await
            .context("Failed to open task database")?;

        let project = Path::new(&working_dir)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default")
            .to_string();
        let session_store = SessionStore::new("agent-orchestrator", &project);

        let db = Arc::new(db);
        let bus = Bus::new();
        let dispatch_mailbox = bus
            .register("dispatcher")
            .map_err(|e| anyhow::anyhow!("Failed to register dispatcher: {}", e))?;
        let dispatcher = Dispatcher::new(db.clone(), dispatch_mailbox);

        Ok(Self {
            state: support::default_state(),
            bus,
            db,
            session_store,
            working_dir,
            project,
            agent_handles: HashMap::new(),
            agent_factory: default_agent_factory(),
            backend,
            no_sandbox,
            dispatcher,
        })
    }

    /// Create a runtime suitable for testing.
    pub async fn new_test(
        bus: Bus,
        working_dir: &str,
        factory: AgentFactory,
        backend: BackendKind,
    ) -> Result<Self> {
        let (db, session_store) = support::open_test_stores().await?;
        let db = Arc::new(db);
        let dispatch_mailbox = bus
            .register("dispatcher")
            .map_err(|e| anyhow::anyhow!("Failed to register dispatcher: {}", e))?;
        let dispatcher = Dispatcher::new(db.clone(), dispatch_mailbox);

        Ok(Self {
            state: support::default_state(),
            bus,
            db,
            session_store,
            working_dir: working_dir.to_string(),
            project: "test".to_string(),
            agent_handles: HashMap::new(),
            agent_factory: factory,
            backend,
            no_sandbox: true,
            dispatcher,
        })
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn db(&self) -> Arc<Database> {
        self.db.clone()
    }

    pub async fn run_watchdog_and_dispatch(&mut self) {
        self.dispatcher.watchdog().await;
        self.poll_dispatch().await;
    }

    /// Insert a fake agent handle for testing.
    pub fn insert_fake_handle(&mut self, name: &str) {
        let handle = tokio::spawn(std::future::pending::<()>());
        self.agent_handles.insert(name.to_string(), handle);
    }

    /// Run in standalone mode.
    pub async fn run(self, _initial_task: Option<String>) -> Result<()> {
        let registry = control::new_registry();
        registry.write().unwrap().insert(self.project.clone(), self.bus.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(control::run_control_server(
            registry,
            shutdown_tx.clone(),
            shutdown_rx,
        ));

        self.run_with_shutdown(shutdown_tx).await
    }

    /// Run as part of a daemon.
    pub async fn run_managed(self, shutdown_tx: watch::Sender<bool>) -> Result<()> {
        self.run_with_shutdown(shutdown_tx).await
    }

    async fn run_with_shutdown(
        mut self,
        shutdown_tx: watch::Sender<bool>,
    ) -> Result<()> {
        let mut mailbox = self
            .bus
            .register("runtime")
            .map_err(|e| anyhow::anyhow!("Failed to register runtime: {}", e))?;

        llm_sdk::sandbox::ensure_state_dirs();
        self.start_relay();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.resume_in_progress_tasks().await;
        self.poll_dispatch().await;
        self.command_loop(&mut mailbox, shutdown_tx).await
    }

    fn start_relay(&self) {
        let relay = RelayServer::new(self.bus.clone(), self.db.clone());
        let socket_path = relay::relay_socket_path(&self.project);
        tokio::spawn(async move {
            if let Err(e) = relay.run(&socket_path).await {
                tracing::error!("Relay server error: {}", e);
            }
        });
    }

    async fn command_loop(
        &mut self,
        mailbox: &mut agent_bus::Mailbox,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
    ) -> Result<()> {
        let mut timers = CommandTimers::new()?;
        let mut shutdown_rx = shutdown_tx.subscribe();

        loop {
            tokio::select! {
                msg = mailbox.recv() => {
                    let Some(msg) = msg else { break };
                    if self.handle_message(&msg.kind, &msg.payload, &msg.from).await {
                        break;
                    }
                }
                _ = timers.poll.tick() => self.poll_dispatch().await,
                _ = timers.timeout.tick() => self.check_agent_timeouts().await,
                _ = timers.watchdog.tick() => self.run_watchdog().await,
                _ = timers.sigint.recv() => { tracing::info!("Received SIGINT, shutting down"); break; }
                _ = timers.sigterm.recv() => { tracing::info!("Received SIGTERM, shutting down"); break; }
                Ok(_) = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        tracing::info!("Received shutdown signal from daemon");
                        break;
                    }
                }
            }
        }

        let _ = shutdown_tx.send(true);
        self.shutdown().await;
        Ok(())
    }

    async fn poll_dispatch(&mut self) {
        let task_ids = self.dispatcher.tasks_to_dispatch(self.state.max_concurrent).await;
        for task_id in task_ids {
            if let Err(e) = self.spawn_task_agent(&task_id).await {
                tracing::error!("Failed to spawn agent for task {}: {}", task_id, e);
            }
        }
    }

    async fn handle_task_event(&mut self, kind: &str, payload: &serde_json::Value, from: &str) {
        match kind {
            "task_created" => {
                let task_id = support::payload_str(payload, "task_id");
                self.spawn_architect_validation(&task_id).await;
            }
            "task_complete" => {
                let content = support::payload_str(payload, "content");
                let task_id = self.resolve_task_id(from);
                if let Some(task_id) = task_id {
                    let agent_name = from.to_string();
                    if self.dispatcher.handle_agent_complete(&task_id, &content).await {
                        self.spawn_completion_review(&task_id, &content, &agent_name);
                    }
                }
            }
            "task_done" => {
                let task_id = support::payload_str(payload, "task_id");
                self.merge_agent_branch(&task_id).await;
            }
            "task_blocked" => {
                let content = support::payload_str(payload, "content");
                if let Some(task_id) = self.resolve_task_id(from) {
                    self.dispatcher.handle_agent_blocked(&task_id, &content).await;
                }
            }
            "agent_heartbeat" => {
                let agent = support::payload_str(payload, "agent");
                if !agent.is_empty() { self.dispatcher.record_activity(&agent); }
                return;
            }
            _ => {}
        }
        self.poll_dispatch().await;
    }

    /// Resolve task ID from an agent's bus name.
    fn resolve_task_id(&self, from: &str) -> Option<String> {
        self.dispatcher.task_id_for_agent_name(from)
            .or_else(|| from.strip_prefix("task-").map(String::from))
    }

    async fn check_agent_timeouts(&mut self) {
        let timed_out = self.dispatcher.check_timeouts().await;
        for agent_name in &timed_out {
            tracing::warn!("Aborting timed-out agent {}", agent_name);
            self.abort_agent(agent_name);
        }
        if !timed_out.is_empty() {
            self.poll_dispatch().await;
        }
    }

    async fn run_watchdog(&mut self) {
        let fixed = self.dispatcher.watchdog().await;
        if fixed > 0 {
            self.poll_dispatch().await;
        }
    }

    async fn spawn_architect_validation(&self, task_id: &str) {
        let task = match self.db.get_task(task_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to get task {task_id} for validation: {e}");
                return;
            }
        };
        architect_client::spawn_validation(
            self.db.clone(), self.bus.clone(),
            self.project.clone(), self.working_dir.clone(), task,
        );
    }

    /// Spawn a fresh agent for a task.
    async fn spawn_task_agent(&mut self, task_id: &str) -> Result<()> {
        let agent_id = AgentId::for_task(task_id);
        let bus_name = agent_id.bus_name();

        if !self.dispatcher.claim_and_register(task_id, &bus_name).await {
            return Ok(());
        }

        let config = self.build_task_agent_config(agent_id)?;
        self.spawn_agent_with_config(config)?;
        self.send_task_assignment(task_id, &bus_name).await;
        Ok(())
    }

    fn build_task_agent_config(&self, agent_id: AgentId) -> Result<AgentConfig> {
        let bus_name = agent_id.bus_name();
        let (working_dir, sandbox_prefix) = self.working_dir_for_task(&bus_name);
        let bus = match self.backend {
            BackendKind::OpenRouter { .. } | BackendKind::Codex { .. } => Some(self.bus.clone()),
            BackendKind::Claude => None,
        };
        Ok(AgentConfig {
            agent_id,
            working_dir,
            system_prompt: AgentRole::TaskAgent.system_prompt().to_string(),
            initial_task: None,
            mcp_config: Some(support::build_mcp_config(&bus_name, &self.project)),
            fresh_session_per_task: true,
            backend: self.backend.clone(),
            session_store: self.session_store.clone(),
            bus,
            sandbox_prefix,
        })
    }

    async fn send_task_assignment(&self, task_id: &str, bus_name: &str) {
        let task = match self.db.get_task(task_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to get task {task_id} for assignment: {e}");
                return;
            }
        };
        let desc = task.description.as_deref().unwrap_or("");
        let branch = format!("agent/{}", bus_name);
        let content = format!(
            "## Task {}\n\n{}\n\n{}\n\nCommit your changes on branch `{}`.",
            task_id, task.title, desc, branch
        );
        let payload = serde_json::json!({"content": content, "task_id": task_id});
        if let Err(e) = self.dispatcher.notify(bus_name, "task_assignment", payload) {
            tracing::error!("Failed to dispatch task {} to {}: {}", task_id, bus_name, e);
        } else {
            tracing::info!("Dispatched task {} to {}", task_id, bus_name);
        }
    }

    /// Look up the task's assignee and send merge_request to the merger.
    async fn merge_agent_branch(&mut self, task_id: &str) {
        let task = match self.db.get_task(task_id).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("Failed to get task {task_id} for merge: {e}");
                return;
            }
        };
        if task.status == "pending_delete" {
            tracing::info!("Task {task_id} is pending_delete, skipping merge and closing");
            let _ = self.db.close_task(task_id, "runtime").await;
            return;
        }
        let assignee = task.assignee.unwrap_or_default();
        if assignee.is_empty() || !assignee.starts_with("task-") {
            tracing::warn!("Task {task_id} has no task agent assignee, skipping merge");
            return;
        }
        self.ensure_merger();
        let branch = format!("agent/{}", assignee);
        let payload = serde_json::json!({
            "branch": branch,
            "description": format!("Merge reviewed task {task_id}"),
            "from_agent": assignee,
        });
        if let Err(e) = self.dispatcher.notify("merger", "merge_request", payload) {
            tracing::error!("Failed to send merge_request for {task_id}: {e}");
        }
    }

    fn spawn_completion_review(&self, task_id: &str, dev_output: &str, agent_name: &str) {
        let branch = format!("agent/{}", agent_name);
        architect_client::spawn_review(
            self.db.clone(), self.bus.clone(),
            self.project.clone(), self.working_dir.clone(),
            task_id.to_string(), dev_output.to_string(), branch,
        );
    }

    /// Returns true if the orchestrator should shut down.
    pub async fn handle_message(
        &mut self,
        kind: &str,
        payload: &serde_json::Value,
        from: &str,
    ) -> bool {
        tracing::info!("Runtime received '{}' from {}", kind, from);
        match kind {
            "task_created" | "task_ready" | "task_done"
            | "task_complete" | "task_blocked" | "agent_heartbeat" => {
                self.handle_task_event(kind, payload, from).await;
            }
            _ => tracing::debug!("Runtime ignoring unknown kind: {}", kind),
        }
        false
    }

    async fn shutdown(&mut self) {
        let in_progress = self.db.list_tasks(Some("in_progress"), None).await.unwrap_or_default();
        for task in &in_progress {
            tracing::info!(
                "Preserving in_progress task {} (assignee: {:?}) for resume",
                task.id, task.assignee
            );
        }
        if let Ok(tasks) = self.db.list_tasks(Some("pending_delete"), None).await {
            for task in &tasks {
                tracing::info!("Closing pending_delete task {} on shutdown", task.id);
                let _ = self.db.close_task(&task.id, "runtime").await;
            }
        }
        tracing::info!("Shutting down {} agents", self.agent_handles.len());
        let merger_handle = self.agent_handles.remove("merger");
        for (name, handle) in self.agent_handles.drain() {
            tracing::info!("Stopping {}", name);
            handle.abort();
        }
        if let Some(handle) = merger_handle {
            tracing::info!("Waiting for merger to finish (30s timeout)");
            match tokio::time::timeout(Duration::from_secs(30), handle).await {
                Ok(_) => tracing::info!("Merger finished cleanly"),
                Err(_) => tracing::warn!("Merger timed out, aborting"),
            }
        }
        self.try_remove_worktree("merger");
    }

    fn ensure_merger(&mut self) {
        let name = "merger";
        if self.agent_handles.get(name).is_some_and(|h| !h.is_finished()) {
            return;
        }
        tracing::info!("Lazy-spawning merger (on demand)");
        if let Err(e) = self.spawn_merger() {
            tracing::error!("Failed to spawn merger: {}", e);
        }
    }

    fn spawn_merger(&mut self) -> Result<()> {
        let agent_id = AgentId::merger();
        let bus_name = agent_id.bus_name();
        let (working_dir, sandbox_prefix) = self.working_dir_for_task(&bus_name);
        let bus = match self.backend {
            BackendKind::OpenRouter { .. } | BackendKind::Codex { .. } => Some(self.bus.clone()),
            BackendKind::Claude => None,
        };
        let config = AgentConfig {
            agent_id,
            working_dir,
            system_prompt: AgentRole::Merger.system_prompt().to_string(),
            initial_task: None,
            mcp_config: Some(support::build_mcp_config(&bus_name, &self.project)),
            fresh_session_per_task: false,
            backend: self.backend.clone(),
            session_store: self.session_store.clone(),
            bus,
            sandbox_prefix,
        };
        self.spawn_agent_with_config(config)
    }

    fn working_dir_for_task(&self, bus_name: &str) -> (String, Vec<String>) {
        let use_sandbox = !self.no_sandbox && llm_sdk::sandbox::is_available();
        let project_path = PathBuf::from(&self.working_dir);

        let worktree_result = if support::is_worktree_role(bus_name) || bus_name == "merger" {
            let cfg = WorktreeConfig {
                project_dir: project_path.clone(),
                agent_name: bus_name.to_string(),
            };
            worktree::create_worktree(&cfg).map_err(|e| {
                tracing::warn!("Failed to create worktree for {}, using project dir: {}", bus_name, e);
                e
            })
        } else {
            Err(anyhow::anyhow!("not a worktree role"))
        };

        support::resolve_sandbox(AgentRole::TaskAgent, &project_path, worktree_result, use_sandbox)
    }

    pub(crate) fn spawn_agent_with_config(&mut self, config: AgentConfig) -> Result<()> {
        let bus_name = config.agent_id.bus_name();
        let mailbox = self
            .bus
            .register(&bus_name)
            .map_err(|e| anyhow::anyhow!("Failed to register {}: {}", bus_name, e))?;

        let agent_id = config.agent_id.clone();
        let factory = self.agent_factory.clone();
        let handle = tokio::spawn(async move {
            match factory(config, mailbox) {
                Ok(agent) => {
                    if let Err(e) = agent.run().await {
                        tracing::error!("Agent {} error: {}", agent_id, e);
                    }
                }
                Err(e) => tracing::error!("Agent {} init failed: {}", agent_id, e),
            }
        });

        self.agent_handles.insert(bus_name, handle);
        Ok(())
    }

    fn abort_agent(&mut self, name: &str) {
        if let Some(handle) = self.agent_handles.remove(name) {
            tracing::info!("Stopping {}", name);
            handle.abort();
            let relay_name = format!("relay-{}", name);
            self.bus.deregister(&relay_name);
            self.dispatcher.remove_task_by_agent(name);
            self.try_remove_worktree(name);
        }
    }

    fn try_remove_worktree(&self, bus_name: &str) {
        if !support::is_worktree_role(bus_name) && bus_name != "merger" {
            return;
        }
        let cfg = WorktreeConfig {
            project_dir: PathBuf::from(&self.working_dir),
            agent_name: bus_name.to_string(),
        };
        if let Err(e) = worktree::remove_worktree(&cfg) {
            tracing::warn!("Failed to remove worktree for {}: {}", bus_name, e);
        }
    }
}
