//! Agent runtime that combines llm-sdk backend and agent-bus transport
//!
//! Each agent:
//! 1. Registers on the in-process message bus
//! 2. Waits for messages from other agents
//! 3. Calls llm-sdk to get a completion (with MCP tools for outbound communication)

use agent_bus::Mailbox;
use anyhow::Result;
use async_trait::async_trait;
use llm_sdk::claude::Claude;
use llm_sdk::session::Session;

use crate::types::{AgentId, AgentRole};

/// Abstraction over Session+Claude so tests can inject a fake.
#[async_trait]
pub trait Completer: Send {
    async fn complete(&mut self, prompt: &str) -> Result<llm_sdk::Output, llm_sdk::Error>;
}

/// Production completer backed by llm-sdk Session + Claude CLI.
struct SessionCompleter {
    session: Session,
    claude: Claude,
}

#[async_trait]
impl Completer for SessionCompleter {
    async fn complete(&mut self, prompt: &str) -> Result<llm_sdk::Output, llm_sdk::Error> {
        self.session.complete(&self.claude, prompt).await
    }
}

/// Configuration for an agent
pub struct AgentConfig {
    pub agent_id: AgentId,
    pub working_dir: String,
    pub system_prompt: String,
    /// Task to process immediately after connecting (before accepting bus messages)
    pub initial_task: Option<String>,
    /// MCP config JSON to pass to Claude CLI (--mcp-config)
    pub mcp_config: Option<String>,
}

/// Running agent instance
pub struct Agent {
    config: AgentConfig,
    mailbox: Mailbox,
    completer: Box<dyn Completer>,
}

impl Agent {
    pub fn new(config: AgentConfig, mailbox: Mailbox, session: Session) -> Result<Self> {
        let perm_mode = permission_mode_for_role(config.agent_id.role);
        let mut base_claude = Claude::new()?
            .permission_mode(perm_mode)
            .working_dir(&config.working_dir);
        if let Some(ref cfg) = config.mcp_config {
            base_claude = base_claude.mcp_config(cfg);
        }
        let completer = Box::new(SessionCompleter {
            session,
            claude: base_claude,
        });
        Ok(Self {
            config,
            mailbox,
            completer,
        })
    }

    /// Create an agent with a custom completer (for testing).
    pub fn with_completer(
        config: AgentConfig,
        mailbox: Mailbox,
        completer: Box<dyn Completer>,
    ) -> Self {
        Self {
            config,
            mailbox,
            completer,
        }
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
        let output = self.completer.complete(content).await?;
        log_completion(&self.config.agent_id, &output);
        Ok(())
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

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn permission_mode_for_role(role: AgentRole) -> &'static str {
    match role {
        AgentRole::Developer => "acceptEdits",
        AgentRole::Manager | AgentRole::Architect | AgentRole::Auditor => "dontAsk",
    }
}
