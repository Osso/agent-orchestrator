//! Orchestrator runtime that manages agent lifecycle and state
//!
//! The runtime:
//! - Creates the in-process message bus
//! - Spawns and tracks agent processes
//! - Listens for runtime commands via its mailbox (CREW sizing, RELIEVE manager)
//! - Persists task history via llm-tasks

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_bus::{Bus, Mailbox};
use anyhow::{Context, Result};
use llm_sdk::session::SessionStore;
use llm_tasks::db::Database;
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentConfig, BackendKind};
use crate::control;
use crate::dispatch::Dispatcher;
use crate::relay::{self, RelayServer};
use crate::types::{AgentId, AgentRole};
use crate::worktree::{self, WorktreeConfig};

pub const RELIEVE_COOLDOWN: Duration = Duration::from_secs(60);
pub const NUDGE_INTERVAL: Duration = Duration::from_secs(120);
pub const NUDGE_IDLE_THRESHOLD: Duration = Duration::from_secs(90);

pub struct RuntimeState {
    pub developer_count: u8,
    pub manager_generation: u32,
    pub last_relieve: Option<Instant>,
    pub last_activity: Instant,
}

/// Factory function that creates an Agent from config + mailbox.
/// Tests inject a factory that uses FakeCompleter instead of real Claude.
pub type AgentFactory = Arc<dyn Fn(AgentConfig, Mailbox) -> Result<Agent> + Send + Sync>;

/// Default factory: creates a real Agent backed by the configured backend.
fn default_agent_factory() -> AgentFactory {
    Arc::new(Agent::new)
}

pub struct OrchestratorRuntime {
    pub state: RuntimeState,
    pub bus: Bus,
    db: Arc<Database>,
    session_store: SessionStore,
    working_dir: String,
    project: String,
    agent_handles: HashMap<String, JoinHandle<()>>,
    agent_factory: AgentFactory,
    pub backend: BackendKind,
    no_sandbox: bool,
    dispatcher: Dispatcher,
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
            state: default_state(),
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

    /// Create a runtime suitable for testing — uses tempdir for DB + sessions,
    /// and an injected agent factory.
    pub async fn new_test(
        bus: Bus,
        working_dir: &str,
        factory: AgentFactory,
        backend: BackendKind,
    ) -> Result<Self> {
        let (db, session_store) = open_test_stores().await?;
        let db = Arc::new(db);
        let dispatch_mailbox = bus
            .register("dispatcher")
            .map_err(|e| anyhow::anyhow!("Failed to register dispatcher: {}", e))?;
        let dispatcher = Dispatcher::new(db.clone(), dispatch_mailbox);

        Ok(Self {
            state: default_state(),
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

    pub async fn run(mut self, initial_task: Option<String>) -> Result<()> {
        let mut mailbox = self
            .bus
            .register("runtime")
            .map_err(|e| anyhow::anyhow!("Failed to register runtime: {}", e))?;

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        llm_sdk::sandbox::ensure_state_dirs();
        self.start_relay();
        self.start_control_server(shutdown_tx.clone(), shutdown_rx);
        // Brief pause for relay and control to bind their sockets before agents connect
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let enriched_task = self.enrich_with_task_state(initial_task).await;
        self.spawn_initial_agents(enriched_task)?;
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

    fn start_control_server(
        &self,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
        shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let bus = self.bus.clone();
        let project = self.project.clone();
        tokio::spawn(control::run_control_server(bus, project, shutdown_tx, shutdown_rx));
    }

    /// Process a single bus message. Returns true if the orchestrator should exit.
    async fn process_bus_message(
        &mut self,
        msg: Option<agent_bus::BusMessage>,
    ) -> Option<bool> {
        let msg = msg?;
        self.state.last_activity = Instant::now();
        let should_exit = self
            .handle_message(&msg.kind, &msg.payload, &msg.from)
            .await;
        Some(should_exit)
    }

    async fn command_loop(
        &mut self,
        mailbox: &mut agent_bus::Mailbox,
        shutdown_tx: tokio::sync::watch::Sender<bool>,
    ) -> Result<()> {
        let mut timers = CommandTimers::new()?;

        loop {
            tokio::select! {
                msg = mailbox.recv() => {
                    match self.process_bus_message(msg).await {
                        Some(true) | None => break,
                        _ => {}
                    }
                }
                _ = timers.timeout.tick() => self.check_dev_timeouts().await,
                _ = timers.nudge.tick() => self.maybe_nudge_manager(mailbox).await,
                _ = timers.audit.tick() => self.send_audit_snapshot(mailbox).await,
                _ = timers.sigint.recv() => { tracing::info!("Received SIGINT, shutting down"); break; }
                _ = timers.sigterm.recv() => { tracing::info!("Received SIGTERM, shutting down"); break; }
            }
        }

        let _ = shutdown_tx.send(true);
        self.shutdown().await;
        Ok(())
    }

    async fn enrich_with_task_state(&self, task: Option<String>) -> Option<String> {
        let task = task?;
        let snapshot = self.build_task_snapshot().await;
        if snapshot.contains("No tasks") {
            Some(task)
        } else {
            Some(format!("{}\n\n## Your Task\n{}", snapshot, task))
        }
    }

    async fn send_audit_snapshot(&self, mailbox: &Mailbox) {
        let snapshot = self.build_task_snapshot().await;
        let payload = serde_json::json!({ "content": snapshot });
        if let Err(e) = mailbox.send("auditor", "patrol_snapshot", payload) {
            tracing::warn!("Failed to send audit snapshot: {}", e);
        }
    }

    async fn maybe_nudge_manager(&self, mailbox: &Mailbox) {
        if self.state.last_activity.elapsed() < NUDGE_IDLE_THRESHOLD {
            return;
        }
        let has_tasks = matches!(self.db.list_tasks(None, None).await, Ok(t) if !t.is_empty());
        if !has_tasks {
            return;
        }
        let snapshot = self.build_task_snapshot().await;
        let idle_secs = self.state.last_activity.elapsed().as_secs();
        let payload = serde_json::json!({
            "content": format!(
                "NUDGE: No activity for {}s. Tasks exist but none are being processed.\n\n{}",
                idle_secs, snapshot
            )
        });
        tracing::info!("Nudging manager (idle {}s with pending tasks)", idle_secs);
        if let Err(e) = mailbox.send("manager", "nudge", payload) {
            tracing::debug!("Failed to nudge manager: {}", e);
        }
    }

    async fn check_dev_timeouts(&mut self) {
        let timed_out = self.dispatcher.check_timeouts().await;
        for dev_name in &timed_out {
            tracing::warn!("Aborting timed-out developer {}", dev_name);
            self.abort_agent(dev_name);
        }
        if !timed_out.is_empty() {
            self.dispatcher.try_dispatch(self.state.developer_count, &self.agent_handles).await;
        }
    }

    async fn build_task_snapshot(&self) -> String {
        let mut snap = String::from("## Task State Snapshot\n\n");
        match self.db.list_tasks(None, None).await {
            Ok(tasks) if tasks.is_empty() => {
                snap.push_str("No tasks recorded yet.\n");
            }
            Ok(tasks) => {
                for task in &tasks {
                    let who = task.assignee.as_deref().unwrap_or("unassigned");
                    snap.push_str(&format!(
                        "- [{}] {} (assigned: {})\n",
                        task.status, task.title, who
                    ));
                }
                snap.push_str(&format!("\nTotal tasks: {}\n", tasks.len()));
            }
            Err(e) => {
                snap.push_str(&format!("Error querying tasks: {}\n", e));
            }
        }
        snap
    }

    /// Returns true if the orchestrator should shut down (goal_complete).
    pub async fn handle_message(
        &mut self,
        kind: &str,
        payload: &serde_json::Value,
        from: &str,
    ) -> bool {
        tracing::info!("Runtime received '{}' from {}", kind, from);
        match kind {
            "goal_complete" => {
                let summary = payload_str(payload, "summary");
                tracing::info!("GOAL COMPLETE: {}", summary);
                return true;
            }
            "set_crew" => {
                let count = payload_u8(payload, "count", 1);
                self.handle_crew_size(count);
            }
            "relieve_manager" => {
                let reason = payload_str(payload, "reason");
                self.handle_relieve_manager(&reason).await;
            }
            "task_created" | "task_ready" | "task_done" => {
                self.dispatcher.try_dispatch(self.state.developer_count, &self.agent_handles).await;
            }
            "task_complete" => {
                let content = payload_str(payload, "content");
                self.dispatcher.handle_dev_complete(from, &content).await;
                self.dispatcher.try_dispatch(self.state.developer_count, &self.agent_handles).await;
            }
            "task_blocked" => {
                let content = payload_str(payload, "content");
                self.dispatcher.handle_dev_needs_info(from, &content).await;
                self.dispatcher.try_dispatch(self.state.developer_count, &self.agent_handles).await;
            }
            "dev_heartbeat" => {
                let dev = payload_str(payload, "developer");
                if !dev.is_empty() {
                    self.dispatcher.record_activity(&dev);
                }
            }
            _ => tracing::debug!("Runtime ignoring unknown kind: {}", kind),
        }
        false
    }

    async fn shutdown(&mut self) {
        tracing::info!("Shutting down {} agents", self.agent_handles.len());

        // Extract merger handle — it needs time to finish current merge
        let merger_handle = self.agent_handles.remove("merger");

        let names: Vec<String> = self.agent_handles.keys().cloned().collect();
        for (name, handle) in self.agent_handles.drain() {
            tracing::info!("Stopping {}", name);
            handle.abort();
        }

        // Wait for merger to finish before cleaning up
        if let Some(handle) = merger_handle {
            tracing::info!("Waiting for merger to finish (30s timeout)");
            match tokio::time::timeout(Duration::from_secs(30), handle).await {
                Ok(_) => tracing::info!("Merger finished cleanly"),
                Err(_) => tracing::warn!("Merger timed out, aborting"),
            }
        }

        for name in &names {
            self.try_remove_worktree(name);
        }
        self.try_remove_worktree("merger");
    }

    fn spawn_initial_agents(&mut self, manager_task: Option<String>) -> Result<()> {
        self.spawn_agent(AgentRole::Manager, 0, manager_task)?;
        self.spawn_agent(AgentRole::Architect, 0, None)?;
        self.spawn_agent(AgentRole::Auditor, 0, None)?;
        self.spawn_agent(AgentRole::Merger, 0, None)?;
        self.spawn_agent(AgentRole::Developer, 0, None)?;
        Ok(())
    }

    pub fn spawn_agent(
        &mut self,
        role: AgentRole,
        index: u8,
        initial_task: Option<String>,
    ) -> Result<()> {
        let agent_id = match role {
            AgentRole::Developer => AgentId::new_developer(index),
            _ => AgentId::new_singleton(role),
        };
        let bus_name = agent_id.bus_name();
        let (working_dir, sandbox_prefix) = self.working_dir_and_sandbox(role, &bus_name);
        let fresh = role == AgentRole::Developer;
        let bus = match self.backend {
            BackendKind::OpenRouter { .. } => Some(self.bus.clone()),
            BackendKind::Claude => None,
        };
        let config = AgentConfig {
            agent_id,
            working_dir,
            system_prompt: role.system_prompt().to_string(),
            initial_task,
            mcp_config: Some(build_mcp_config(&bus_name, &self.project)),
            fresh_session_per_task: fresh,
            backend: self.backend.clone(),
            session_store: self.session_store.clone(),
            bus,
            sandbox_prefix,
        };
        self.spawn_agent_with_config(config)
    }

    fn working_dir_and_sandbox(
        &self,
        role: AgentRole,
        bus_name: &str,
    ) -> (String, Vec<String>) {
        let use_sandbox = !self.no_sandbox && llm_sdk::sandbox::is_available();
        let project_path = PathBuf::from(&self.working_dir);

        let worktree_result = if matches!(role, AgentRole::Developer) {
            let cfg = WorktreeConfig {
                project_dir: project_path.clone(),
                agent_name: bus_name.to_string(),
            };
            match worktree::create_worktree(&cfg) {
                Ok(wt_path) => Ok(wt_path),
                Err(e) => {
                    tracing::warn!(
                        "Failed to create worktree for {}, using project dir: {}",
                        bus_name,
                        e
                    );
                    Err(e)
                }
            }
        } else {
            Err(anyhow::anyhow!("not a developer role"))
        };

        resolve_sandbox(role, &project_path, worktree_result, use_sandbox)
    }

    fn spawn_agent_with_config(&mut self, config: AgentConfig) -> Result<()> {
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

    pub fn handle_crew_size(&mut self, count: u8) {
        let count = count.clamp(1, 3);
        let current = self.state.developer_count;
        if count == current {
            return;
        }

        tracing::info!("CREW resize: {} -> {}", current, count);

        if count > current {
            for i in current..count {
                if let Err(e) = self.spawn_agent(AgentRole::Developer, i, None) {
                    tracing::error!("Failed to spawn developer-{}: {}", i, e);
                }
            }
        } else {
            for i in count..current {
                self.abort_agent(&format!("developer-{}", i));
            }
        }

        self.state.developer_count = count;
    }

    pub async fn handle_relieve_manager(&mut self, reason: &str) {
        if !self.relieve_cooldown_elapsed(reason) {
            return;
        }

        tracing::warn!(
            "RELIEVE: firing manager gen {} — {}",
            self.state.manager_generation,
            reason
        );

        self.abort_agent("manager");
        self.session_store.remove("manager");
        self.session_store.remove_message_log("manager");
        self.state.manager_generation += 1;
        self.state.last_relieve = Some(Instant::now());

        let briefing = self.build_manager_briefing(reason).await;
        self.spawn_replacement_manager(briefing);
    }

    fn relieve_cooldown_elapsed(&self, reason: &str) -> bool {
        if let Some(last) = self.state.last_relieve
            && last.elapsed() < RELIEVE_COOLDOWN
        {
            tracing::warn!(
                "RELIEVE rejected ({}): cooldown ({:.0}s remaining)",
                reason,
                (RELIEVE_COOLDOWN - last.elapsed()).as_secs_f64()
            );
            return false;
        }
        true
    }

    fn spawn_replacement_manager(&mut self, briefing: String) {
        let prompt = format!("{}\n\n{}", AgentRole::Manager.system_prompt(), briefing);
        let bus = match self.backend {
            BackendKind::OpenRouter { .. } => Some(self.bus.clone()),
            BackendKind::Claude => None,
        };
        let use_sandbox = !self.no_sandbox && llm_sdk::sandbox::is_available();
        let project_path = PathBuf::from(&self.working_dir);
        let (working_dir, sandbox_prefix) = if use_sandbox {
            let prefix = llm_sdk::sandbox::readonly_prefix(&project_path);
            (llm_sdk::sandbox::REPO_MOUNT.to_string(), prefix)
        } else {
            (self.working_dir.clone(), Vec::new())
        };
        let config = AgentConfig {
            agent_id: AgentId::new_singleton(AgentRole::Manager),
            working_dir,
            system_prompt: prompt,
            initial_task: None,
            mcp_config: Some(build_mcp_config("manager", &self.project)),
            fresh_session_per_task: false,
            backend: self.backend.clone(),
            session_store: self.session_store.clone(),
            bus,
            sandbox_prefix,
        };
        if let Err(e) = self.spawn_agent_with_config(config) {
            tracing::error!("Failed to spawn replacement manager: {}", e);
        }
    }

    fn abort_agent(&mut self, name: &str) {
        if let Some(handle) = self.agent_handles.remove(name) {
            tracing::info!("Stopping {}", name);
            handle.abort();
            // Mailbox drop on the agent task handles deregistration
            self.try_remove_worktree(name);
        }
    }

    fn try_remove_worktree(&self, bus_name: &str) {
        if !is_worktree_role(bus_name) {
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

    async fn build_manager_briefing(&self, relieve_reason: &str) -> String {
        let mut b = String::from("## State Briefing (you are replacing the previous manager)\n\n");
        b.push_str(&format!("**Reason:** {}\n", relieve_reason));
        b.push_str(&format!("**Generation:** {}\n", self.state.manager_generation));
        b.push_str(&format!(
            "**Active developers:** {}\n\n",
            self.state.developer_count
        ));

        match self.db.list_tasks(None, None).await {
            Ok(tasks) if tasks.is_empty() => b.push_str("No task history recorded.\n"),
            Ok(tasks) => {
                b.push_str("### Task History\n");
                for task in &tasks {
                    let who = task.assignee.as_deref().unwrap_or("unassigned");
                    b.push_str(&format!("- [{}] {}: {}\n", who, task.status, task.title));
                }
            }
            Err(e) => b.push_str(&format!("Failed to load task history: {}\n", e)),
        }

        b
    }
}

/// Determine working directory and sandbox prefix for an agent.
///
/// Developer/Merger agents always get a writable sandbox — either the worktree
/// (if creation succeeded) or the project directory as fallback.
/// Non-developer agents get a read-only sandbox.
pub fn resolve_sandbox(
    role: AgentRole,
    project_path: &Path,
    worktree_result: Result<PathBuf>,
    use_sandbox: bool,
) -> (String, Vec<String>) {
    let is_dev = matches!(role, AgentRole::Developer | AgentRole::Merger);

    if is_dev {
        let dev_path = worktree_result.unwrap_or_else(|_| project_path.to_path_buf());
        if use_sandbox {
            let prefix = llm_sdk::sandbox::developer_prefix(&dev_path);
            return (llm_sdk::sandbox::REPO_MOUNT.to_string(), prefix);
        }
        return (dev_path.to_string_lossy().into_owned(), Vec::new());
    }

    if use_sandbox {
        let prefix = llm_sdk::sandbox::readonly_prefix(project_path);
        (llm_sdk::sandbox::REPO_MOUNT.to_string(), prefix)
    } else {
        (project_path.to_string_lossy().into_owned(), Vec::new())
    }
}

fn build_mcp_config(agent_name: &str, project: &str) -> String {
    let socket_path = relay::relay_socket_path(project);
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("agent-orchestrator"))
        .to_string_lossy()
        .into_owned();
    serde_json::json!({
        "mcpServers": {
            "orchestrator": {
                "command": exe,
                "args": ["mcp-serve", "--socket", socket_path.to_string_lossy(), "--agent", agent_name]
            }
        }
    })
    .to_string()
}

struct CommandTimers {
    audit: tokio::time::Interval,
    nudge: tokio::time::Interval,
    timeout: tokio::time::Interval,
    sigint: tokio::signal::unix::Signal,
    sigterm: tokio::signal::unix::Signal,
}

impl CommandTimers {
    fn new() -> Result<Self> {
        let now = tokio::time::Instant::now();
        Ok(Self {
            audit: tokio::time::interval_at(now + Duration::from_secs(60), Duration::from_secs(600)),
            nudge: tokio::time::interval_at(now + NUDGE_INTERVAL, NUDGE_INTERVAL),
            timeout: tokio::time::interval_at(now + Duration::from_secs(60), Duration::from_secs(60)),
            sigint: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            sigterm: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }
}

fn default_state() -> RuntimeState {
    RuntimeState {
        developer_count: 1,
        manager_generation: 0,
        last_relieve: None,
        last_activity: Instant::now(),
    }
}

async fn open_test_stores() -> Result<(Database, SessionStore)> {
    let tmp = std::env::temp_dir().join(format!(
        "orch-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::create_dir_all(&tmp);
    let db = Database::open(&tmp.join("tasks.db"))
        .await
        .context("Failed to open test database")?;
    Ok((db, SessionStore::load(tmp.join("sessions"))))
}

fn payload_str(payload: &serde_json::Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn payload_u8(payload: &serde_json::Value, key: &str, default: u8) -> u8 {
    payload
        .get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default as u64) as u8
}

fn is_worktree_role(bus_name: &str) -> bool {
    bus_name.starts_with("developer-")
}
