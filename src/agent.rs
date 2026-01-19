//! Agent runtime that combines backend and transport
//!
//! Each agent:
//! 1. Listens on a Unix socket for inter-agent messages
//! 2. Spawns an AI backend (Claude, Gemini, etc.) to process prompts
//! 3. Parses output for structured messages (TASK:, APPROVED:, etc.)
//! 4. Routes messages to other agents via their sockets

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::backend::{AgentBackend, AgentOutput};
use crate::transport::{AgentConnection, AgentListener, AgentMessage, MessageKind};
use crate::types::AgentRole;

/// Configuration for an agent
pub struct AgentConfig {
    pub role: AgentRole,
    pub working_dir: String,
    pub system_prompt: String,
}

/// Running agent instance
pub struct Agent {
    config: AgentConfig,
    backend: Arc<dyn AgentBackend>,
    listener: AgentListener,
    base_path: std::path::PathBuf,
}

impl Agent {
    /// Create a new agent
    pub async fn new(
        config: AgentConfig,
        backend: Arc<dyn AgentBackend>,
        base_path: &Path,
    ) -> Result<Self> {
        let listener = AgentListener::bind(config.role, base_path).await?;

        Ok(Self {
            config,
            backend,
            listener,
            base_path: base_path.to_path_buf(),
        })
    }

    /// Run the agent main loop
    pub async fn run(self) -> Result<()> {
        tracing::info!("Agent {} starting", self.config.role);

        // Channel for outgoing messages to other agents
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<AgentMessage>(64);

        // Spawn task to handle outgoing messages
        let base_path = self.base_path.clone();
        let role = self.config.role;
        tokio::spawn(async move {
            while let Some(msg) = outgoing_rx.recv().await {
                if let Err(e) = send_to_agent(&msg, &base_path).await {
                    tracing::error!("Failed to send message to {}: {}", msg.to, e);
                }
            }
            tracing::debug!("Outgoing message handler for {} stopped", role);
        });

        loop {
            // Accept incoming connection
            let (mut conn, creds) = self.listener.accept().await?;
            tracing::debug!(
                "Agent {} got connection from pid={}",
                self.config.role,
                creds.pid
            );

            // Receive message
            let msg = match conn.recv().await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("Failed to receive message: {}", e);
                    continue;
                }
            };

            tracing::info!(
                "Agent {} received {:?} from {}",
                self.config.role,
                msg.kind,
                msg.from
            );

            // Process the message through the AI backend
            let prompt = format_prompt_for_agent(&msg, &self.config);

            let (mut handle, mut output_rx) = self
                .backend
                .spawn(&prompt, &self.config.working_dir, None)
                .await
                .context("Failed to spawn backend")?;

            // Process output stream
            let outgoing_tx = outgoing_tx.clone();
            let from_role = self.config.role;

            while let Some(output) = output_rx.recv().await {
                // Log the output
                if let Some(text) = output.text() {
                    tracing::debug!("[{}] {}", from_role, text);

                    // Parse for structured output
                    if let Some(outgoing) = parse_agent_output(from_role, text) {
                        if outgoing_tx.send(outgoing).await.is_err() {
                            break;
                        }
                    }
                }

                if output.is_final() {
                    break;
                }
            }

            // Ensure process completes
            let _ = handle.wait().await;
        }
    }
}

/// Format incoming message as a prompt for the AI
fn format_prompt_for_agent(msg: &AgentMessage, config: &AgentConfig) -> String {
    let context = match msg.kind {
        MessageKind::TaskAssignment => "NEW TASK",
        MessageKind::TaskComplete => "TASK COMPLETE",
        MessageKind::TaskGiveUp => "TASK BLOCKED",
        MessageKind::Interrupt => "INTERRUPT",
        MessageKind::ArchitectReview => "ARCHITECT REVIEW",
        MessageKind::Info => "INFO",
        MessageKind::Evaluation => "EVALUATION",
        MessageKind::Observation => "OBSERVATION",
    };

    format!(
        "{}\n\n{} from {}: {}\n\nTask ID: {:?}",
        config.system_prompt, context, msg.from, msg.content, msg.task_id
    )
}

/// Parse agent output for structured messages
fn parse_agent_output(from: AgentRole, text: &str) -> Option<AgentMessage> {
    let text = text.trim();

    // Manager -> Architect: TASK:
    if text.starts_with("TASK:") {
        let content = text.strip_prefix("TASK:")?.trim().to_string();
        return Some(AgentMessage::new(
            from,
            AgentRole::Architect,
            MessageKind::TaskAssignment,
            content,
        ));
    }

    // Architect -> Developer: APPROVED:
    if text.starts_with("APPROVED:") {
        let content = text.strip_prefix("APPROVED:")?.trim().to_string();
        return Some(AgentMessage::new(
            from,
            AgentRole::Developer,
            MessageKind::TaskAssignment,
            content,
        ));
    }

    // Architect -> Manager: REJECTED:
    if text.starts_with("REJECTED:") {
        let content = text.strip_prefix("REJECTED:")?.trim().to_string();
        return Some(AgentMessage::new(
            from,
            AgentRole::Manager,
            MessageKind::ArchitectReview,
            content,
        ));
    }

    // Developer -> Manager: COMPLETE:
    if text.starts_with("COMPLETE:") {
        let content = text.strip_prefix("COMPLETE:")?.trim().to_string();
        return Some(AgentMessage::new(
            from,
            AgentRole::Manager,
            MessageKind::TaskComplete,
            content,
        ));
    }

    // Developer -> Manager: BLOCKED:
    if text.starts_with("BLOCKED:") {
        let content = text.strip_prefix("BLOCKED:")?.trim().to_string();
        return Some(AgentMessage::new(
            from,
            AgentRole::Manager,
            MessageKind::TaskGiveUp,
            content,
        ));
    }

    // Architect -> Developer: INTERRUPT:
    if text.starts_with("INTERRUPT:") && from == AgentRole::Architect {
        let content = text.strip_prefix("INTERRUPT:")?.trim().to_string();
        return Some(AgentMessage::new(
            from,
            AgentRole::Developer,
            MessageKind::Interrupt,
            content,
        ));
    }

    // Scorer outputs (logged but not routed to decision-makers)
    if from == AgentRole::Scorer {
        if text.starts_with("EVALUATION:") {
            let content = text.strip_prefix("EVALUATION:")?.trim().to_string();
            // Log evaluation but don't route (scorer has no decision power)
            tracing::info!("[SCORER EVALUATION] {}", content);
            return None;
        }
        if text.starts_with("OBSERVATION:") {
            let content = text.strip_prefix("OBSERVATION:")?.trim().to_string();
            tracing::info!("[SCORER OBSERVATION] {}", content);
            return None;
        }
    }

    None
}

/// Send a message to another agent via their socket
async fn send_to_agent(msg: &AgentMessage, base_path: &Path) -> Result<()> {
    let mut conn = AgentConnection::connect(msg.to, base_path).await?;
    conn.send(msg).await?;
    tracing::debug!("Sent {:?} message to {}", msg.kind, msg.to);
    Ok(())
}
