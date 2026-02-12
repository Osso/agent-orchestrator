mod agent;
mod backend;
mod runtime;
mod transport;
mod types;

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use agent::{Agent, AgentConfig};
use backend::ClaudeBackend;
use runtime::{OrchestratorRuntime, RuntimeCommand};
use transport::{AgentConnection, AgentMessage, MessageKind};
use types::{AgentId, AgentRole};

const DEFAULT_SOCKET_PATH: &str = "/tmp/claude/orchestrator";

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("agent_orchestrator=info".parse()?),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

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
            run_task(working_dir, &task).await
        }
        "agent" => {
            if args.len() < 3 {
                bail!("Usage: agent-orchestrator agent <role> [working_dir]");
            }
            let role = parse_role(&args[2])?;
            let working_dir = args.get(3).map(|s| s.as_str()).unwrap_or(".");
            run_agent(role, working_dir).await
        }
        "orchestrate" => {
            let working_dir = args.get(2).map(|s| s.as_str()).unwrap_or(".");
            run_orchestrator(working_dir).await
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
    agent-orchestrator <COMMAND>

COMMANDS:
    run <dir> <task...>     Run agents on a task (non-interactive)

    orchestrate [dir]       Start agents and wait for messages

    send <role> <message>   Send a message to a running agent

    agent <role> [dir]      Run a single agent standalone

    status                  Show socket status

EXAMPLES:
    # Run a task non-interactively
    agent-orchestrator run ~/my-project "Add a login button to the homepage"

    # Start agents, then send tasks separately
    agent-orchestrator orchestrate ~/my-project
    agent-orchestrator send manager "Add a login button"
"#
    );
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

async fn run_agent(role: AgentRole, working_dir: &str) -> Result<()> {
    let agent_id = AgentId::new_singleton(role);
    info!("Starting agent {} in {}", agent_id, working_dir);

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    let backend = Arc::new(ClaudeBackend::new());

    // Standalone mode: commands are logged but not handled
    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<RuntimeCommand>(64);
    tokio::spawn(async move {
        while let Some(cmd) = command_rx.recv().await {
            tracing::info!("Standalone agent received runtime command: {:?}", cmd);
        }
    });

    let config = AgentConfig {
        agent_id,
        working_dir: working_dir.to_string(),
        system_prompt: role.system_prompt().to_string(),
    };

    let agent = Agent::new(config, backend, &base_path, command_tx).await?;
    agent.run().await
}

async fn run_task(working_dir: &str, task: &str) -> Result<()> {
    info!("Running task in {}: {}", working_dir, task);

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    let backend = Arc::new(ClaudeBackend::new());
    let runtime = OrchestratorRuntime::new(backend, base_path, working_dir.to_string());
    runtime.run_with_task(task.to_string()).await
}

async fn run_orchestrator(working_dir: &str) -> Result<()> {
    info!("Starting orchestrator for {}", working_dir);

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    let backend = Arc::new(ClaudeBackend::new());
    let runtime = OrchestratorRuntime::new(backend, base_path, working_dir.to_string());
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
