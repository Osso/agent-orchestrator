mod agent;
mod backend;
mod runtime;
mod transport;
mod types;
mod watcher;

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::info;
use tracing_subscriber::EnvFilter;

use agent::{Agent, AgentConfig};
use backend::{AgentBackend, ClaudeBackend, ClaudiusBackend, build_claudius_client};
use runtime::{OrchestratorRuntime, RuntimeCommand};
use transport::{AgentConnection, AgentMessage, MessageKind};
use types::{AgentId, AgentRole};
use watcher::{SessionRegistry, WatcherConfig};

const DEFAULT_SOCKET_PATH: &str = "/tmp/claude/orchestrator";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("agent_orchestrator=info".parse()?),
        )
        .init();

    let mut args: Vec<String> = std::env::args().collect();
    let opts = extract_global_opts(&mut args);

    if args.len() < 2 {
        print_usage();
        return Ok(());
    }

    match args[1].as_str() {
        "run" => {
            if args.len() < 4 {
                bail!("Usage: agent-orchestrator run <dir> <task...>");
            }
            let working_dir = &args[2];
            let task = args[3..].join(" ");
            let watcher = build_watcher_config(&opts, working_dir);
            run_task(working_dir, &task, create_backend(&opts), watcher).await
        }
        "agent" => {
            if args.len() < 3 {
                bail!("Usage: agent-orchestrator agent <role> [working_dir]");
            }
            let role = parse_role(&args[2])?;
            let working_dir = args.get(3).map(|s| s.as_str()).unwrap_or(".");
            run_agent(role, working_dir, create_backend(&opts)).await
        }
        "orchestrate" => {
            let working_dir = args.get(2).map(|s| s.as_str()).unwrap_or(".");
            let watcher = build_watcher_config(&opts, working_dir);
            run_orchestrator(working_dir, create_backend(&opts), watcher).await
        }
        "send" => {
            if args.len() < 4 {
                bail!("Usage: agent-orchestrator send <to_role> <message>");
            }
            let to = parse_role(&args[2])?;
            let message = args[3..].join(" ");
            send_message(to, &message).await
        }
        "status" => show_status().await,
        _ => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    eprintln!(
        r#"Agent Orchestrator - Multi-agent coordination for AI coding assistants

USAGE:
    agent-orchestrator [OPTIONS] <COMMAND>

OPTIONS:
    --backend <name>             Backend: claude (default), claudius
    --claudius-url <url>         Claudius API URL (default: http://127.0.0.1:43527)
    --claudius-password <pass>   Claudius password (or OPENCODE_SERVER_PASSWORD env)

COMMANDS:
    run <dir> <task...>     Run agents on a task (non-interactive)

    orchestrate [dir]       Start agents and wait for messages

    send <role> <message>   Send a message to a running agent

    agent <role> [dir]      Run a single agent standalone

    status                  Show socket status

EXAMPLES:
    # Run a task non-interactively
    agent-orchestrator run ~/my-project "Add a login button to the homepage"

    # Use Claudius backend (agents appear as tabs in the desktop app)
    agent-orchestrator --backend claudius orchestrate ~/my-project

    # Start agents, then send tasks separately
    agent-orchestrator orchestrate ~/my-project
    agent-orchestrator send manager "Add a login button"
"#
    );
}

struct GlobalOpts {
    backend: String,
    claudius_url: String,
    claudius_password: Option<String>,
}

fn extract_global_opts(args: &mut Vec<String>) -> GlobalOpts {
    let mut backend = "claude".to_string();
    let mut claudius_url = "http://127.0.0.1:43527".to_string();
    let mut claudius_password = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--backend" if i + 1 < args.len() => {
                backend = args[i + 1].clone();
                args.drain(i..i + 2);
            }
            "--claudius-url" if i + 1 < args.len() => {
                claudius_url = args[i + 1].clone();
                args.drain(i..i + 2);
            }
            "--claudius-password" if i + 1 < args.len() => {
                claudius_password = Some(args[i + 1].clone());
                args.drain(i..i + 2);
            }
            _ => i += 1,
        }
    }

    // Fall back to env var for password
    if claudius_password.is_none() {
        claudius_password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();
    }

    GlobalOpts {
        backend,
        claudius_url,
        claudius_password,
    }
}

fn create_backend(opts: &GlobalOpts) -> Arc<dyn AgentBackend> {
    match opts.backend.as_str() {
        "claudius" => {
            info!("Using Claudius backend at {}", opts.claudius_url);
            Arc::new(ClaudiusBackend::new(
                &opts.claudius_url,
                opts.claudius_password.as_deref(),
            ))
        }
        _ => Arc::new(ClaudeBackend::new()),
    }
}

fn build_watcher_config(opts: &GlobalOpts, working_dir: &str) -> Option<WatcherConfig> {
    if opts.backend != "claudius" {
        return None;
    }
    Some(WatcherConfig {
        base_url: opts.claudius_url.trim_end_matches('/').to_string(),
        client: build_claudius_client(opts.claudius_password.as_deref()),
        working_dir: working_dir.to_string(),
    })
}

fn parse_role(s: &str) -> Result<AgentRole> {
    match s.to_lowercase().as_str() {
        "manager" => Ok(AgentRole::Manager),
        "architect" => Ok(AgentRole::Architect),
        "developer" => Ok(AgentRole::Developer),
        "scorer" => Ok(AgentRole::Scorer),
        _ => bail!(
            "Unknown role: {}. Use: manager, architect, developer, scorer",
            s
        ),
    }
}

async fn run_agent(role: AgentRole, working_dir: &str, backend: Arc<dyn AgentBackend>) -> Result<()> {
    let agent_id = AgentId::new_singleton(role);
    info!("Starting agent {} in {} (backend: {})", agent_id, working_dir, backend.name());

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    // Standalone mode: commands are logged but not handled
    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<RuntimeCommand>(64);
    tokio::spawn(async move {
        while let Some(cmd) = command_rx.recv().await {
            tracing::info!("Standalone agent received runtime command: {:?}", cmd);
        }
    });

    let registry = Arc::new(Mutex::new(SessionRegistry::new()));
    let config = AgentConfig {
        agent_id,
        working_dir: working_dir.to_string(),
        system_prompt: role.system_prompt().to_string(),
        initial_task: None,
    };

    let agent = Agent::new(config, backend, &base_path, command_tx, registry).await?;
    agent.run().await
}

async fn run_task(
    working_dir: &str,
    task: &str,
    backend: Arc<dyn AgentBackend>,
    watcher_config: Option<WatcherConfig>,
) -> Result<()> {
    info!("Running task in {}: {}", working_dir, task);

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    let runtime = OrchestratorRuntime::new(backend, base_path, working_dir.to_string(), watcher_config);
    runtime.run_with_task(task.to_string()).await
}

async fn run_orchestrator(
    working_dir: &str,
    backend: Arc<dyn AgentBackend>,
    watcher_config: Option<WatcherConfig>,
) -> Result<()> {
    info!("Starting orchestrator for {}", working_dir);

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    let runtime = OrchestratorRuntime::new(backend, base_path, working_dir.to_string(), watcher_config);
    runtime.run().await
}

async fn send_message(to: AgentRole, content: &str) -> Result<()> {
    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    let to_id = AgentId::new_singleton(to);

    let message = AgentMessage::new(
        AgentId::new_singleton(AgentRole::Manager), // External input goes to manager
        to_id.clone(),
        MessageKind::Info,
        content.to_string(),
    );

    let mut conn = AgentConnection::connect(&to_id, &base_path).await?;
    conn.send(&message).await?;

    info!("Sent message to {}: {}", to_id, content);
    Ok(())
}

async fn show_status() -> Result<()> {
    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);

    println!("=== Agent Sockets ===");

    // Singletons
    for role in [AgentRole::Manager, AgentRole::Architect, AgentRole::Scorer] {
        let agent_id = AgentId::new_singleton(role);
        println!("  {}: {}", agent_id, probe_socket(&agent_id, &base_path).await);
    }

    // Developers (0-2)
    for i in 0..3u8 {
        let agent_id = AgentId::new_developer(i);
        let status = probe_socket(&agent_id, &base_path).await;
        if status != "not running" || i == 0 {
            println!("  {}: {}", agent_id, status);
        }
    }

    Ok(())
}

async fn probe_socket(agent_id: &AgentId, base_path: &Path) -> &'static str {
    let socket_path = base_path.join(format!("{}.sock", agent_id.socket_name()));
    if socket_path.exists() {
        match AgentConnection::connect(agent_id, base_path).await {
            Ok(_) => "listening",
            Err(_) => "stale socket",
        }
    } else {
        "not running"
    }
}
