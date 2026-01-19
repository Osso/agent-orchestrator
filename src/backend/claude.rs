//! Claude Code backend implementation
//!
//! Uses Claude Code's stream-json protocol for communication.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use super::{AgentBackend, AgentHandle, AgentOutput};

/// Claude Code backend
pub struct ClaudeBackend {
    /// Additional CLI arguments to pass to claude
    extra_args: Vec<String>,
}

impl ClaudeBackend {
    pub fn new() -> Self {
        Self {
            extra_args: Vec::new(),
        }
    }

    /// Add extra CLI arguments (e.g., for model selection)
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }
}

impl Default for ClaudeBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to a running Claude Code process
pub struct ClaudeHandle {
    child: Child,
}

#[async_trait]
impl AgentHandle for ClaudeHandle {
    async fn abort(&mut self) -> Result<()> {
        self.child.kill().await.context("Failed to kill claude process")
    }

    async fn wait(&mut self) -> Result<()> {
        self.child.wait().await.context("Failed to wait for claude process")?;
        Ok(())
    }
}

#[async_trait]
impl AgentBackend for ClaudeBackend {
    fn name(&self) -> &'static str {
        "claude"
    }

    async fn spawn(
        &self,
        prompt: &str,
        working_dir: &str,
        session_id: Option<String>,
    ) -> Result<(Box<dyn AgentHandle>, mpsc::Receiver<AgentOutput>)> {
        let mut cmd = Command::new("claude");

        cmd.args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ]);

        // Add extra args
        for arg in &self.extra_args {
            cmd.arg(arg);
        }

        if let Some(id) = session_id {
            cmd.args(["--session-id", &id]);
        }

        cmd.current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| "Failed to spawn claude process. Is 'claude' in PATH?")?;

        let mut stdin = child.stdin.take().context("Failed to get stdin")?;
        let stdout = child.stdout.take().context("Failed to get stdout")?;

        // Send the prompt
        let input = ClaudeInput::user(prompt);
        let json = serde_json::to_string(&input)?;
        stdin.write_all(json.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        drop(stdin); // Close stdin to signal end of input

        // Channel for streaming responses
        let (tx, rx) = mpsc::channel::<AgentOutput>(256);

        // Spawn reader task
        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<ClaudeOutput>(&line) {
                    Ok(output) => {
                        let agent_output = convert_output(output);
                        let is_final = agent_output.is_final();
                        if tx.send(agent_output).await.is_err() {
                            break;
                        }
                        if is_final {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse Claude output: {} - line: {}", e, line);
                    }
                }
            }
        });

        Ok((Box::new(ClaudeHandle { child }), rx))
    }
}

/// Convert Claude-specific output to generic AgentOutput
fn convert_output(output: ClaudeOutput) -> AgentOutput {
    match output {
        ClaudeOutput::System(msg) => AgentOutput::System {
            session_id: msg.session_id,
        },
        ClaudeOutput::Assistant(msg) => {
            // Extract first text block
            for block in msg.message.content {
                if let ContentBlock::Text { text } = block {
                    return AgentOutput::Text(text);
                }
            }
            AgentOutput::Text(String::new())
        }
        ClaudeOutput::ToolUse(msg) => AgentOutput::ToolUse {
            id: msg.tool_use_id,
            name: msg.tool_name,
            input: msg.input,
        },
        ClaudeOutput::ToolResult(msg) => AgentOutput::ToolResult {
            id: msg.tool_use_id,
            output: msg.output.unwrap_or_default(),
            is_error: msg.is_error.unwrap_or(false),
        },
        ClaudeOutput::Result(msg) => AgentOutput::Result {
            text: msg.result,
            is_error: msg.is_error,
            session_id: msg.session_id,
        },
        ClaudeOutput::Error(msg) => {
            AgentOutput::Error(msg.error.or(msg.message).unwrap_or_default())
        }
        ClaudeOutput::Unknown => AgentOutput::Text(String::new()),
    }
}

// --- Claude Code stream-json protocol types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeInput {
    User { message: UserMessage },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UserMessage {
    role: String,
    content: String,
}

impl ClaudeInput {
    fn user(content: impl Into<String>) -> Self {
        Self::User {
            message: UserMessage {
                role: "user".to_string(),
                content: content.into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClaudeOutput {
    System(SystemMessage),
    Assistant(AssistantMessage),
    ToolUse(ToolUseMessage),
    ToolResult(ToolResultMessage),
    Result(ResultMessage),
    Error(ErrorMessage),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SystemMessage {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(flatten)]
    _extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssistantMessage {
    message: AssistantContent,
    #[serde(flatten)]
    _extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AssistantContent {
    #[serde(default)]
    content: Vec<ContentBlock>,
    #[serde(flatten)]
    _extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text { text: String },
    ToolUse { id: String, name: String, input: Value },
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolUseMessage {
    tool_use_id: String,
    tool_name: String,
    #[serde(default)]
    input: Value,
    #[serde(flatten)]
    _extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolResultMessage {
    tool_use_id: String,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(flatten)]
    _extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResultMessage {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(flatten)]
    _extra: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ErrorMessage {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(flatten)]
    _extra: Value,
}
