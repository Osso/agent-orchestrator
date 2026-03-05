use agent_orchestrator::agent::BackendKind;
use agent_orchestrator::control;
use agent_orchestrator::relay;
use agent_orchestrator::runtime::OrchestratorRuntime;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

const APP_NAME: &str = "agent-orchestrator";

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    init_tracing()?;
    let mut args: Vec<String> = std::env::args().collect();
    let opts = extract_opts(&mut args);
    dispatch(&args, &opts).await
}

async fn dispatch(args: &[String], opts: &Opts) -> Result<()> {
    if args.len() < 2 {
        print_usage();
        return Ok(());
    }
    match args[1].as_str() {
        "run" => cmd_run(args, opts).await,
        "orchestrate" => cmd_orchestrate(args, opts).await,
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

async fn cmd_run(args: &[String], opts: &Opts) -> Result<()> {
    if args.len() < 4 {
        bail!("Usage: agent-orchestrator run <dir> <task...>");
    }
    let working_dir = &args[2];
    let task = args[3..].join(" ");

    // Clean sessions so agents start fresh, but keep the task DB for history
    clean_sessions(working_dir);

    run_orchestrator(working_dir, Some(task), opts).await
}

async fn cmd_orchestrate(args: &[String], opts: &Opts) -> Result<()> {
    let working_dir = args.get(2).map(|s| s.as_str()).unwrap_or(".");
    run_orchestrator(working_dir, None, opts).await
}

fn cmd_send(args: &[String]) -> Result<()> {
    if args.len() < 4 {
        bail!("Usage: agent-orchestrator send <to> <message>");
    }
    let to = &args[2];
    let message = args[3..].join(" ");
    send_message(to, &message)
}

async fn cmd_mcp_serve(args: &[String]) -> Result<()> {
    let socket = extract_named_arg(args, "--socket")
        .map(PathBuf::from)
        .unwrap_or_else(relay::relay_socket_path);
    let agent = extract_named_arg(args, "--agent")
        .ok_or_else(|| anyhow::anyhow!("--agent required for mcp-serve"))?;
    agent_orchestrator::mcp::run_mcp_server(socket, agent).await
}

async fn cmd_mcp_tasks(args: &[String]) -> Result<()> {
    let project = extract_named_arg(args, "--project")
        .or_else(|| std::env::var("CLAUDE_CODE_TASK_LIST_ID").ok())
        .ok_or_else(|| anyhow::anyhow!("--project or CLAUDE_CODE_TASK_LIST_ID required"))?;
    let db_path = db_path_for_project(&project);
    agent_orchestrator::mcp_tasks::run(&db_path).await
}

fn print_usage() {
    eprintln!(
        r#"Agent Orchestrator - Multi-agent coordination for AI coding assistants

USAGE:
    agent-orchestrator [OPTIONS] <COMMAND>

OPTIONS:
    --db <path>         Task database path (default: ~/.local/share/agent-orchestrator/<project>/tasks.db)
    --backend <name>    Backend to use: claude (default) or openrouter
    --model <name>      Model name (required for --backend openrouter)
    --no-sandbox        Disable bwrap sandboxing

COMMANDS:
    run <dir> <task...>                         Run agents on a task (non-interactive)
    orchestrate [dir]                           Start agents and wait for messages
    send <to> <message>                         Send a message to a running agent
    mcp-serve --agent <name> [--socket <path>]  Run MCP stdio server for an agent
    mcp-tasks [--project <name>]                Task DB MCP for Claude Code (uses CLAUDE_CODE_TASK_LIST_ID)

EXAMPLES:
    agent-orchestrator run ~/my-project "Add a login button"
    agent-orchestrator orchestrate ~/my-project
    agent-orchestrator --backend openrouter --model anthropic/claude-3.5-sonnet run ~/my-project "task"
    agent-orchestrator send manager "Add a login button"
"#
    );
}

struct Opts {
    db_path: Option<PathBuf>,
    backend: String,
    model: Option<String>,
    no_sandbox: bool,
}

fn extract_opts(args: &mut Vec<String>) -> Opts {
    let mut db_path: Option<PathBuf> = None;
    let mut backend = "claude".to_string();
    let mut model: Option<String> = None;
    let mut no_sandbox = false;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" && i + 1 < args.len() {
            db_path = Some(PathBuf::from(&args[i + 1]));
            args.drain(i..i + 2);
        } else if args[i] == "--backend" && i + 1 < args.len() {
            backend = args[i + 1].clone();
            args.drain(i..i + 2);
        } else if args[i] == "--model" && i + 1 < args.len() {
            model = Some(args[i + 1].clone());
            args.drain(i..i + 2);
        } else if args[i] == "--no-sandbox" {
            no_sandbox = true;
            args.drain(i..i + 1);
        } else {
            i += 1;
        }
    }
    Opts { db_path, backend, model, no_sandbox }
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

fn build_backend(opts: &Opts) -> Result<BackendKind> {
    match opts.backend.as_str() {
        "openrouter" => {
            let api_key = std::env::var("OPENROUTER_API_KEY")
                .context("OPENROUTER_API_KEY required for openrouter backend")?;
            let model = opts
                .model
                .as_deref()
                .context("--model required for openrouter backend")?;
            Ok(BackendKind::OpenRouter {
                model: model.to_string(),
                api_key,
            })
        }
        _ => Ok(BackendKind::Claude),
    }
}

async fn run_orchestrator(working_dir: &str, task: Option<String>, opts: &Opts) -> Result<()> {
    info!("Starting orchestrator for {}", working_dir);
    let db_path = opts
        .db_path
        .clone()
        .unwrap_or_else(|| db_path_for_project(working_dir));
    let backend = build_backend(opts)?;
    let runtime =
        OrchestratorRuntime::new(&db_path, working_dir.to_string(), backend, opts.no_sandbox)
            .await?;
    runtime.run(task).await
}

/// Remove session data for a project so agents start fresh.
/// Removes sessions.json and message_logs/ but preserves the task DB.
fn clean_sessions(working_dir: &str) {
    let project = std::path::Path::new(working_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
    let project_dir = base.join(APP_NAME).join(project);
    let _ = std::fs::remove_file(project_dir.join("sessions.json"));
    let _ = std::fs::remove_dir_all(project_dir.join("message_logs"));
}

fn cmd_notify(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        bail!("Usage: agent-orchestrator notify <task-id>");
    }
    let socket = control::control_socket_path();
    let request = control::ControlRequest::NotifyTaskCreated { task_id: args[2].clone() };
    let response: control::ControlResponse = peercred_ipc::Client::call(&socket, &request)?;
    match response {
        control::ControlResponse::Ok => println!("Notified runtime about {}", args[2]),
        control::ControlResponse::Error { message } => bail!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

fn send_message(to: &str, content: &str) -> Result<()> {
    use control::{ControlRequest, ControlResponse};
    let socket = control::control_socket_path();
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
