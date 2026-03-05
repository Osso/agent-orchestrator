use agent_orchestrator::agent::BackendKind;
use agent_orchestrator::control;

use anyhow::{bail, Result};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

const APP_NAME: &str = "agent-orchestrator";

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing()?;
    let args: Vec<String> = std::env::args().collect();
    dispatch(&args).await
}

async fn dispatch(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }
    match args[1].as_str() {
        "daemon" => cmd_daemon().await,
        "send" => cmd_send(args),
        "notify" => cmd_notify(args),
        "mcp-serve" => cmd_mcp_serve(args).await,
        "mcp-tasks" => cmd_mcp_tasks(args).await,
        _ => {
            print_usage();
            Ok(())
        }
    }
}

fn init_tracing() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("agent_orchestrator=info".parse()?),
        )
        .init();
    Ok(())
}

async fn cmd_daemon() -> Result<()> {
    let backend = BackendKind::Claude;
    agent_orchestrator::daemon::run(backend, false).await
}

fn cmd_send(args: &[String]) -> Result<()> {
    let project = extract_named_arg(args, "--project")
        .ok_or_else(|| anyhow::anyhow!("--project required for send"))?;
    // Remaining positional args after removing binary, subcommand, --project, and its value
    let positional: Vec<&String> = args.iter()
        .enumerate()
        .filter(|(i, a)| {
            *i > 1 && *a != "--project" && {
                // Skip value after --project
                *i == 0 || args.get(i.wrapping_sub(1)).map(|p| p.as_str()) != Some("--project")
            }
        })
        .map(|(_, a)| a)
        .collect();
    if positional.len() < 2 {
        bail!("Usage: agent-orchestrator send --project <name> <to> <message>");
    }
    let to = positional[0];
    let message: String = positional[1..].iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ");
    send_message(&project, to, &message)
}

async fn cmd_mcp_serve(args: &[String]) -> Result<()> {
    let socket = extract_named_arg(args, "--socket")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("--socket required for mcp-serve"))?;
    let agent = extract_named_arg(args, "--agent")
        .ok_or_else(|| anyhow::anyhow!("--agent required for mcp-serve"))?;
    agent_orchestrator::mcp::run_mcp_server(socket, agent).await
}

async fn cmd_mcp_tasks(args: &[String]) -> Result<()> {
    let project = extract_named_arg(args, "--project")
        .or_else(|| std::env::var("CLAUDE_CODE_TASK_LIST_ID").ok())
        .ok_or_else(|| anyhow::anyhow!("--project or CLAUDE_CODE_TASK_LIST_ID required"))?;
    let db_path = db_path_for_project(&project);
    agent_orchestrator::mcp_tasks::run(&db_path, &project).await
}

fn print_usage() {
    eprintln!(
        r#"Agent Orchestrator - Multi-agent coordination for AI coding assistants

USAGE:
    agent-orchestrator <COMMAND>

COMMANDS:
    daemon                                      Run as daemon for all configured projects
    send --project <name> <to> <message>        Send a message to a running agent
    notify --project <name> <task-id>           Notify runtime about a new task
    mcp-serve --agent <name> --socket <path>    Run MCP stdio server for an agent
    mcp-tasks [--project <name>]                Task DB MCP for Claude Code (uses CLAUDE_CODE_TASK_LIST_ID)

EXAMPLES:
    agent-orchestrator daemon
    agent-orchestrator send --project my-project manager "Add a login button"
"#
    );
}

/// Derive a project-specific DB path from the working directory.
/// Uses ~/.local/share/agent-orchestrator/{project}/tasks.db (same base as session store).
fn db_path_for_project(working_dir: &str) -> PathBuf {
    let project = std::path::Path::new(working_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(APP_NAME).join(project).join("tasks.db")
}

fn extract_named_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn cmd_notify(args: &[String]) -> Result<()> {
    let project = extract_named_arg(args, "--project")
        .ok_or_else(|| anyhow::anyhow!("--project required for notify"))?;
    let task_id = args.iter()
        .find(|a| *a != "--project" && *a != &project && *a != "notify" && *a != &args[0])
        .ok_or_else(|| anyhow::anyhow!("Usage: agent-orchestrator notify --project <name> <task-id>"))?;
    let socket = control::control_socket_path(&project);
    let request = control::ControlRequest::NotifyTaskCreated { task_id: task_id.clone() };
    let response: control::ControlResponse = peercred_ipc::Client::call(&socket, &request)?;
    match response {
        control::ControlResponse::Ok => println!("Notified runtime about {}", task_id),
        control::ControlResponse::Error { message } => bail!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

fn send_message(project: &str, to: &str, content: &str) -> Result<()> {
    use control::{ControlRequest, ControlResponse};
    let socket = control::control_socket_path(project);
    let request = ControlRequest::SendMessage {
        to: to.to_string(),
        content: content.to_string(),
    };
    let response: ControlResponse = peercred_ipc::Client::call(&socket, &request)?;
    match response {
        ControlResponse::Ok => println!("Sent to {to}"),
        ControlResponse::Error { message } => bail!("Error: {message}"),
        _ => {}
    }
    Ok(())
}
