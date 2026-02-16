//! Agent backend abstraction
//!
//! Provides a trait that abstracts away the specific AI provider (Claude, Gemini, Codex).
//! Each backend knows how to spawn a process, send messages, and receive streaming output.

mod claude;
mod claudius;

pub use claude::ClaudeBackend;
pub use claudius::ClaudiusBackend;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Output from an AI agent, normalized across providers
#[derive(Debug, Clone)]
pub enum AgentOutput {
    /// Text content from the assistant
    Text(String),

    /// Agent is using a tool
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// Tool execution result
    ToolResult {
        id: String,
        output: String,
        is_error: bool,
    },

    /// Final result - agent has completed
    Result {
        text: Option<String>,
        is_error: bool,
        session_id: Option<String>,
    },

    /// Error from the agent
    Error(String),

    /// System/initialization message
    System { session_id: Option<String> },
}

impl AgentOutput {
    /// Check if this is a final result
    pub fn is_final(&self) -> bool {
        matches!(self, AgentOutput::Result { .. } | AgentOutput::Error(_))
    }

    /// Extract text content if available
    pub fn text(&self) -> Option<&str> {
        match self {
            AgentOutput::Text(t) => Some(t),
            AgentOutput::Result { text, .. } => text.as_deref(),
            _ => None,
        }
    }
}

/// Handle to a running agent process
#[async_trait]
pub trait AgentHandle: Send {
    /// Abort the current operation
    async fn abort(&mut self) -> Result<()>;

    /// Wait for the process to complete
    async fn wait(&mut self) -> Result<()>;
}

/// Backend for spawning and communicating with an AI agent
#[async_trait]
pub trait AgentBackend: Send + Sync {
    /// Name of this backend (e.g., "claude", "gemini", "codex")
    fn name(&self) -> &'static str;

    /// Pre-create a session for an agent. Returns session_id if the backend
    /// supports persistent sessions (e.g. Claudius). Default: no-op.
    /// If `system_prompt` is provided, it is sent as the first message so
    /// the agent has its instructions even when used interactively.
    async fn init_session(
        &self,
        _working_dir: &str,
        _title: Option<&str>,
        _system_prompt: Option<&str>,
    ) -> Result<Option<String>> {
        Ok(None)
    }

    /// Spawn a new agent process with the given prompt
    ///
    /// Returns a handle for control and a receiver for streaming output.
    async fn spawn(
        &self,
        prompt: &str,
        working_dir: &str,
        session_id: Option<String>,
        title: Option<&str>,
    ) -> Result<(Box<dyn AgentHandle>, mpsc::Receiver<AgentOutput>)>;
}
