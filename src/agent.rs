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

use crate::backend::AgentBackend;
use crate::runtime::{RuntimeCommand, TaskStatus};
use crate::transport::{AgentConnection, AgentListener, AgentMessage, MessageKind};
use crate::types::{AgentId, AgentRole};

/// Configuration for an agent
pub struct AgentConfig {
    pub agent_id: AgentId,
    pub working_dir: String,
    pub system_prompt: String,
}

/// Result of parsing an agent output line
pub enum ParsedOutput {
    /// Route a message to another agent
    Message(AgentMessage),
    /// Send a command to the runtime
    Command(RuntimeCommand),
    /// No actionable output
    None,
}

/// Running agent instance
pub struct Agent {
    config: AgentConfig,
    backend: Arc<dyn AgentBackend>,
    listener: AgentListener,
    base_path: std::path::PathBuf,
    command_tx: mpsc::Sender<RuntimeCommand>,
}

impl Agent {
    /// Create a new agent
    pub async fn new(
        config: AgentConfig,
        backend: Arc<dyn AgentBackend>,
        base_path: &Path,
        command_tx: mpsc::Sender<RuntimeCommand>,
    ) -> Result<Self> {
        let listener = AgentListener::bind(config.agent_id.clone(), base_path).await?;

        Ok(Self {
            config,
            backend,
            listener,
            base_path: base_path.to_path_buf(),
            command_tx,
        })
    }

    /// Run the agent main loop
    pub async fn run(self) -> Result<()> {
        tracing::info!("Agent {} starting", self.config.agent_id);

        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<AgentMessage>(64);

        let base_path = self.base_path.clone();
        let agent_id_for_log = self.config.agent_id.clone();
        tokio::spawn(async move {
            while let Some(msg) = outgoing_rx.recv().await {
                if let Err(e) = send_to_agent(&msg, &base_path).await {
                    tracing::error!("Failed to send message to {}: {}", msg.to, e);
                }
            }
            tracing::debug!("Outgoing message handler for {} stopped", agent_id_for_log);
        });

        loop {
            let (msg, outgoing_tx) = (
                self.accept_message().await?,
                outgoing_tx.clone(),
            );

            let prompt = format_prompt_for_agent(&msg, &self.config);
            let (mut handle, mut output_rx) = self
                .backend
                .spawn(&prompt, &self.config.working_dir, None)
                .await
                .context("Failed to spawn backend")?;

            let from_id = self.config.agent_id.clone();
            let cmd_tx = self.command_tx.clone();
            while let Some(output) = output_rx.recv().await {
                if let Some(text) = output.text() {
                    tracing::debug!("[{}] {}", from_id, text);
                    match parse_agent_output(&from_id, text) {
                        ParsedOutput::Message(msg) => {
                            if outgoing_tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                        ParsedOutput::Command(cmd) => {
                            let _ = cmd_tx.send(cmd).await;
                        }
                        ParsedOutput::None => {}
                    }
                }
                if output.is_final() {
                    break;
                }
            }

            let _ = handle.wait().await;
        }
    }

    /// Accept and validate an incoming message
    async fn accept_message(&self) -> Result<AgentMessage> {
        loop {
            let (mut conn, creds) = self.listener.accept().await?;
            tracing::debug!("Agent {} got connection from pid={}", self.config.agent_id, creds.pid);

            match conn.recv().await {
                Ok(msg) => {
                    tracing::info!(
                        "Agent {} received {:?} from {}",
                        self.config.agent_id, msg.kind, msg.from
                    );
                    return Ok(msg);
                }
                Err(e) => {
                    tracing::warn!("Failed to receive message: {}", e);
                    continue;
                }
            }
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

/// Message routing table: (prefix, target_role, kind, require_from_role)
const ROUTES: &[(&str, AgentRole, MessageKind, Option<AgentRole>)] = &[
    ("TASK:", AgentRole::Architect, MessageKind::TaskAssignment, None),
    ("REJECTED:", AgentRole::Manager, MessageKind::ArchitectReview, None),
    ("INTERRUPT:", AgentRole::Developer, MessageKind::Interrupt, Some(AgentRole::Architect)),
];

/// Parse agent output line into a message, runtime command, or nothing
pub fn parse_agent_output(from: &AgentId, text: &str) -> ParsedOutput {
    let text = text.trim();

    if let Some(cmd) = parse_runtime_command(from, text) {
        return ParsedOutput::Command(cmd);
    }
    if let Some(msg) = parse_approved_message(from, text) {
        return ParsedOutput::Message(msg);
    }
    if let Some(parsed) = parse_completion_message(from, text) {
        return parsed;
    }
    if let Some(msg) = parse_routed_message(from, text) {
        return ParsedOutput::Message(msg);
    }
    log_scorer_output(from, text);
    ParsedOutput::None
}

/// Parse CREW: and RELIEVE: commands destined for the runtime
fn parse_runtime_command(from: &AgentId, text: &str) -> Option<RuntimeCommand> {
    if let Some(rest) = text.strip_prefix("CREW:") {
        if from.role != AgentRole::Manager {
            return None;
        }
        let count: u8 = rest.trim().parse().ok()?;
        return Some(RuntimeCommand::SetCrewSize { count });
    }
    if let Some(rest) = text.strip_prefix("RELIEVE:") {
        if from.role != AgentRole::Scorer {
            return None;
        }
        let reason = rest.trim().to_string();
        return Some(RuntimeCommand::RelieveManager { reason });
    }
    None
}

/// Parse APPROVED: with optional developer-N target
fn parse_approved_message(from: &AgentId, text: &str) -> Option<AgentMessage> {
    let rest = text.strip_prefix("APPROVED:")?;
    let content = rest.trim();
    let target = parse_developer_target(content);
    Some(AgentMessage::new(
        from.clone(),
        target,
        MessageKind::TaskAssignment,
        content.to_string(),
    ))
}

/// Extract `developer-N` from text prefix, default to developer-0
fn parse_developer_target(text: &str) -> AgentId {
    if let Some(rest) = text.strip_prefix("developer-")
        && let Some(digit) = rest.chars().next()
        && let Some(idx) = digit.to_digit(10)
    {
        return AgentId::new_developer(idx as u8);
    }
    AgentId::new_developer(0)
}

/// Parse COMPLETE:/BLOCKED: — produces both a message and a runtime TaskUpdate
fn parse_completion_message(from: &AgentId, text: &str) -> Option<ParsedOutput> {
    let (content, status, kind) = if let Some(rest) = text.strip_prefix("COMPLETE:") {
        (rest.trim(), TaskStatus::Completed, MessageKind::TaskComplete)
    } else if let Some(rest) = text.strip_prefix("BLOCKED:") {
        (rest.trim(), TaskStatus::Blocked, MessageKind::TaskGiveUp)
    } else {
        return None;
    };

    // The message goes to manager; the TaskUpdate goes to runtime via the caller
    let msg = AgentMessage::new(
        from.clone(),
        AgentId::new_singleton(AgentRole::Manager),
        kind,
        content.to_string(),
    );

    // Log the task update (runtime will pick it up via command channel separately)
    tracing::info!("[TASK {:?}] from {}: {}", status, from, content);
    Some(ParsedOutput::Message(msg))
}

/// Match output against simple routing table
fn parse_routed_message(from: &AgentId, text: &str) -> Option<AgentMessage> {
    for &(prefix, target_role, kind, require_from) in ROUTES {
        if !text.starts_with(prefix) {
            continue;
        }
        if let Some(required) = require_from
            && from.role != required
        {
            continue;
        }
        let content = text[prefix.len()..].trim().to_string();
        return Some(AgentMessage::new(
            from.clone(),
            AgentId::new_singleton(target_role),
            kind,
            content,
        ));
    }
    None
}

/// Log scorer evaluation/observation output (not routed)
fn log_scorer_output(from: &AgentId, text: &str) {
    if from.role != AgentRole::Scorer {
        return;
    }
    if let Some(content) = text.strip_prefix("EVALUATION:") {
        tracing::info!("[SCORER EVALUATION] {}", content.trim());
    } else if let Some(content) = text.strip_prefix("OBSERVATION:") {
        tracing::info!("[SCORER OBSERVATION] {}", content.trim());
    }
}

/// Send a message to another agent via their socket
async fn send_to_agent(msg: &AgentMessage, base_path: &Path) -> Result<()> {
    let mut conn = AgentConnection::connect(&msg.to, base_path).await?;
    conn.send(msg).await?;
    tracing::debug!("Sent {:?} message to {}", msg.kind, msg.to);
    Ok(())
}
