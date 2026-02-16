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
use crate::runtime::{RuntimeCommand, TaskStatus};
use crate::transport::{AgentConnection, AgentListener, AgentMessage, MessageKind};
use crate::types::{AgentId, AgentRole};

/// Configuration for an agent
pub struct AgentConfig {
    pub agent_id: AgentId,
    pub working_dir: String,
    pub system_prompt: String,
    /// Task to process immediately after session init (before accepting socket messages)
    pub initial_task: Option<String>,
}

/// Result of parsing an agent output section
pub enum ParsedOutput {
    /// Route a message to another agent
    Message(AgentMessage),
    /// Send a command to the runtime
    Command(RuntimeCommand),
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

    /// Initialize session and build pending task from config
    async fn init(&mut self) -> (Option<String>, bool, Option<AgentMessage>) {
        let title = format!("Orchestrator: {}", self.config.agent_id);
        let perm_mode = permission_mode_for_role(self.config.agent_id.role);
        let session_id = self
            .backend
            .init_session(
                &self.config.working_dir,
                Some(&title),
                Some(&self.config.system_prompt),
                Some(perm_mode),
            )
            .await
            .unwrap_or(None);
        let system_prompt_sent = session_id.is_some();
        if system_prompt_sent {
            tracing::info!("Agent {} session pre-created with system prompt", self.config.agent_id);
        }

        // Queue initial task to process before accepting socket messages.
        // This avoids the race where socket injection arrives before init_session completes.
        let pending = self.config.initial_task.take().map(|task| {
            AgentMessage::new(
                self.config.agent_id.clone(),
                self.config.agent_id.clone(),
                MessageKind::Info,
                task,
            )
        });

        (session_id, system_prompt_sent, pending)
    }

    /// Run the agent main loop
    pub async fn run(mut self) -> Result<()> {
        tracing::info!("Agent {} starting", self.config.agent_id);

        let outgoing_tx = spawn_outgoing_handler(
            self.config.agent_id.clone(),
            self.base_path.clone(),
        );

        let title = format!("Orchestrator: {}", self.config.agent_id);
        let perm_mode = permission_mode_for_role(self.config.agent_id.role);
        let (mut session_id, mut system_prompt_sent, mut pending_task) = self.init().await;

        loop {
            let msg = match pending_task.take() {
                Some(m) => {
                    tracing::info!("Agent {} processing initial task", self.config.agent_id);
                    m
                }
                None => self.accept_message().await?,
            };

            let prompt = if system_prompt_sent {
                format_task_content(&msg)
            } else {
                system_prompt_sent = true;
                format_prompt_for_agent(&msg, &self.config)
            };
            let (mut handle, mut output_rx) = self
                .backend
                .spawn(&prompt, &self.config.working_dir, session_id.clone(), Some(&title), Some(perm_mode))
                .await
                .context("Failed to spawn backend")?;

            session_id = process_output(
                &mut output_rx,
                session_id,
                &self.config.agent_id,
                &outgoing_tx,
                &self.command_tx,
            )
            .await;

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

/// Spawn a task that forwards outgoing messages to their target agents
fn spawn_outgoing_handler(
    agent_id: AgentId,
    base_path: std::path::PathBuf,
) -> mpsc::Sender<AgentMessage> {
    let (tx, mut rx) = mpsc::channel::<AgentMessage>(64);
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = send_to_agent(&msg, &base_path).await {
                tracing::error!("Failed to send message to {}: {}", msg.to, e);
            }
        }
        tracing::debug!("Outgoing message handler for {} stopped", agent_id);
    });
    tx
}

/// Process output stream: log text, route parsed messages, capture session_id
async fn process_output(
    output_rx: &mut mpsc::Receiver<AgentOutput>,
    mut session_id: Option<String>,
    from_id: &AgentId,
    outgoing_tx: &mpsc::Sender<AgentMessage>,
    cmd_tx: &mpsc::Sender<RuntimeCommand>,
) -> Option<String> {
    while let Some(output) = output_rx.recv().await {
        match &output {
            AgentOutput::System { session_id: sid @ Some(_) }
            | AgentOutput::Result { session_id: sid @ Some(_), .. } => {
                session_id = sid.clone();
            }
            _ => {}
        }
        if let Some(text) = output.text() {
            if !text.is_empty() {
                tracing::info!("[{}] {}", from_id, text);
            }
        }
        if let AgentOutput::Text(ref text) = output {
            dispatch_parsed(from_id, text, outgoing_tx, cmd_tx).await;
        }
        if output.is_final() {
            break;
        }
    }
    session_id
}

/// Dispatch all parsed outputs from a text block to the appropriate channels
async fn dispatch_parsed(
    from: &AgentId,
    text: &str,
    outgoing_tx: &mpsc::Sender<AgentMessage>,
    cmd_tx: &mpsc::Sender<RuntimeCommand>,
) {
    for parsed in parse_agent_output(from, text) {
        match parsed {
            ParsedOutput::Message(msg) => {
                tracing::info!("{} -> {} ({:?})", from, msg.to, msg.kind);
                let _ = outgoing_tx.send(msg).await;
            }
            ParsedOutput::Command(cmd) => {
                tracing::info!("{} -> runtime ({:?})", from, cmd);
                let _ = cmd_tx.send(cmd).await;
            }
        }
    }
}

/// Format incoming message as a prompt for the AI (includes system prompt)
fn format_prompt_for_agent(msg: &AgentMessage, config: &AgentConfig) -> String {
    format!("{}\n\n{}", config.system_prompt, format_task_content(msg))
}

/// Format just the task content (without system prompt, for subsequent messages)
fn format_task_content(msg: &AgentMessage) -> String {
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
        "{} from {}: {}\n\nTask ID: {:?}",
        context, msg.from, msg.content, msg.task_id
    )
}

// --- Multi-line section-based output parsing ---

/// All recognized output prefixes
const ALL_PREFIXES: &[&str] = &[
    "CREW:", "RELIEVE:", "TASK:", "APPROVED:", "REJECTED:",
    "COMPLETE:", "BLOCKED:", "INTERRUPT:", "EVALUATION:", "OBSERVATION:",
];

/// Prefixes whose content is only the remainder of the same line
const SINGLE_LINE_PREFIXES: &[&str] = &["CREW:", "RELIEVE:"];

/// Message routing table: (prefix, target_role, kind, require_from_role)
const ROUTES: &[(&str, AgentRole, MessageKind, Option<AgentRole>)] = &[
    ("TASK:", AgentRole::Architect, MessageKind::TaskAssignment, None),
    ("REJECTED:", AgentRole::Manager, MessageKind::ArchitectReview, None),
    ("INTERRUPT:", AgentRole::Developer, MessageKind::Interrupt, Some(AgentRole::Architect)),
];

/// Check if a line starts with a recognized prefix
fn recognized_prefix(line: &str) -> Option<&'static str> {
    ALL_PREFIXES.iter().find(|&&p| line.starts_with(p)).copied()
}

/// Extract structured sections from multi-line agent output.
/// Returns (prefix, content) pairs where content includes continuation lines.
fn extract_sections(text: &str) -> Vec<(&'static str, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut sections = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim_start();
        if let Some(prefix) = recognized_prefix(line) {
            let first = line[prefix.len()..].trim();

            if SINGLE_LINE_PREFIXES.contains(&prefix) {
                sections.push((prefix, first.to_string()));
                i += 1;
                continue;
            }

            // Multi-line: collect until next prefix or end
            let mut content = first.to_string();
            i += 1;
            while i < lines.len() {
                if recognized_prefix(lines[i].trim_start()).is_some() {
                    break;
                }
                content.push('\n');
                content.push_str(lines[i]);
                i += 1;
            }

            sections.push((prefix, content.trim_end().to_string()));
        } else {
            i += 1;
        }
    }

    sections
}

/// Parse multi-line agent output into messages and runtime commands
pub fn parse_agent_output(from: &AgentId, text: &str) -> Vec<ParsedOutput> {
    extract_sections(text)
        .into_iter()
        .filter_map(|(prefix, content)| parse_section(from, prefix, &content))
        .collect()
}

/// Parse a single extracted section into a ParsedOutput
fn parse_section(from: &AgentId, prefix: &str, content: &str) -> Option<ParsedOutput> {
    match prefix {
        "CREW:" => {
            if from.role != AgentRole::Manager { return None; }
            let count: u8 = content.trim().parse().ok()?;
            Some(ParsedOutput::Command(RuntimeCommand::SetCrewSize { count }))
        }
        "RELIEVE:" => {
            if from.role != AgentRole::Scorer { return None; }
            Some(ParsedOutput::Command(RuntimeCommand::RelieveManager {
                reason: content.to_string(),
            }))
        }
        "APPROVED:" => {
            let target = parse_developer_target(content);
            Some(ParsedOutput::Message(AgentMessage::new(
                from.clone(), target, MessageKind::TaskAssignment, content.to_string(),
            )))
        }
        "COMPLETE:" | "BLOCKED:" => parse_completion_section(from, prefix, content),
        "EVALUATION:" | "OBSERVATION:" => {
            if from.role == AgentRole::Scorer {
                tracing::info!("[SCORER {}] {}", prefix.trim_end_matches(':'), first_line(content));
            }
            None
        }
        _ => parse_routed_section(from, prefix, content),
    }
}

/// Parse COMPLETE:/BLOCKED: into a message back to the manager
fn parse_completion_section(from: &AgentId, prefix: &str, content: &str) -> Option<ParsedOutput> {
    let (status, kind) = if prefix == "COMPLETE:" {
        (TaskStatus::Completed, MessageKind::TaskComplete)
    } else {
        (TaskStatus::Blocked, MessageKind::TaskGiveUp)
    };
    tracing::info!("[TASK {:?}] from {}: {}", status, from, first_line(content));
    Some(ParsedOutput::Message(AgentMessage::new(
        from.clone(),
        AgentId::new_singleton(AgentRole::Manager),
        kind,
        content.to_string(),
    )))
}

/// Route a section via the routing table (TASK:, REJECTED:, INTERRUPT:)
fn parse_routed_section(from: &AgentId, prefix: &str, content: &str) -> Option<ParsedOutput> {
    for &(route_prefix, target_role, kind, require_from) in ROUTES {
        if prefix != route_prefix { continue; }
        if let Some(required) = require_from
            && from.role != required
        {
            continue;
        }
        return Some(ParsedOutput::Message(AgentMessage::new(
            from.clone(),
            AgentId::new_singleton(target_role),
            kind,
            content.to_string(),
        )));
    }
    None
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

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

/// Map agent role to permission mode.
/// Developers auto-accept edits. Others use dontAsk mode which auto-denies
/// tool uses that aren't pre-approved (read-only tools work without approval).
fn permission_mode_for_role(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Developer => "acceptEdits",
        AgentRole::Manager | AgentRole::Architect | AgentRole::Scorer => "dontAsk",
    }
}

/// Send a message to another agent via their socket
async fn send_to_agent(msg: &AgentMessage, base_path: &Path) -> Result<()> {
    let mut conn = AgentConnection::connect(&msg.to, base_path).await?;
    conn.send(msg).await?;
    tracing::info!("Delivered {:?} to {} from {}", msg.kind, msg.to, msg.from);
    Ok(())
}
