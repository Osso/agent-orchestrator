//! Agent runtime that combines llm-sdk backend and agent-bus transport
//!
//! Each agent:
//! 1. Registers on the in-process message bus
//! 2. Waits for messages from other agents
//! 3. Calls llm-sdk to get a completion
//! 4. Parses output for structured messages (TASK:, APPROVED:, etc.)
//! 5. Routes messages to other agents via the bus

use agent_bus::Mailbox;
use anyhow::Result;
use llm_sdk::claude::Claude;
use llm_sdk::session::Session;

use crate::types::{AgentId, AgentRole};

/// Configuration for an agent
pub struct AgentConfig {
    pub agent_id: AgentId,
    pub working_dir: String,
    pub system_prompt: String,
    /// Task to process immediately after connecting (before accepting bus messages)
    pub initial_task: Option<String>,
}

/// Result of parsing an agent output section
pub enum ParsedOutput {
    /// Route a message to another agent via the bus
    Message {
        to: String,
        kind: String,
        content: String,
    },
    /// Send a command to the runtime via the bus
    Command {
        kind: String,
        payload: serde_json::Value,
    },
}

/// Running agent instance
pub struct Agent {
    config: AgentConfig,
    mailbox: Mailbox,
    session: Session,
    base_claude: Claude,
}

impl Agent {
    pub fn new(config: AgentConfig, mailbox: Mailbox, session: Session) -> Result<Self> {
        let perm_mode = permission_mode_for_role(config.agent_id.role);
        let base_claude = Claude::new()?
            .permission_mode(perm_mode)
            .working_dir(&config.working_dir);
        Ok(Self {
            config,
            mailbox,
            session,
            base_claude,
        })
    }

    /// Run the agent main loop
    pub async fn run(mut self) -> Result<()> {
        tracing::info!("Agent {} started", self.config.agent_id);

        if let Some(task) = self.config.initial_task.take() {
            tracing::info!("Agent {} processing initial task", self.config.agent_id);
            self.process_prompt(&task).await?;
        }

        while let Some(msg) = self.mailbox.recv().await {
            tracing::info!(
                "Agent {} received '{}' from {}",
                self.config.agent_id,
                msg.kind,
                msg.from
            );
            let content = extract_content(&msg.payload);

            if let Err(e) = self.process_prompt(&content).await {
                tracing::error!("Agent {} completion failed: {}", self.config.agent_id, e);
            }
        }

        tracing::info!("Agent {} stopped", self.config.agent_id);
        Ok(())
    }

    async fn process_prompt(&mut self, content: &str) -> Result<()> {
        let output = self.session.complete(&self.base_claude, content).await?;
        log_completion(&self.config.agent_id, &output);
        self.dispatch_output(&output.text);
        Ok(())
    }

    fn dispatch_output(&self, text: &str) {
        for parsed in parse_agent_output(&self.config.agent_id, text) {
            match parsed {
                ParsedOutput::Message { to, kind, content } => {
                    tracing::info!("{} -> {} ({})", self.config.agent_id, to, kind);
                    let payload = serde_json::json!({ "content": content });
                    if let Err(e) = self.mailbox.send(&to, &kind, payload) {
                        tracing::error!("Failed to send to {}: {}", to, e);
                    }
                }
                ParsedOutput::Command { kind, payload } => {
                    tracing::info!("{} -> runtime ({})", self.config.agent_id, kind);
                    if let Err(e) = self.mailbox.send("runtime", &kind, payload) {
                        tracing::error!("Failed to send command to runtime: {}", e);
                    }
                }
            }
        }
    }
}

fn log_completion(agent_id: &AgentId, output: &llm_sdk::Output) {
    if let Some(ref usage) = output.usage {
        tracing::debug!(
            "Agent {} tokens: in={} out={}",
            agent_id,
            usage.input_tokens,
            usage.output_tokens
        );
    }
    if !output.text.is_empty() {
        tracing::info!("[{}] {}", agent_id, first_line(&output.text));
    }
}

fn extract_content(payload: &serde_json::Value) -> String {
    payload
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string()
}

// --- Multi-line section-based output parsing ---

const ALL_PREFIXES: &[&str] = &[
    "CREW:",
    "RELIEVE:",
    "TASK:",
    "APPROVED:",
    "REJECTED:",
    "COMPLETE:",
    "BLOCKED:",
    "INTERRUPT:",
    "EVALUATION:",
    "OBSERVATION:",
];

const SINGLE_LINE_PREFIXES: &[&str] = &["CREW:", "RELIEVE:"];

/// Message routing table: (prefix, target_role, kind, require_from_role)
const ROUTES: &[(&str, AgentRole, &str, Option<AgentRole>)] = &[
    ("TASK:", AgentRole::Architect, "task_assignment", None),
    ("REJECTED:", AgentRole::Manager, "architect_review", None),
    (
        "INTERRUPT:",
        AgentRole::Developer,
        "interrupt",
        Some(AgentRole::Architect),
    ),
];

fn strip_markdown_bold(line: &str) -> &str {
    line.strip_prefix("**").unwrap_or(line)
}

fn recognized_prefix(line: &str) -> Option<&'static str> {
    let clean = strip_markdown_bold(line);
    ALL_PREFIXES.iter().find(|&&p| clean.starts_with(p)).copied()
}

fn extract_sections(text: &str) -> Vec<(&'static str, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut sections = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim_start();
        if let Some(prefix) = recognized_prefix(line) {
            let (content, next_i) = collect_section(&lines, i, prefix);
            sections.push((prefix, content));
            i = next_i;
        } else {
            i += 1;
        }
    }

    sections
}

fn collect_section(lines: &[&str], start: usize, prefix: &str) -> (String, usize) {
    let clean = strip_markdown_bold(lines[start].trim_start());
    let first = clean[prefix.len()..].trim().trim_end_matches("**");

    if SINGLE_LINE_PREFIXES.contains(&prefix) {
        return (first.to_string(), start + 1);
    }

    let mut content = first.to_string();
    let mut i = start + 1;
    while i < lines.len() && recognized_prefix(lines[i].trim_start()).is_none() {
        content.push('\n');
        content.push_str(lines[i]);
        i += 1;
    }

    (content.trim_end().to_string(), i)
}

pub fn parse_agent_output(from: &AgentId, text: &str) -> Vec<ParsedOutput> {
    extract_sections(text)
        .into_iter()
        .filter_map(|(prefix, content)| parse_section(from, prefix, &content))
        .collect()
}

fn parse_section(from: &AgentId, prefix: &str, content: &str) -> Option<ParsedOutput> {
    match prefix {
        "CREW:" => parse_crew_command(from, content),
        "RELIEVE:" => parse_relieve_command(from, content),
        "APPROVED:" => parse_approved_section(content),
        "COMPLETE:" => parse_completion_message(from, "task_complete", content),
        "BLOCKED:" => parse_completion_message(from, "task_blocked", content),
        "EVALUATION:" | "OBSERVATION:" => {
            log_scorer_output(from, prefix, content);
            None
        }
        _ => parse_routed_section(from, prefix, content),
    }
}

fn parse_crew_command(from: &AgentId, content: &str) -> Option<ParsedOutput> {
    if from.role != AgentRole::Manager {
        return None;
    }
    let count: u8 = content.trim().parse().ok()?;
    Some(ParsedOutput::Command {
        kind: "set_crew".to_string(),
        payload: serde_json::json!({ "count": count }),
    })
}

fn parse_relieve_command(from: &AgentId, content: &str) -> Option<ParsedOutput> {
    if from.role != AgentRole::Scorer {
        return None;
    }
    Some(ParsedOutput::Command {
        kind: "relieve_manager".to_string(),
        payload: serde_json::json!({ "reason": content }),
    })
}

fn parse_approved_section(content: &str) -> Option<ParsedOutput> {
    let target = parse_developer_target(content);
    Some(ParsedOutput::Message {
        to: target,
        kind: "task_assignment".to_string(),
        content: content.to_string(),
    })
}

fn parse_completion_message(from: &AgentId, kind: &str, content: &str) -> Option<ParsedOutput> {
    tracing::info!("[{}] from {}: {}", kind.to_uppercase(), from, first_line(content));
    Some(ParsedOutput::Message {
        to: AgentId::new_singleton(AgentRole::Manager).bus_name(),
        kind: kind.to_string(),
        content: content.to_string(),
    })
}

fn log_scorer_output(from: &AgentId, prefix: &str, content: &str) {
    if from.role == AgentRole::Scorer {
        tracing::info!(
            "[SCORER {}] {}",
            prefix.trim_end_matches(':'),
            first_line(content)
        );
    }
}

fn parse_routed_section(from: &AgentId, prefix: &str, content: &str) -> Option<ParsedOutput> {
    for &(route_prefix, target_role, kind, require_from) in ROUTES {
        if prefix != route_prefix {
            continue;
        }
        if let Some(required) = require_from
            && from.role != required
        {
            continue;
        }
        return Some(ParsedOutput::Message {
            to: AgentId::new_singleton(target_role).bus_name(),
            kind: kind.to_string(),
            content: content.to_string(),
        });
    }
    None
}

fn parse_developer_target(text: &str) -> String {
    if let Some(rest) = text.strip_prefix("developer-")
        && let Some(digit) = rest.chars().next()
        && digit.is_ascii_digit()
    {
        return format!("developer-{}", digit);
    }
    "developer-0".to_string()
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn permission_mode_for_role(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Developer => "acceptEdits",
        AgentRole::Manager | AgentRole::Architect | AgentRole::Scorer => "dontAsk",
    }
}
