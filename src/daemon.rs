use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::agent::BackendKind;
use crate::config::{self, ProjectConfig};
use crate::control::{self, ProjectRegistry};
use crate::runtime::{GlobalLimits, OrchestratorRuntime};

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
    let global_limits = Arc::new(GlobalLimits::new(10));
    let (global_shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(control::run_control_server(
        registry.clone(),
        global_limits.clone(),
        global_shutdown_tx.clone(),
        shutdown_rx,
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut supervisor = Supervisor::new(registry, global_limits, backend, no_sandbox);
    supervisor.start_all(projects).await;
    supervisor.wait_for_signal(global_shutdown_tx).await;
    info!("Daemon stopped");
    Ok(())
}

struct ProjectHandle {
    handle: JoinHandle<()>,
    shutdown_tx: watch::Sender<bool>,
}

struct Supervisor {
    projects: HashMap<String, ProjectHandle>,
    registry: ProjectRegistry,
    global_limits: Arc<GlobalLimits>,
    backend: BackendKind,
    no_sandbox: bool,
}

impl Supervisor {
    fn new(
        registry: ProjectRegistry,
        global_limits: Arc<GlobalLimits>,
        backend: BackendKind,
        no_sandbox: bool,
    ) -> Self {
        Self {
            projects: HashMap::new(),
            registry,
            global_limits,
            backend,
            no_sandbox,
        }
    }

    async fn start_all(&mut self, configs: HashMap<String, ProjectConfig>) {
        for (name, config) in configs {
            self.start_project(name, config).await;
        }
    }

    async fn sync_projects_from_disk(&mut self) {
        let configs = match config::load_config() {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("Failed to reload project config: {}", e);
                return;
            }
        };

        for (name, cfg) in configs {
            if self.projects.contains_key(&name) {
                continue;
            }
            info!("Discovered new project '{}', starting runtime", name);
            self.start_project(name, cfg).await;
        }
    }

    async fn start_project(&mut self, name: String, config: ProjectConfig) {
        let db_path = db_path_for_project(&name);
        let runtime = match OrchestratorRuntime::new(
            &db_path,
            config.dir.clone(),
            self.backend.clone(),
            self.no_sandbox,
            self.global_limits.clone(),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to create runtime '{}': {}", name, e);
                return;
            }
        };

        self.registry
            .write()
            .unwrap()
            .insert(name.clone(), runtime.bus.clone());

        let (shutdown_tx, _) = watch::channel(false);
        let tx = shutdown_tx.clone();
        let project_name = name.clone();
        let handle = tokio::spawn(async move {
            info!("Starting runtime for project '{}'", project_name);
            if let Err(e) = runtime.run_managed(tx).await {
                tracing::error!("Runtime '{}' error: {}", project_name, e);
            }
            info!("Runtime '{}' exited", project_name);
        });

        self.projects.insert(
            name,
            ProjectHandle {
                handle,
                shutdown_tx,
            },
        );
    }

    async fn wait_for_signal(&mut self, global_shutdown_tx: watch::Sender<bool>) {
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT");
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        let mut reload = tokio::time::interval(Duration::from_secs(2));

        tokio::select! {
            _ = async {
                loop {
                    reload.tick().await;
                    self.sync_projects_from_disk().await;
                }
            } => unreachable!(),
            _ = sigint.recv() => info!("Received SIGINT"),
            _ = sigterm.recv() => info!("Received SIGTERM"),
        }

        let _ = global_shutdown_tx.send(true);
        self.shutdown_all().await;
    }

    async fn shutdown_all(&mut self) {
        let names: Vec<String> = self.projects.keys().cloned().collect();
        info!("Shutting down {} project(s)", names.len());

        for name in &names {
            if let Some(ph) = self.projects.get(name) {
                let _ = ph.shutdown_tx.send(true);
            }
            self.registry.write().unwrap().remove(name);
        }

        for name in &names {
            if let Some(ph) = self.projects.remove(name) {
                match tokio::time::timeout(Duration::from_secs(5), ph.handle).await {
                    Ok(_) => info!("Project '{}' shut down cleanly", name),
                    Err(_) => warn!("Project '{}' shutdown timed out, aborting", name),
                }
            }
        }
    }
}

pub fn db_path_for_project(project: &str) -> std::path::PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("agent-orchestrator")
        .join(project)
        .join("tasks.db")
}
