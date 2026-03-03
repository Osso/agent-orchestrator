mod agent;
mod control;
mod mcp;
mod relay;
mod runtime;
mod types;

use anyhow::{bail, Result};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

use runtime::OrchestratorRuntime;

const DEFAULT_DB_PATH: &str = "/tmp/claude/orchestrator/tasks.db";

#[tokio::main]
async fn main() -> Result<()> {
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
    mcp::run_mcp_server(socket, agent).await
}

fn print_usage() {
    eprintln!(
        r#"Agent Orchestrator - Multi-agent coordination for AI coding assistants

USAGE:
    agent-orchestrator [OPTIONS] <COMMAND>

OPTIONS:
    --db <path>     Task database path (default: {DEFAULT_DB_PATH})

COMMANDS:
    run <dir> <task...>                         Run agents on a task (non-interactive)
    orchestrate [dir]                           Start agents and wait for messages
    send <to> <message>                         Send a message to a running agent
    mcp-serve --agent <name> [--socket <path>]  Run MCP stdio server for an agent

EXAMPLES:
    agent-orchestrator run ~/my-project "Add a login button"
    agent-orchestrator orchestrate ~/my-project
    agent-orchestrator send manager "Add a login button"
"#
    );
}

struct Opts {
    db_path: PathBuf,
}

fn extract_opts(args: &mut Vec<String>) -> Opts {
    let mut db_path = PathBuf::from(DEFAULT_DB_PATH);
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--db" && i + 1 < args.len() {
            db_path = PathBuf::from(&args[i + 1]);
            args.drain(i..i + 2);
        } else {
            i += 1;
        }
    }
    Opts { db_path }
}

fn extract_named_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

async fn run_orchestrator(working_dir: &str, task: Option<String>, opts: &Opts) -> Result<()> {
    info!("Starting orchestrator for {}", working_dir);
    let runtime = OrchestratorRuntime::new(&opts.db_path, working_dir.to_string()).await?;
    runtime.run(task).await
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
