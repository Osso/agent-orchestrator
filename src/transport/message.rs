//! Wire protocol messages for inter-agent communication

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::AgentId;

/// Message sent between agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// Unique message ID
    pub id: Uuid,
    /// Sender
    pub from: AgentId,
    /// Recipient
    pub to: AgentId,
    /// Message type
    pub kind: MessageKind,
    /// Message content
    pub content: String,
    /// Optional task reference
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<Uuid>,
}

impl AgentMessage {
    pub fn new(from: AgentId, to: AgentId, kind: MessageKind, content: String) -> Self {
        Self {
            id: Uuid::new_v4(),
            from,
            to,
            kind,
            content,
            task_id: None,
        }
    }

    pub fn with_task(mut self, task_id: Uuid) -> Self {
        self.task_id = Some(task_id);
        self
    }
}

/// Types of messages between agents
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    /// New task assignment
    TaskAssignment,
    /// Task completed successfully
    TaskComplete,
    /// Agent gave up on task
    TaskGiveUp,
    /// Request to interrupt current work
    Interrupt,
    /// Architect review/feedback
    ArchitectReview,
    /// General information
    Info,
    /// Scorer evaluation (informational only)
    Evaluation,
    /// Scorer observation (informational only)
    Observation,
}

/// Length-prefixed message encoding (4 bytes big-endian length + JSON)
pub mod wire {
    use anyhow::{bail, Context, Result};
    use serde::{de::DeserializeOwned, Serialize};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    const MAX_MESSAGE_SIZE: usize = 16 * 1024 * 1024; // 16 MB

    /// Write a message with length prefix
    pub async fn write_message<W, T>(writer: &mut W, msg: &T) -> Result<()>
    where
        W: AsyncWrite + Unpin,
        T: Serialize,
    {
        let json = serde_json::to_vec(msg)?;
        if json.len() > MAX_MESSAGE_SIZE {
            bail!("Message too large: {} bytes", json.len());
        }

        let len = (json.len() as u32).to_be_bytes();
        writer.write_all(&len).await.context("Failed to write length")?;
        writer.write_all(&json).await.context("Failed to write message")?;
        writer.flush().await.context("Failed to flush")?;
        Ok(())
    }

    /// Read a length-prefixed message
    pub async fn read_message<R, T>(reader: &mut R) -> Result<T>
    where
        R: AsyncRead + Unpin,
        T: DeserializeOwned,
    {
        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf).await.context("Failed to read length")?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len > MAX_MESSAGE_SIZE {
            bail!("Message too large: {} bytes", len);
        }

        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await.context("Failed to read message")?;

        serde_json::from_slice(&buf).context("Failed to parse message")
    }
}
