//! Claudius backend implementation
//!
//! Creates sessions in the Claudius desktop app via its HTTP API.
//! Each agent appears as a tab, allowing real-time observation and permission approval.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{AgentBackend, AgentHandle, AgentOutput};

/// Claudius HTTP API backend
pub struct ClaudiusBackend {
    base_url: String,
    client: reqwest::Client,
}

impl ClaudiusBackend {
    pub fn new(base_url: &str, password: Option<&str>) -> Self {
        let mut headers = HeaderMap::new();
        if let Some(pass) = password {
            let encoded =
                base64::engine::general_purpose::STANDARD.encode(format!("opencode:{}", pass));
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {}", encoded))
                    .expect("valid auth header value"),
            );
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .expect("failed to build HTTP client");

        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
        }
    }
}

/// Handle to a running Claudius session request
struct ClaudiusHandle {
    session_id: String,
    base_url: String,
    client: reqwest::Client,
    task: Option<JoinHandle<()>>,
}

#[async_trait]
impl AgentHandle for ClaudiusHandle {
    async fn abort(&mut self) -> Result<()> {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let url = format!("{}/session/{}/abort", self.base_url, self.session_id);
        let _ = self.client.post(&url).send().await;
        Ok(())
    }

    async fn wait(&mut self) -> Result<()> {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        Ok(())
    }
}

#[async_trait]
impl AgentBackend for ClaudiusBackend {
    fn name(&self) -> &'static str {
        "claudius"
    }

    async fn init_session(
        &self,
        working_dir: &str,
        title: Option<&str>,
        system_prompt: Option<&str>,
        permission_mode: Option<&str>,
    ) -> Result<Option<String>> {
        let mode = permission_mode.unwrap_or("acceptEdits");
        let (sid, is_new) =
            find_or_create_session(&self.client, &self.base_url, working_dir, title, permission_mode).await?;
        if is_new {
            if let Some(prompt) = system_prompt {
                send_system_prompt(
                    &self.client,
                    &self.base_url,
                    &sid,
                    prompt,
                    working_dir,
                    mode,
                )
                .await?;
            }
        }
        Ok(Some(sid))
    }

    async fn spawn(
        &self,
        prompt: &str,
        working_dir: &str,
        session_id: Option<String>,
        title: Option<&str>,
        permission_mode: Option<&str>,
    ) -> Result<(Box<dyn AgentHandle>, mpsc::Receiver<AgentOutput>)> {
        let (tx, rx) = mpsc::channel(256);

        let sid = match session_id {
            Some(id) => id,
            None => {
                let (id, _) =
                    find_or_create_session(&self.client, &self.base_url, working_dir, title, permission_mode)
                        .await?;
                id
            }
        };

        let _ = tx
            .send(AgentOutput::System {
                session_id: Some(sid.clone()),
            })
            .await;

        let mode = permission_mode.unwrap_or("acceptEdits").to_string();
        let task = spawn_message_task(
            self.client.clone(),
            self.base_url.clone(),
            mode,
            sid.clone(),
            prompt.to_string(),
            working_dir.to_string(),
            tx,
        );

        let handle = ClaudiusHandle {
            session_id: sid,
            base_url: self.base_url.clone(),
            client: self.client.clone(),
            task: Some(task),
        };

        Ok((Box::new(handle), rx))
    }
}

fn spawn_message_task(
    client: reqwest::Client,
    base_url: String,
    permission_mode: String,
    session_id: String,
    prompt: String,
    working_dir: String,
    tx: mpsc::Sender<AgentOutput>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = send_and_process(
            &client,
            &base_url,
            &session_id,
            &prompt,
            &working_dir,
            &permission_mode,
            &tx,
        )
        .await
        {
            let _ = tx.send(AgentOutput::Error(e.to_string())).await;
        }
    })
}

/// Find an existing session by title (case-insensitive), or create a new one.
async fn find_or_create_session(
    client: &reqwest::Client,
    base_url: &str,
    working_dir: &str,
    title: Option<&str>,
    permission_mode: Option<&str>,
) -> Result<(String, bool)> {
    if let Some(title) = title {
        if let Some(id) = find_session_by_title(client, base_url, working_dir, title).await? {
            tracing::info!("Reusing Claudius session: {} ({})", id, title);
            return Ok((id, false));
        }
    }
    let id = create_session(client, base_url, working_dir, title, permission_mode).await?;
    Ok((id, true))
}

async fn find_session_by_title(
    client: &reqwest::Client,
    base_url: &str,
    working_dir: &str,
    title: &str,
) -> Result<Option<String>> {
    let url = format!("{}/session", base_url);
    let resp = client
        .get(&url)
        .query(&[("directory", working_dir)])
        .send()
        .await
        .context("Failed to list Claudius sessions")?;

    if !resp.status().is_success() {
        return Ok(None);
    }

    let sessions: Vec<SessionListEntry> = resp
        .json()
        .await
        .unwrap_or_default();

    let needle = title.to_lowercase();
    Ok(sessions
        .into_iter()
        .find(|s| s.title.to_lowercase() == needle)
        .map(|s| s.id))
}

async fn create_session(
    client: &reqwest::Client,
    base_url: &str,
    working_dir: &str,
    title: Option<&str>,
    permission_mode: Option<&str>,
) -> Result<String> {
    let url = format!("{}/session", base_url);
    let mut body = serde_json::json!({ "title": title.unwrap_or("Orchestrator") });
    if let Some(mode) = permission_mode {
        body["permissionMode"] = serde_json::Value::String(mode.to_string());
    }

    let resp = client
        .post(&url)
        .query(&[("directory", working_dir)])
        .json(&body)
        .send()
        .await
        .context("Failed to create Claudius session")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Create session failed ({}): {}", status, text);
    }

    let session: CreateSessionResponse = resp
        .json()
        .await
        .context("Failed to parse session response")?;

    tracing::info!("Created Claudius session: {}", session.id);
    Ok(session.id)
}

/// Send the system prompt as the first message so the agent has instructions
/// even when users interact with the session directly through Claudius.
async fn send_system_prompt(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
    prompt: &str,
    working_dir: &str,
    permission_mode: &str,
) -> Result<()> {
    let url = format!("{}/session/{}/message", base_url, session_id);
    let body = serde_json::json!({
        "parts": [{ "type": "text", "text": prompt }],
        "permissionMode": permission_mode,
    });

    let resp = client
        .post(&url)
        .query(&[("directory", working_dir)])
        .json(&body)
        .send()
        .await
        .context("Failed to send system prompt to Claudius")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("System prompt failed ({}): {}", status, text);
    }

    tracing::info!("Sent system prompt to session {}", session_id);
    Ok(())
}

async fn send_and_process(
    client: &reqwest::Client,
    base_url: &str,
    session_id: &str,
    prompt: &str,
    working_dir: &str,
    permission_mode: &str,
    tx: &mpsc::Sender<AgentOutput>,
) -> Result<()> {
    let url = format!("{}/session/{}/message", base_url, session_id);
    let body = serde_json::json!({
        "parts": [{ "type": "text", "text": prompt }],
        "permissionMode": permission_mode,
    });

    let resp = client
        .post(&url)
        .query(&[("directory", working_dir)])
        .json(&body)
        .send()
        .await
        .context("Failed to send message to Claudius")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        bail!("Claudius message failed ({}): {}", status, text);
    }

    let response: MessageResponse = resp
        .json()
        .await
        .context("Failed to parse Claudius message response")?;

    let result_text = emit_parts(&response.parts, tx).await;

    let _ = tx
        .send(AgentOutput::Result {
            text: if result_text.is_empty() {
                None
            } else {
                Some(result_text)
            },
            is_error: response.info.error.is_some(),
            session_id: Some(session_id.to_string()),
        })
        .await;

    Ok(())
}

async fn emit_parts(parts: &[MessagePart], tx: &mpsc::Sender<AgentOutput>) -> String {
    let mut result_text = String::new();

    for part in parts {
        match part {
            MessagePart::Text { text } => {
                if !text.is_empty() {
                    result_text.push_str(text);
                    let _ = tx.send(AgentOutput::Text(text.clone())).await;
                }
            }
            MessagePart::Tool {
                call_id,
                tool,
                state,
            } => {
                emit_tool_part(call_id, tool, state, tx).await;
            }
            MessagePart::Unknown => {}
        }
    }

    result_text
}

async fn emit_tool_part(
    call_id: &str,
    tool: &str,
    state: &ToolState,
    tx: &mpsc::Sender<AgentOutput>,
) {
    let _ = tx
        .send(AgentOutput::ToolUse {
            id: call_id.to_string(),
            name: tool.to_string(),
            input: state.input.clone(),
        })
        .await;

    match state.status.as_str() {
        "completed" => {
            let _ = tx
                .send(AgentOutput::ToolResult {
                    id: call_id.to_string(),
                    output: value_as_string(&state.output),
                    is_error: false,
                })
                .await;
        }
        "error" => {
            let _ = tx
                .send(AgentOutput::ToolResult {
                    id: call_id.to_string(),
                    output: state.error.clone().unwrap_or_default(),
                    is_error: true,
                })
                .await;
        }
        _ => {}
    }
}

fn value_as_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// --- Claudius API response types ---

#[derive(Debug, Deserialize)]
struct SessionListEntry {
    id: String,
    title: String,
}

#[derive(Debug, Deserialize)]
struct CreateSessionResponse {
    id: String,
}

#[derive(Debug, Deserialize)]
struct MessageResponse {
    info: MessageInfo,
    parts: Vec<MessagePart>,
}

#[derive(Debug, Deserialize)]
struct MessageInfo {
    #[allow(dead_code)]
    id: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum MessagePart {
    Text {
        #[serde(default)]
        text: String,
    },
    Tool {
        #[serde(rename = "callID")]
        call_id: String,
        tool: String,
        state: ToolState,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct ToolState {
    status: String,
    #[serde(default)]
    input: Value,
    #[serde(default)]
    output: Value,
    #[serde(default)]
    error: Option<String>,
}
