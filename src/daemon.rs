use anyhow::{Context, Result};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tracing::info;

use crate::agent::BackendKind;
use crate::config;
use crate::runtime::OrchestratorRuntime;

pub async fn run(backend: BackendKind, no_sandbox: bool) -> Result<()> {
    let projects = config::load_config().context("Failed to load project config")?;
    if projects.is_empty() {
        anyhow::bail!(
            "No projects configured in {}",
            config::config_path().display()
        );
    }

    info!("Daemon starting with {} project(s)", projects.len());
    let handles = spawn_runtimes(&projects, backend, no_sandbox);
    wait_for_shutdown().await;
    shutdown_runtimes(handles).await;
    info!("Daemon stopped");
    Ok(())
}

fn spawn_runtimes(
    projects: &std::collections::HashMap<String, config::ProjectConfig>,
    backend: BackendKind,
    no_sandbox: bool,
) -> Vec<(String, JoinHandle<()>)> {
    projects
        .iter()
        .map(|(name, cfg)| {
            let handle = spawn_one_runtime(name.clone(), cfg.dir.clone(), backend.clone(), no_sandbox);
            (name.clone(), handle)
        })
        .collect()
}

fn spawn_one_runtime(
    name: String,
    working_dir: String,
    backend: BackendKind,
    no_sandbox: bool,
) -> JoinHandle<()> {
    let db_path = db_path_for_project(&name);
    tokio::spawn(async move {
        info!("Starting runtime for project '{}'", name);
        match OrchestratorRuntime::new(&db_path, working_dir, backend, no_sandbox).await {
            Ok(runtime) => {
                if let Err(e) = runtime.run(None).await {
                    tracing::error!("Runtime '{}' error: {}", name, e);
                }
            }
            Err(e) => tracing::error!("Failed to start runtime '{}': {}", name, e),
        }
    })
}

async fn wait_for_shutdown() {
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
    tokio::select! {
        _ = sigint.recv() => info!("Received SIGINT"),
        _ = sigterm.recv() => info!("Received SIGTERM"),
    }
}

async fn shutdown_runtimes(handles: Vec<(String, JoinHandle<()>)>) {
    info!("Daemon shutting down, aborting {} runtime(s)", handles.len());
    for (name, handle) in &handles {
        info!("Stopping runtime '{}'", name);
        handle.abort();
    }
    for (name, handle) in handles {
        match handle.await {
            Ok(()) => info!("Runtime '{}' stopped", name),
            Err(e) if e.is_cancelled() => info!("Runtime '{}' cancelled", name),
            Err(e) => tracing::error!("Runtime '{}' join error: {}", name, e),
        }
    }
}

fn db_path_for_project(project: &str) -> std::path::PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("agent-orchestrator").join(project).join("tasks.db")
}
