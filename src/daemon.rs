use anyhow::{Context, Result};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::info;

use crate::agent::BackendKind;
use crate::config;
use crate::control::{self, ProjectRegistry};
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

    let registry = control::new_registry();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(control::run_control_server(
        registry.clone(),
        shutdown_tx.clone(),
        shutdown_rx,
    ));
    // Brief pause for control socket to bind
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let handles = spawn_runtimes(&projects, &registry, &shutdown_tx, backend, no_sandbox).await;
    wait_for_shutdown().await;
    shutdown_runtimes(handles).await;
    info!("Daemon stopped");
    Ok(())
}

async fn spawn_runtimes(
    projects: &std::collections::HashMap<String, config::ProjectConfig>,
    registry: &ProjectRegistry,
    shutdown_tx: &watch::Sender<bool>,
    backend: BackendKind,
    no_sandbox: bool,
) -> Vec<(String, JoinHandle<()>)> {
    let mut handles = Vec::new();
    for (name, cfg) in projects {
        match create_runtime(name, &cfg.dir, backend.clone(), no_sandbox).await {
            Ok(runtime) => {
                registry
                    .write()
                    .unwrap()
                    .insert(name.clone(), runtime.bus.clone());
                let handle = spawn_runtime(name.clone(), runtime, shutdown_tx.clone());
                handles.push((name.clone(), handle));
            }
            Err(e) => tracing::error!("Failed to create runtime '{}': {}", name, e),
        }
    }
    handles
}

async fn create_runtime(
    name: &str,
    working_dir: &str,
    backend: BackendKind,
    no_sandbox: bool,
) -> Result<OrchestratorRuntime> {
    let db_path = db_path_for_project(name);
    info!("Creating runtime for project '{}'", name);
    OrchestratorRuntime::new(&db_path, working_dir.to_string(), backend, no_sandbox).await
}

fn spawn_runtime(
    name: String,
    runtime: OrchestratorRuntime,
    shutdown_tx: watch::Sender<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        info!("Starting runtime for project '{}'", name);
        if let Err(e) = runtime.run_managed(shutdown_tx).await {
            tracing::error!("Runtime '{}' error: {}", name, e);
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
