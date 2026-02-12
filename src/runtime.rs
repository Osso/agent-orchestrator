//! Orchestrator runtime that manages agent lifecycle and state
//!
//! The runtime:
//! - Spawns and tracks agent processes
//! - Handles runtime commands (CREW sizing, RELIEVE manager)
//! - Maintains task log for manager briefings

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const RELIEVE_COOLDOWN: Duration = Duration::from_secs(60);

use crate::agent::{Agent, AgentConfig};
use crate::backend::AgentBackend;
use crate::types::{AgentId, AgentRole};

/// Commands sent from agents to the runtime (not over the wire)
#[derive(Debug)]
pub enum RuntimeCommand {
    /// Manager requests N developers (1-3)
    SetCrewSize { count: u8 },
    /// Scorer fires the manager
    RelieveManager { reason: String },
    /// Agent reports task status change
    TaskUpdate {
        agent: AgentId,
        status: TaskStatus,
        summary: String,
    },
}

/// Status of a task tracked by the runtime
#[derive(Debug, Clone)]
pub enum TaskStatus {
    InProgress,
    Completed,
    Blocked,
}

/// Record of a task for briefing new managers
#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub agent: AgentId,
    pub status: TaskStatus,
    pub summary: String,
}

/// Mutable state tracked by the runtime
struct RuntimeState {
    developer_count: u8,
    task_log: Vec<TaskRecord>,
    manager_generation: u32,
    last_relieve: Option<Instant>,
}

/// Core orchestrator that spawns agents and handles runtime commands
pub struct OrchestratorRuntime {
    state: RuntimeState,
    backend: Arc<dyn AgentBackend>,
    base_path: PathBuf,
    working_dir: String,
    command_tx: mpsc::Sender<RuntimeCommand>,
    command_rx: mpsc::Receiver<RuntimeCommand>,
    agent_handles: HashMap<AgentId, JoinHandle<()>>,
}

impl OrchestratorRuntime {
    pub fn new(
        backend: Arc<dyn AgentBackend>,
        base_path: PathBuf,
        working_dir: String,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel(64);

        Self {
            state: RuntimeState {
                developer_count: 1,
                task_log: Vec::new(),
                manager_generation: 0,
                last_relieve: None,
            },
            backend,
            base_path,
            working_dir,
            command_tx,
            command_rx,
            agent_handles: HashMap::new(),
        }
    }

    /// Run the orchestrator: spawn initial agents, then process commands
    pub async fn run(mut self) -> Result<()> {
        self.spawn_initial_agents().await?;

        while let Some(cmd) = self.command_rx.recv().await {
            tracing::info!("Runtime command: {:?}", cmd);
            match cmd {
                RuntimeCommand::SetCrewSize { count } => {
                    self.handle_crew_size(count).await;
                }
                RuntimeCommand::RelieveManager { reason } => {
                    self.handle_relieve_manager(&reason).await;
                }
                RuntimeCommand::TaskUpdate {
                    agent,
                    status,
                    summary,
                } => {
                    self.state.task_log.push(TaskRecord {
                        agent,
                        status,
                        summary,
                    });
                }
            }
        }

        Ok(())
    }

    /// Spawn the four initial agents: manager, architect, scorer, developer-0
    async fn spawn_initial_agents(&mut self) -> Result<()> {
        for role in [AgentRole::Manager, AgentRole::Architect, AgentRole::Scorer] {
            let id = AgentId::new_singleton(role);
            self.spawn_agent(id, role.system_prompt().to_string())
                .await?;
        }

        let dev_id = AgentId::new_developer(0);
        self.spawn_agent(dev_id, AgentRole::Developer.system_prompt().to_string())
            .await?;

        Ok(())
    }

    /// Spawn a single agent and track its handle
    async fn spawn_agent(&mut self, agent_id: AgentId, system_prompt: String) -> Result<()> {
        let backend = self.backend.clone();
        let base_path = self.base_path.clone();
        let working_dir = self.working_dir.clone();
        let command_tx = self.command_tx.clone();
        let id_for_log = agent_id.clone();

        let handle = tokio::spawn(async move {
            let config = AgentConfig {
                agent_id: id_for_log.clone(),
                working_dir,
                system_prompt,
            };

            match Agent::new(config, backend, &base_path, command_tx).await {
                Ok(agent) => {
                    if let Err(e) = agent.run().await {
                        tracing::error!("Agent {} error: {}", id_for_log, e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create agent {}: {}", id_for_log, e);
                }
            }
        });

        self.agent_handles.insert(agent_id, handle);
        Ok(())
    }

    /// Adjust developer count: spawn or kill developers to match target
    async fn handle_crew_size(&mut self, count: u8) {
        let count = count.clamp(1, 3);
        let current = self.state.developer_count;

        if count == current {
            return;
        }

        tracing::info!("CREW resize: {} -> {}", current, count);

        if count > current {
            self.spawn_developers(current, count).await;
        } else {
            self.kill_developers(count, current);
        }

        self.state.developer_count = count;
    }

    /// Spawn developers from index `from` to `to` (exclusive)
    async fn spawn_developers(&mut self, from: u8, to: u8) {
        for i in from..to {
            let dev_id = AgentId::new_developer(i);
            let prompt = AgentRole::Developer.system_prompt().to_string();
            if let Err(e) = self.spawn_agent(dev_id.clone(), prompt).await {
                tracing::error!("Failed to spawn {}: {}", dev_id, e);
            }
        }
    }

    /// Abort developers from index `from` to `to` (exclusive) and clean up
    fn kill_developers(&mut self, from: u8, to: u8) {
        for i in from..to {
            let dev_id = AgentId::new_developer(i);
            if let Some(handle) = self.agent_handles.remove(&dev_id) {
                tracing::info!("Stopping {}", dev_id);
                handle.abort();
            }
        }
    }

    /// Replace the current manager with a fresh instance briefed on state
    async fn handle_relieve_manager(&mut self, reason: &str) {
        if let Some(last) = self.state.last_relieve
            && last.elapsed() < RELIEVE_COOLDOWN
        {
            tracing::warn!(
                "RELIEVE rejected: cooldown ({:.0}s remaining)",
                (RELIEVE_COOLDOWN - last.elapsed()).as_secs_f64()
            );
            return;
        }

        tracing::warn!(
            "RELIEVE: firing manager gen {} — {}",
            self.state.manager_generation,
            reason
        );

        self.abort_manager();
        self.state.manager_generation += 1;
        self.state.last_relieve = Some(Instant::now());

        let briefing = self.build_manager_briefing(reason);
        let prompt = format!("{}\n\n{}", AgentRole::Manager.system_prompt(), briefing);

        if let Err(e) = self.spawn_agent(AgentId::new_singleton(AgentRole::Manager), prompt).await
        {
            tracing::error!("Failed to spawn replacement manager: {}", e);
        }
    }

    /// Abort the current manager process
    fn abort_manager(&mut self) {
        let mgr_id = AgentId::new_singleton(AgentRole::Manager);
        if let Some(handle) = self.agent_handles.remove(&mgr_id) {
            handle.abort();
        }
    }

    /// Build a state briefing for a replacement manager from the task log
    fn build_manager_briefing(&self, relieve_reason: &str) -> String {
        let mut briefing = String::from("## State Briefing (you are replacing the previous manager)\n\n");
        briefing.push_str(&format!(
            "**Reason for replacement:** {}\n\n",
            relieve_reason
        ));
        briefing.push_str(&format!(
            "**Manager generation:** {}\n",
            self.state.manager_generation
        ));
        briefing.push_str(&format!(
            "**Active developers:** {}\n\n",
            self.state.developer_count
        ));

        if self.state.task_log.is_empty() {
            briefing.push_str("No task history recorded.\n");
        } else {
            briefing.push_str("### Task History\n");
            for record in &self.state.task_log {
                briefing.push_str(&format!(
                    "- [{}] {:?}: {}\n",
                    record.agent, record.status, record.summary
                ));
            }
        }

        briefing
    }
}
