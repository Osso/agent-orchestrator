//! Orchestrator runtime that manages agent lifecycle and state
//!
//! The runtime:
//! - Creates the in-process message bus
//! - Spawns and tracks agent processes
//! - Listens for runtime commands via its mailbox (CREW sizing, RELIEVE manager)
//! - Persists task history via llm-tasks

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_bus::Bus;
use anyhow::{Context, Result};
use llm_sdk::session::SessionStore;
use llm_tasks::db::Database;
use tokio::task::JoinHandle;

use crate::agent::{Agent, AgentConfig};
use crate::control;
use crate::relay::{self, RelayServer};
use crate::types::{AgentId, AgentRole};

const RELIEVE_COOLDOWN: Duration = Duration::from_secs(60);

struct RuntimeState {
    developer_count: u8,
    manager_generation: u32,
    last_relieve: Option<Instant>,
}

pub struct OrchestratorRuntime {
    state: RuntimeState,
    bus: Bus,
    db: Database,
    session_store: SessionStore,
    working_dir: String,
    agent_handles: HashMap<String, JoinHandle<()>>,
}

impl OrchestratorRuntime {
    pub async fn new(db_path: &Path, working_dir: String) -> Result<Self> {
        let db = Database::open(db_path)
            .await
            .context("Failed to open task database")?;

        let project = Path::new(&working_dir)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("default");
        let session_store = SessionStore::new("agent-orchestrator", project);

        Ok(Self {
            state: RuntimeState {
                developer_count: 1,
                manager_generation: 0,
                last_relieve: None,
            },
            bus: Bus::new(),
            db,
            session_store,
            working_dir,
            agent_handles: HashMap::new(),
        })
    }

    pub async fn run(mut self, initial_task: Option<String>) -> Result<()> {
        let mut mailbox = self
            .bus
            .register("runtime")
            .map_err(|e| anyhow::anyhow!("Failed to register runtime: {}", e))?;

        self.start_relay();
        // Brief pause for relay to bind its socket before agents connect
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.spawn_initial_agents(initial_task)?;
        self.command_loop(&mut mailbox).await
    }

    fn start_relay(&self) {
        let relay = RelayServer::new(self.bus.clone());
        let socket_path = relay::relay_socket_path();
        tokio::spawn(async move {
            if let Err(e) = relay.run(&socket_path).await {
                tracing::error!("Relay server error: {}", e);
            }
        });
    }

    async fn command_loop(&mut self, mailbox: &mut agent_bus::Mailbox) -> Result<()> {
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

        loop {
            tokio::select! {
                msg = mailbox.recv() => {
                    match msg {
                        Some(msg) => {
                            self.handle_message(&msg.kind, &msg.payload, &msg.from).await;
                        }
                        None => break,
                    }
                }
                _ = sigint.recv() => {
                    tracing::info!("Received SIGINT, shutting down");
                    break;
                }
                _ = sigterm.recv() => {
                    tracing::info!("Received SIGTERM, shutting down");
                    break;
                }
            }
        }

        self.shutdown();
        Ok(())
    }

    async fn handle_message(&mut self, kind: &str, payload: &serde_json::Value, from: &str) {
        tracing::info!("Runtime received '{}' from {}", kind, from);
        match kind {
            "set_crew" => {
                let count = payload_u8(payload, "count", 1);
                self.handle_crew_size(count);
            }
            "relieve_manager" => {
                let reason = payload_str(payload, "reason");
                self.handle_relieve_manager(&reason).await;
            }
            "task_complete" => self.record_task_event(from, payload).await,
            "task_blocked" => self.record_task_event(from, payload).await,
            _ => tracing::debug!("Runtime ignoring unknown kind: {}", kind),
        }
    }

    async fn record_task_event(&self, from: &str, payload: &serde_json::Value) {
        let content = payload_str(payload, "content");
        let title = first_line(&content);
        if let Err(e) = self.db.create_task(title, Some(&content), 0, from).await {
            tracing::error!("Failed to record task event: {}", e);
        }
    }

    fn shutdown(&mut self) {
        tracing::info!("Shutting down {} agents", self.agent_handles.len());
        for (name, handle) in self.agent_handles.drain() {
            tracing::info!("Stopping {}", name);
            handle.abort();
        }
    }

    fn spawn_initial_agents(&mut self, manager_task: Option<String>) -> Result<()> {
        self.spawn_agent(AgentRole::Manager, 0, manager_task)?;
        self.spawn_agent(AgentRole::Architect, 0, None)?;
        self.spawn_agent(AgentRole::Scorer, 0, None)?;
        self.spawn_agent(AgentRole::Developer, 0, None)?;
        Ok(())
    }

    fn spawn_agent(
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
        let config = AgentConfig {
            agent_id,
            working_dir: self.working_dir.clone(),
            system_prompt: role.system_prompt().to_string(),
            initial_task,
            mcp_config: Some(build_mcp_config(&bus_name)),
        };
        self.spawn_agent_with_config(config)
    }

    fn spawn_agent_with_config(&mut self, config: AgentConfig) -> Result<()> {
        let bus_name = config.agent_id.bus_name();
        let session = self
            .session_store
            .session(&bus_name)
            .system_prompt(&config.system_prompt);
        let mailbox = self
            .bus
            .register(&bus_name)
            .map_err(|e| anyhow::anyhow!("Failed to register {}: {}", bus_name, e))?;

        let agent_id = config.agent_id.clone();
        let handle = tokio::spawn(async move {
            match Agent::new(config, mailbox, session) {
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

    fn handle_crew_size(&mut self, count: u8) {
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

    async fn handle_relieve_manager(&mut self, reason: &str) {
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
        let config = AgentConfig {
            agent_id: AgentId::new_singleton(AgentRole::Manager),
            working_dir: self.working_dir.clone(),
            system_prompt: prompt,
            initial_task: None,
            mcp_config: Some(build_mcp_config("manager")),
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

fn build_mcp_config(agent_name: &str) -> String {
    let socket_path = relay::relay_socket_path();
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

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}
