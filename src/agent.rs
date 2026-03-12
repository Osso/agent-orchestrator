//! Agent runtime that combines llm-sdk backend and agent-bus transport
//!
//! Each agent:
//! 1. Registers on the in-process message bus
//! 2. Waits for messages from other agents
//! 3. Calls llm-sdk to get a completion (with MCP tools for outbound communication)

use std::sync::{Arc, Mutex};

use agent_bus::{Bus, Mailbox};
use anyhow::Result;
use async_trait::async_trait;
use llm_sdk::claude::Claude;
use llm_sdk::session::{LogEntry, Session, SessionStore, append_log, now_utc};

use crate::types::{AgentId, AgentRole};

/// Tools blocked for non-task agents (currently unused, all agents get full tools).
const DISALLOWED_TOOLS: &[&str] = &["Bash", "Write", "Edit", "NotebookEdit", "Agent"];

/// Which backend to use for completions.
#[derive(Clone, Debug)]
pub enum BackendKind {
    Claude,
    OpenRouter { model: String, api_key: String },
    Codex { model: String },
}

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

/// Production completer backed by OpenRouter chat API with MessageLog persistence.
struct OpenRouterCompleter {
    openrouter: Arc<llm_sdk::openrouter::OpenRouter>,
    log: llm_sdk::MessageLog,
}

#[async_trait]
impl Completer for OpenRouterCompleter {
    async fn complete(&mut self, prompt: &str) -> Result<llm_sdk::Output, llm_sdk::Error> {
        self.openrouter.complete_chat(&mut self.log, prompt).await
    }
}

/// Production completer backed by Codex (ChatGPT Pro OAuth) with MessageLog persistence.
struct CodexCompleter {
    codex: Arc<llm_sdk::codex::Codex>,
    log: Arc<Mutex<llm_sdk::MessageLog>>,
}

#[async_trait]
impl Completer for CodexCompleter {
    async fn complete(&mut self, prompt: &str) -> Result<llm_sdk::Output, llm_sdk::Error> {
        self.codex.complete_with_log(prompt, self.log.clone()).await
    }
}

/// Context needed to issue a fresh completer for each task.
enum FreshCtx {
    Claude {
        store: SessionStore,
        key: String,
        system_prompt: String,
        base_claude: Box<Claude>,
    },
    OpenRouter {
        store: SessionStore,
        key: String,
        openrouter: Arc<llm_sdk::openrouter::OpenRouter>,
    },
    Codex {
        store: SessionStore,
        key: String,
        codex: Arc<llm_sdk::codex::Codex>,
    },
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
    /// When true, reset the session/log before each task_assignment message.
    pub fresh_session_per_task: bool,
    /// Backend to use for completions.
    pub backend: BackendKind,
    /// Session store for persistence.
    pub session_store: SessionStore,
    /// Bus for OpenRouter bus tools (None for Claude backend / tests).
    pub bus: Option<Bus>,
    /// Bwrap command prefix for sandboxing (empty = no sandbox).
    pub sandbox_prefix: Vec<String>,
}

/// Running agent instance
pub struct Agent {
    config: AgentConfig,
    mailbox: Mailbox,
    completer: Box<dyn Completer>,
    fresh_ctx: Option<FreshCtx>,
    /// Last task_assignment content received (for completion verification).
    last_task: Option<String>,
}

impl Agent {
    pub fn new(config: AgentConfig, mailbox: Mailbox) -> Result<Self> {
        let bus_name = config.agent_id.bus_name();
        let (completer, fresh_ctx) = build_completer(&config, &bus_name)?;
        Ok(Self {
            config,
            mailbox,
            completer,
            fresh_ctx,
            last_task: None,
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
            fresh_ctx: None,
            last_task: None,
        }
    }

    /// Run the agent main loop
    pub async fn run(mut self) -> Result<()> {
        tracing::info!("Agent {} started", self.config.agent_id);
        self.process_initial_task().await;
        while let Some(msg) = self.mailbox.recv().await {
            self.handle_bus_message(msg).await;
        }
        tracing::info!("Agent {} stopped", self.config.agent_id);
        Ok(())
    }

    async fn handle_bus_message(&mut self, msg: agent_bus::BusMessage) {
        tracing::info!(
            "Agent {} received '{}' from {}",
            self.config.agent_id,
            msg.kind,
            msg.from
        );
        let is_task = msg.kind == "task_assignment";
        if is_task {
            self.reset_completer_for_task();
            self.last_task = Some(extract_content(&msg.payload));
        }
        if msg.kind == "external_message" {
            self.log_external_message(&msg.payload);
        }
        let content = format_prompt(&msg, is_task);
        tracing::info!(
            "Agent {} processing '{}' ({} bytes)",
            self.config.agent_id,
            msg.kind,
            content.len()
        );
        self.dispatch_completion(&content, is_task).await;
    }

    fn log_external_message(&self, payload: &serde_json::Value) {
        let text = extract_content(payload);
        let data_dir = self.config.session_store.data_dir();
        let key = self.config.agent_id.bus_name();
        append_log(
            &data_dir,
            &key,
            &LogEntry::User {
                text,
                timestamp: now_utc(),
            },
        );
    }

    async fn dispatch_completion(&mut self, content: &str, is_task: bool) {
        match self.process_prompt(content).await {
            Ok(output) if is_task => {
                self.auto_report_completion(&output.text);
            }
            Ok(output) => {
                tracing::info!(
                    "Agent {} responded ({} bytes)",
                    self.config.agent_id,
                    output.text.len()
                );
            }
            Err(e) if is_task => {
                tracing::error!("Agent {} completion failed: {}", self.config.agent_id, e);
                self.auto_report_blocked(&e.to_string());
            }
            Err(e) => {
                tracing::error!("Agent {} completion failed: {}", self.config.agent_id, e);
            }
        }
    }

    async fn process_initial_task(&mut self) {
        let Some(task) = self.config.initial_task.take() else {
            return;
        };
        tracing::info!("Agent {} processing initial task", self.config.agent_id);
        self.last_task = Some(task.clone());
        // No auto_report here — the manager (only role with initial_task)
        // handles its own lifecycle via goal_complete MCP tool.
        if let Err(e) = self.process_prompt(&task).await {
            tracing::error!("Agent {} initial task failed: {}", self.config.agent_id, e);
        }
    }

    fn reset_completer_for_task(&mut self) {
        let Some(ctx) = &self.fresh_ctx else {
            return;
        };
        match ctx {
            FreshCtx::Claude {
                store,
                key,
                system_prompt,
                base_claude,
            } => {
                store.remove(key);
                let session = store.session(key).system_prompt(system_prompt);
                self.completer = Box::new(SessionCompleter {
                    session,
                    claude: *base_claude.clone(),
                });
            }
            FreshCtx::OpenRouter {
                store,
                key,
                openrouter,
            } => {
                store.remove_message_log(key);
                let log = store.message_log(key);
                self.completer = Box::new(OpenRouterCompleter {
                    openrouter: openrouter.clone(),
                    log,
                });
            }
            FreshCtx::Codex { store, key, codex } => {
                store.remove_message_log(key);
                let log = Arc::new(Mutex::new(store.message_log(key)));
                self.completer = Box::new(CodexCompleter {
                    codex: codex.clone(),
                    log,
                });
            }
        }
        tracing::info!("Agent {} fresh completer for task", self.config.agent_id);
    }

    /// Signal task completion to the runtime (which handles DB transitions and routing).
    fn auto_report_completion(&self, text: &str) {
        let output = if text.is_empty() {
            "Task completed (no output)".to_string()
        } else {
            truncate(text, 2000)
        };
        let payload = serde_json::json!({ "content": output });
        let _ = self.mailbox.send("runtime", "task_complete", payload);
    }

    /// Signal task blocked/needs_info to the runtime.
    fn auto_report_blocked(&self, error: &str) {
        let payload = serde_json::json!({ "content": format!("Task failed: {error}") });
        let _ = self.mailbox.send("runtime", "task_blocked", payload);
    }

    async fn process_prompt(&mut self, content: &str) -> Result<llm_sdk::Output> {
        let output = self.completer.complete(content).await?;
        log_completion(&self.config.agent_id, &output);
        Ok(output)
    }
}

fn build_completer(
    config: &AgentConfig,
    bus_name: &str,
) -> Result<(Box<dyn Completer>, Option<FreshCtx>)> {
    match &config.backend {
        BackendKind::Claude => build_claude_completer(config, bus_name),
        BackendKind::OpenRouter { model, api_key } => {
            build_openrouter_completer(config, bus_name, model, api_key)
        }
        BackendKind::Codex { model } => build_codex_completer(config, bus_name, model),
    }
}

fn build_claude_completer(
    config: &AgentConfig,
    bus_name: &str,
) -> Result<(Box<dyn Completer>, Option<FreshCtx>)> {
    let perm_mode = permission_mode_for_role(config.agent_id.role);
    let mut base_claude = Claude::new()?
        .permission_mode(perm_mode)
        .working_dir(&config.working_dir)
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .command_prefix(config.sandbox_prefix.clone());
    if !role_has_tools(config.agent_id.role) {
        base_claude =
            base_claude.disallowed_tools(DISALLOWED_TOOLS.iter().map(|s| s.to_string()).collect());
    }
    if let Some(ref cfg) = config.mcp_config {
        base_claude = base_claude.mcp_config(cfg);
    }

    let session = config
        .session_store
        .session(bus_name)
        .system_prompt(&config.system_prompt);
    let completer: Box<dyn Completer> = Box::new(SessionCompleter {
        session,
        claude: base_claude.clone(),
    });

    let fresh_ctx = if config.fresh_session_per_task {
        Some(FreshCtx::Claude {
            store: config.session_store.clone(),
            key: bus_name.to_string(),
            system_prompt: config.system_prompt.clone(),
            base_claude: Box::new(base_claude),
        })
    } else {
        None
    };

    Ok((completer, fresh_ctx))
}

fn build_openrouter_completer(
    config: &AgentConfig,
    bus_name: &str,
    model: &str,
    api_key: &str,
) -> Result<(Box<dyn Completer>, Option<FreshCtx>)> {
    let mut builder = llm_sdk::openrouter::OpenRouter::new(model)
        .api_key(api_key)
        .system_prompt(&config.system_prompt);
    let tools = build_openrouter_tools(config, bus_name);
    let tool_names: Vec<String> = tools.definitions().iter().map(|d| d.name.clone()).collect();
    tracing::info!("OpenRouter tools for {}: {:?}", bus_name, tool_names);
    if !tools.is_empty() {
        builder = builder.tools(tools);
    }
    let openrouter = Arc::new(builder);

    let log = config.session_store.message_log(bus_name);
    let completer: Box<dyn Completer> = Box::new(OpenRouterCompleter {
        openrouter: openrouter.clone(),
        log,
    });

    let fresh_ctx = if config.fresh_session_per_task {
        Some(FreshCtx::OpenRouter {
            store: config.session_store.clone(),
            key: bus_name.to_string(),
            openrouter,
        })
    } else {
        None
    };

    Ok((completer, fresh_ctx))
}

fn build_codex_completer(
    config: &AgentConfig,
    bus_name: &str,
    model: &str,
) -> Result<(Box<dyn Completer>, Option<FreshCtx>)> {
    let mut builder = llm_sdk::codex::Codex::new(model).system_prompt(&config.system_prompt);
    let tools = build_openrouter_tools(config, bus_name);
    let tool_names: Vec<String> = tools.definitions().iter().map(|d| d.name.clone()).collect();
    tracing::info!("Codex tools for {}: {:?}", bus_name, tool_names);
    if !tools.is_empty() {
        builder = builder.tools(tools);
    }
    let codex = Arc::new(builder);

    let log = Arc::new(Mutex::new(config.session_store.message_log(bus_name)));
    let completer: Box<dyn Completer> = Box::new(CodexCompleter {
        codex: codex.clone(),
        log,
    });

    let fresh_ctx = if config.fresh_session_per_task {
        Some(FreshCtx::Codex {
            store: config.session_store.clone(),
            key: bus_name.to_string(),
            codex,
        })
    } else {
        None
    };

    Ok((completer, fresh_ctx))
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

fn format_prompt(msg: &agent_bus::BusMessage, is_task: bool) -> String {
    if is_task {
        extract_content(&msg.payload)
    } else {
        format!("[{}] {}", msg.kind, extract_content(&msg.payload))
    }
}

fn extract_content(payload: &serde_json::Value) -> String {
    // Prefer "content" field, fall back to full JSON (for merge_request etc.)
    if let Some(content) = payload.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            return content.to_string();
        }
    }
    serde_json::to_string_pretty(payload).unwrap_or_default()
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        text.to_string()
    } else {
        let truncated: String = text.chars().take(max_chars).collect();
        format!("{}...(truncated)", truncated)
    }
}

pub fn permission_mode_for_role(role: AgentRole) -> &'static str {
    // All agents use bypassPermissions — bwrap sandbox is the real security boundary.
    match role {
        AgentRole::TaskAgent | AgentRole::Merger => "bypassPermissions",
    }
}

pub fn role_has_tools(role: AgentRole) -> bool {
    matches!(role, AgentRole::TaskAgent | AgentRole::Merger)
}

/// Build the ToolSet for an OpenRouter agent: file tools for developers, bus tools for all.
fn build_openrouter_tools(config: &AgentConfig, bus_name: &str) -> llm_sdk::tools::ToolSet {
    let mut set = if role_has_tools(config.agent_id.role) {
        if config.sandbox_prefix.is_empty() {
            llm_sdk::tools::ToolSet::standard_with_cwd(&config.working_dir)
        } else {
            llm_sdk::tools::ToolSet::standard_sandboxed(config.sandbox_prefix.clone())
        }
    } else {
        llm_sdk::tools::ToolSet::new()
    };

    if let Some(ref bus) = config.bus {
        let tools_name = format!("{}-tools", bus_name);
        match bus.register(&tools_name) {
            Ok(mailbox) => {
                let bus_set =
                    crate::bus_tools::bus_tools_for_role(config.agent_id.role, Arc::new(mailbox));
                set = set.merge(bus_set);
            }
            Err(e) => tracing::warn!("Failed to register bus tools for {}: {}", bus_name, e),
        }
    }

    set
}
