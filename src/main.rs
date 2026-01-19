mod agent;
mod backend;
mod transport;
mod types;

use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::EnvFilter;

use agent::{Agent, AgentConfig};
use backend::ClaudeBackend;
use transport::{AgentConnection, AgentMessage, MessageKind};
use types::AgentRole;

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
    agent <role> [dir]      Run a single agent
                            Roles: manager, architect, developer, scorer

    orchestrate [dir]       Run all four agents

    send <role> <message>   Send a message to an agent

    status                  Show socket status

EXAMPLES:
    # Run developer agent in current directory
    agent-orchestrator agent developer .

    # Run full orchestration
    agent-orchestrator orchestrate ~/my-project

    # Send a task to the manager
    agent-orchestrator send manager "Add a login button to the homepage"
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
    info!("Starting agent {} in {}", role, working_dir);

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    let backend = Arc::new(ClaudeBackend::new());

    let config = AgentConfig {
        role,
        working_dir: working_dir.to_string(),
        system_prompt: role.system_prompt().to_string(),
    };

    let agent = Agent::new(config, backend, &base_path).await?;
    agent.run().await
}

async fn run_orchestrator(working_dir: &str) -> Result<()> {
    info!("Starting orchestrator for {}", working_dir);

    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);
    std::fs::create_dir_all(&base_path)?;

    let backend = Arc::new(ClaudeBackend::new());

    // Spawn all agents
    let mut handles = Vec::new();

    for role in [
        AgentRole::Manager,
        AgentRole::Architect,
        AgentRole::Developer,
        AgentRole::Scorer,
    ] {
        let backend = backend.clone();
        let base_path = base_path.clone();
        let working_dir = working_dir.to_string();

        let handle = tokio::spawn(async move {
            let config = AgentConfig {
                role,
                working_dir,
                system_prompt: role.system_prompt().to_string(),
            };

            match Agent::new(config, backend, &base_path).await {
                Ok(agent) => {
                    if let Err(e) = agent.run().await {
                        tracing::error!("Agent {} error: {}", role, e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to create agent {}: {}", role, e);
                }
            }
        });

        handles.push(handle);
    }

    // Wait for all agents (they run forever unless interrupted)
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

async fn send_message(to: AgentRole, content: &str) -> Result<()> {
    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);

    let message = AgentMessage::new(
        AgentRole::Manager, // External input goes to manager
        to,
        MessageKind::Info,
        content.to_string(),
    );

    let mut conn = AgentConnection::connect(to, &base_path).await?;
    conn.send(&message).await?;

    info!("Sent message to {}: {}", to, content);
    Ok(())
}

async fn show_status() -> Result<()> {
    let base_path = PathBuf::from(DEFAULT_SOCKET_PATH);

    println!("=== Agent Sockets ===");
    for role in [
        AgentRole::Manager,
        AgentRole::Architect,
        AgentRole::Developer,
        AgentRole::Scorer,
    ] {
        let socket_path = base_path.join(format!("{}.sock", role.as_str()));
        let status = if socket_path.exists() {
            // Try to connect
            match AgentConnection::connect(role, &base_path).await {
                Ok(_) => "listening",
                Err(_) => "stale socket",
            }
        } else {
            "not running"
        };
        println!("  {}: {}", role, status);
    }

    Ok(())
}
