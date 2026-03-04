use agent_orchestrator::agent::BackendKind;
use agent_orchestrator::relay;
use agent_orchestrator::runtime::OrchestratorRuntime;

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

const DEFAULT_DB_DIR: &str = "/tmp/claude/orchestrator";

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
        "mcp-serve" => cmd_mcp_serve(args).await,
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

    // Fresh start for each run — clean DB and sessions from previous goals
    let db_path = opts
        .db_path
        .clone()
        .unwrap_or_else(|| db_path_for_project(working_dir));
    let _ = std::fs::remove_file(&db_path);
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

fn print_usage() {
    eprintln!(
        r#"Agent Orchestrator - Multi-agent coordination for AI coding assistants

USAGE:
    agent-orchestrator [OPTIONS] <COMMAND>

OPTIONS:
    --db <path>         Task database path (default: per-project under {DEFAULT_DB_DIR}/)
    --backend <name>    Backend to use: claude (default) or openrouter
    --model <name>      Model name (required for --backend openrouter)
    --no-sandbox        Disable bwrap sandboxing

COMMANDS:
    run <dir> <task...>                         Run agents on a task (non-interactive)
    orchestrate [dir]                           Start agents and wait for messages
    send <to> <message>                         Send a message to a running agent
    mcp-serve --agent <name> [--socket <path>]  Run MCP stdio server for an agent

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
fn db_path_for_project(working_dir: &str) -> PathBuf {
    let project = std::path::Path::new(working_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    PathBuf::from(DEFAULT_DB_DIR).join(project).join("tasks.db")
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

/// Remove session store for a project so agents start fresh.
fn clean_sessions(working_dir: &str) {
    let project = std::path::Path::new(working_dir)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    // Match llm-sdk's SessionStore::new which uses dirs::data_dir()
    let base = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/share"))
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    let session_dir = base.join("agent-orchestrator").join(project);
    if session_dir.exists() {
        let _ = std::fs::remove_dir_all(&session_dir);
    }
}

fn send_message(to: &str, content: &str) -> Result<()> {
    bail!(
        "Cannot send to '{}': the bus is in-process. \
         Use 'run' or 'orchestrate' to start agents with a task.\n\
         Message was: {}",
        to,
        content
    );
}
