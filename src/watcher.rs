//! SSE watcher for external message routing
//!
//! Subscribes to the Claudius server's SSE event stream and routes
//! structured output from UI-initiated messages through the orchestrator.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::agent::{parse_agent_output, send_to_agent, ParsedOutput};
use crate::backend::fetch_message;
use crate::runtime::RuntimeCommand;
use crate::types::AgentId;

/// Shared state tracking which sessions belong to which agents
pub struct SessionRegistry {
    /// Maps session_id -> owning agent
    pub sessions: HashMap<String, AgentId>,
    /// Sessions with pending orchestrator-initiated requests
    pub busy: HashSet<String>,
    /// Message IDs already processed by agents (prevents double-routing)
    pub processed: HashSet<String>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            busy: HashSet::new(),
            processed: HashSet::new(),
        }
    }
}

/// Configuration for the SSE watcher
pub struct WatcherConfig {
    pub base_url: String,
    pub client: reqwest::Client,
    pub working_dir: String,
}

/// Spawn the SSE watcher as a background task
pub fn spawn_watcher(
    config: WatcherConfig,
    registry: Arc<Mutex<SessionRegistry>>,
    cmd_tx: mpsc::Sender<RuntimeCommand>,
    base_path: PathBuf,
) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = run_sse_loop(&config, &registry, &cmd_tx, &base_path).await {
                tracing::warn!("SSE watcher disconnected: {}", e);
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            tracing::info!("SSE watcher reconnecting...");
        }
    });
}

/// Connect to SSE and process events until disconnection
async fn run_sse_loop(
    config: &WatcherConfig,
    registry: &Arc<Mutex<SessionRegistry>>,
    cmd_tx: &mpsc::Sender<RuntimeCommand>,
    base_path: &PathBuf,
) -> anyhow::Result<()> {
    let url = format!("{}/global/event", config.base_url);
    tracing::info!("SSE watcher connecting to {}", url);

    let mut resp = config
        .client
        .get(&url)
        .query(&[("directory", &config.working_dir)])
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("SSE endpoint returned {}", resp.status());
    }

    tracing::info!("SSE watcher connected");

    let mut buffer = String::new();

    while let Some(chunk) = resp.chunk().await? {
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        // Process complete SSE events (terminated by \n\n)
        while let Some(pos) = buffer.find("\n\n") {
            let event = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();

            if let Some(data) = extract_sse_data(&event) {
                handle_sse_event(data, config, registry, cmd_tx, base_path).await;
            }
        }
    }

    Ok(())
}

/// Extract the JSON data from an SSE event block
fn extract_sse_data(event: &str) -> Option<&str> {
    for line in event.lines() {
        if let Some(data) = line.strip_prefix("data: ") {
            return Some(data.trim());
        }
    }
    None
}

/// Parsed fields from an sdk.completed SSE event
struct CompletionEvent {
    session_id: String,
    message_id: String,
}

/// Try to parse an SSE data payload as an sdk.completed event
fn parse_completion_event(data: &str) -> Option<CompletionEvent> {
    let json: serde_json::Value = serde_json::from_str(data).ok()?;
    let payload = json.get("payload")?;

    let event_type = payload.get("type").and_then(|t| t.as_str())?;
    if event_type != "sdk.completed" {
        return None;
    }

    let props = payload.get("properties")?;
    Some(CompletionEvent {
        session_id: props.get("sessionID").and_then(|s| s.as_str())?.to_string(),
        message_id: props.get("messageID").and_then(|s| s.as_str())?.to_string(),
    })
}

/// Check if an event should be routed: returns the owning agent if so
fn check_registry(
    registry: &Arc<Mutex<SessionRegistry>>,
    event: &CompletionEvent,
) -> Option<AgentId> {
    let reg = registry.lock().unwrap();

    let agent_id = reg.sessions.get(&event.session_id)?.clone();

    if reg.busy.contains(&event.session_id) {
        tracing::debug!("SSE watcher: skipping busy session {}", event.session_id);
        return None;
    }

    if reg.processed.contains(&event.message_id) {
        tracing::debug!("SSE watcher: skipping processed message {}", event.message_id);
        return None;
    }

    Some(agent_id)
}

/// Handle a single SSE event
async fn handle_sse_event(
    data: &str,
    config: &WatcherConfig,
    registry: &Arc<Mutex<SessionRegistry>>,
    cmd_tx: &mpsc::Sender<RuntimeCommand>,
    base_path: &PathBuf,
) {
    let event = match parse_completion_event(data) {
        Some(e) => e,
        None => return,
    };

    let agent_id = match check_registry(registry, &event) {
        Some(id) => id,
        None => return,
    };

    tracing::info!(
        "SSE watcher: external completion for {} (session={}, msg={})",
        agent_id,
        event.session_id,
        event.message_id
    );

    match fetch_message(
        &config.client,
        &config.base_url,
        &event.session_id,
        &event.message_id,
        &config.working_dir,
    )
    .await
    {
        Ok(text) => {
            if text.is_empty() {
                return;
            }
            registry.lock().unwrap().processed.insert(event.message_id);
            route_external_output(&agent_id, &text, cmd_tx, base_path).await;
        }
        Err(e) => {
            tracing::error!("SSE watcher: failed to fetch message: {}", e);
        }
    }
}

/// Parse and route structured output from an external message
async fn route_external_output(
    agent_id: &AgentId,
    text: &str,
    cmd_tx: &mpsc::Sender<RuntimeCommand>,
    base_path: &PathBuf,
) {
    for parsed in parse_agent_output(agent_id, text) {
        match parsed {
            ParsedOutput::Message(msg) => {
                tracing::info!(
                    "SSE watcher: routing {} -> {} ({:?})",
                    agent_id,
                    msg.to,
                    msg.kind
                );
                if let Err(e) = send_to_agent(&msg, base_path).await {
                    tracing::error!("SSE watcher: failed to send to {}: {}", msg.to, e);
                }
            }
            ParsedOutput::Command(cmd) => {
                tracing::info!("SSE watcher: routing {} -> runtime ({:?})", agent_id, cmd);
                let _ = cmd_tx.send(cmd).await;
            }
        }
    }
}
