use agent_orchestrator::agent::BackendKind;
use agent_orchestrator::control;

use anyhow::{Result, bail};
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
        "status" => cmd_status(args),
        "scale" => cmd_scale(args),
        _ => {
            print_usage();
            Ok(())
        }
    }
}

fn init_tracing() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("agent_orchestrator=info".parse()?),
        )
        .init();
    Ok(())
}

async fn cmd_daemon() -> Result<()> {
    let backend = load_backend_config();
    agent_orchestrator::daemon::run(backend, false).await
}

fn load_backend_config() -> BackendKind {
    let path = backend_config_path();
    let Some(table) = read_backend_table(&path) else {
        return BackendKind::Claude;
    };
    parse_backend_kind(&table)
}

fn backend_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("agent-orchestrator")
        .join("config.toml")
}

fn read_backend_table(path: &std::path::Path) -> Option<toml::Table> {
    let contents = std::fs::read_to_string(path).ok()?;
    match contents.parse() {
        Ok(table) => Some(table),
        Err(error) => {
            tracing::warn!("Bad config {}: {error}", path.display());
            None
        }
    }
}

fn parse_backend_kind(table: &toml::Table) -> BackendKind {
    match table_str(table, "backend").unwrap_or("claude") {
        "codex" => codex_backend(table),
        "openrouter" => openrouter_backend(table),
        _ => BackendKind::Claude,
    }
}

fn codex_backend(table: &toml::Table) -> BackendKind {
    let model = table_str(table, "model").unwrap_or("gpt-5.4");
    BackendKind::Codex {
        model: model.to_string(),
    }
}

fn openrouter_backend(table: &toml::Table) -> BackendKind {
    let model = table_str(table, "model").unwrap_or("anthropic/claude-sonnet-4");
    let api_key = table_str(table, "api_key")
        .map(str::to_string)
        .or_else(|| std::env::var("OPENROUTER_API_KEY").ok())
        .unwrap_or_default();
    BackendKind::OpenRouter {
        model: model.to_string(),
        api_key,
    }
}

fn table_str<'a>(table: &'a toml::Table, key: &str) -> Option<&'a str> {
    table.get(key).and_then(|value| value.as_str())
}

fn cmd_send(args: &[String]) -> Result<()> {
    let project = extract_named_arg(args, "--project")
        .ok_or_else(|| anyhow::anyhow!("--project required for send"))?;
    // Remaining positional args after removing binary, subcommand, --project, and its value
    let positional: Vec<&String> = args
        .iter()
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
    let message: String = positional[1..]
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(" ");
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
    let explicit_project = extract_named_arg(args, "--project");
    let env_project = std::env::var("CLAUDE_CODE_TASK_LIST_ID").ok();
    let derived_project = project_from_cwd();
    let project = explicit_project
        .clone()
        .or_else(|| env_project.clone())
        .or_else(|| derived_project.clone())
        .ok_or_else(|| anyhow::anyhow!("--project or CLAUDE_CODE_TASK_LIST_ID required"))?;
    let db_path = db_path_for_project(&project);
    let should_register_cwd =
        explicit_project.is_none() && env_project.is_none() && derived_project.is_some();
    agent_orchestrator::mcp_tasks::run(&db_path, &project, should_register_cwd).await
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
    status --project <name>                     Show running agents for a project
    scale <max>                                  Set global max concurrent task agents (1-20)
    mcp-serve --agent <name> --socket <path>    Run MCP stdio server for an agent
    mcp-tasks [--project <name>]                Task DB MCP for Claude Code (uses CLAUDE_CODE_TASK_LIST_ID)

EXAMPLES:
    agent-orchestrator daemon
    agent-orchestrator send --project my-project runtime "check status"
    agent-orchestrator scale 5
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

/// Derive project name from the current working directory basename.
fn project_from_cwd() -> Option<String> {
    std::env::current_dir()
        .ok()?
        .file_name()
        .and_then(|n| n.to_str())
        .map(String::from)
}

fn extract_named_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn cmd_notify(args: &[String]) -> Result<()> {
    let project = extract_named_arg(args, "--project")
        .ok_or_else(|| anyhow::anyhow!("--project required for notify"))?;
    let task_id = args
        .iter()
        .find(|a| *a != "--project" && *a != &project && *a != "notify" && *a != &args[0])
        .ok_or_else(|| {
            anyhow::anyhow!("Usage: agent-orchestrator notify --project <name> <task-id>")
        })?;
    let socket = control::control_socket_path();
    let request = control::ControlRequest::NotifyTaskCreated {
        project,
        task_id: task_id.clone(),
    };
    let response: control::ControlResponse = peercred_ipc::Client::call(&socket, &request)?;
    match response {
        control::ControlResponse::Ok => println!("Notified runtime about {}", task_id),
        control::ControlResponse::Error { message } => bail!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

fn cmd_status(args: &[String]) -> Result<()> {
    let project = extract_named_arg(args, "--project")
        .ok_or_else(|| anyhow::anyhow!("--project required for status"))?;
    let socket = control::control_socket_path();
    let request = control::ControlRequest::Status { project };
    let response: control::ControlResponse = peercred_ipc::Client::call(&socket, &request)?;
    match response {
        control::ControlResponse::Status { agents, project } => {
            let out: Vec<serde_json::Value> = agents
                .iter()
                .map(|a| {
                    let task_id = a.name.strip_prefix("task-").map(String::from);
                    serde_json::json!({ "name": a.name, "role": a.role, "task_id": task_id })
                })
                .collect();
            println!(
                "{}",
                serde_json::json!({ "project": project, "agents": out })
            );
        }
        control::ControlResponse::Error { message } => bail!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

fn cmd_scale(args: &[String]) -> Result<()> {
    let max: u8 = args
        .get(2)
        .ok_or_else(|| anyhow::anyhow!("Usage: agent-orchestrator scale <max>"))?
        .parse()
        .map_err(|_| anyhow::anyhow!("max must be a number 1-20"))?;
    let max = max.clamp(1, 20);
    let socket = control::control_socket_path();
    let request = control::ControlRequest::SetConcurrency { max };
    let response: control::ControlResponse = peercred_ipc::Client::call(&socket, &request)?;
    match response {
        control::ControlResponse::Ok => println!("Global max concurrency set to {max}"),
        control::ControlResponse::Error { message } => bail!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

fn send_message(project: &str, to: &str, content: &str) -> Result<()> {
    use control::{ControlRequest, ControlResponse};
    let socket = control::control_socket_path();
    let request = ControlRequest::SendMessage {
        project: project.to_string(),
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
