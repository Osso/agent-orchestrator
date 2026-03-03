mod agent;
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("agent_orchestrator=info".parse()?),
        )
        .init();

    let mut args: Vec<String> = std::env::args().collect();
    let opts = extract_opts(&mut args);

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
            run_orchestrator(working_dir, Some(task), &opts).await
        }
        "orchestrate" => {
            let working_dir = args.get(2).map(|s| s.as_str()).unwrap_or(".");
            run_orchestrator(working_dir, None, &opts).await
        }
        "send" => {
            if args.len() < 4 {
                bail!("Usage: agent-orchestrator send <to> <message>");
            }
            let to = &args[2];
            let message = args[3..].join(" ");
            send_message(to, &message)
        }
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
    --db <path>     Task database path (default: {DEFAULT_DB_PATH})

COMMANDS:
    run <dir> <task...>     Run agents on a task (non-interactive)
    orchestrate [dir]       Start agents and wait for messages
    send <to> <message>     Send a message to a running agent

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

async fn run_orchestrator(working_dir: &str, task: Option<String>, opts: &Opts) -> Result<()> {
    info!("Starting orchestrator for {}", working_dir);

    let runtime =
        OrchestratorRuntime::new(&opts.db_path, working_dir.to_string()).await?;
    runtime.run(task).await
}

fn send_message(to: &str, content: &str) -> Result<()> {
    // In-process bus requires a running orchestrator.
    // For external message injection, the orchestrator would need
    // to expose a separate IPC endpoint (future work).
    bail!(
        "Cannot send to '{}': the bus is in-process. \
         Use 'run' or 'orchestrate' to start agents with a task.\n\
         Message was: {}",
        to,
        content
    );
}
