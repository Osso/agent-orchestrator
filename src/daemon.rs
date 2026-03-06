use std::collections::HashMap;
use std::time::Instant;

use anyhow::{Context, Result};
use llm_tasks::db::Database;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::info;

use crate::agent::BackendKind;
use crate::config::{self, ProjectConfig};
use crate::control::{self, ProjectRegistry};
use crate::runtime::OrchestratorRuntime;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const IDLE_TEARDOWN: std::time::Duration = std::time::Duration::from_secs(300);

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
    let (global_shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(control::run_control_server(
        registry.clone(),
        global_shutdown_tx.clone(),
        shutdown_rx,
    ));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut supervisor = Supervisor::new(projects, registry, backend, no_sandbox);
    supervisor.run(global_shutdown_tx).await;
    info!("Daemon stopped");
    Ok(())
}

struct ProjectState {
    config: ProjectConfig,
    handle: Option<JoinHandle<()>>,
    shutdown_tx: Option<watch::Sender<bool>>,
    idle_since: Option<Instant>,
}

struct Supervisor {
    projects: HashMap<String, ProjectState>,
    registry: ProjectRegistry,
    backend: BackendKind,
    no_sandbox: bool,
}

impl Supervisor {
    fn new(
        configs: HashMap<String, ProjectConfig>,
        registry: ProjectRegistry,
        backend: BackendKind,
        no_sandbox: bool,
    ) -> Self {
        let projects = configs
            .into_iter()
            .map(|(name, config)| {
                let state = ProjectState {
                    config,
                    handle: None,
                    shutdown_tx: None,
                    idle_since: None,
                };
                (name, state)
            })
            .collect();
        Self { projects, registry, backend, no_sandbox }
    }

    async fn run(&mut self, global_shutdown_tx: watch::Sender<bool>) {
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT");
        let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM");
        let mut poll = tokio::time::interval(POLL_INTERVAL);

        loop {
            tokio::select! {
                _ = poll.tick() => self.poll_projects().await,
                _ = sigint.recv() => { info!("Received SIGINT"); break; }
                _ = sigterm.recv() => { info!("Received SIGTERM"); break; }
            }
        }

        let _ = global_shutdown_tx.send(true);
        self.shutdown_all().await;
    }

    async fn poll_projects(&mut self) {
        let names: Vec<String> = self.projects.keys().cloned().collect();
        for name in names {
            self.poll_project(&name).await;
        }
    }

    async fn poll_project(&mut self, name: &str) {
        let db_path = db_path_for_project(name);
        let has_active = has_active_tasks(&db_path).await;
        let state = self.projects.get_mut(name).unwrap();
        let is_running = is_runtime_alive(state);

        match (has_active, is_running) {
            (true, false) => self.activate_project(name).await,
            (false, true) => self.maybe_deactivate(name),
            (true, true) => { state.idle_since = None; }
            (false, false) => {}
        }
    }

    async fn activate_project(&mut self, name: &str) {
        let state = self.projects.get_mut(name).unwrap();
        state.idle_since = None;
        info!("Activating project '{}' (has actionable tasks)", name);

        let db_path = db_path_for_project(name);
        let runtime = match OrchestratorRuntime::new(
            &db_path,
            state.config.dir.clone(),
            self.backend.clone(),
            self.no_sandbox,
        ).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("Failed to create runtime '{}': {}", name, e);
                return;
            }
        };

        self.registry
            .write()
            .unwrap()
            .insert(name.to_string(), runtime.bus.clone());

        let (shutdown_tx, _) = watch::channel(false);
        let tx = shutdown_tx.clone();
        let project_name = name.to_string();
        let handle = tokio::spawn(async move {
            info!("Starting runtime for project '{}'", project_name);
            if let Err(e) = runtime.run_managed(tx).await {
                tracing::error!("Runtime '{}' error: {}", project_name, e);
            }
            info!("Runtime '{}' exited", project_name);
        });

        let state = self.projects.get_mut(name).unwrap();
        state.handle = Some(handle);
        state.shutdown_tx = Some(shutdown_tx);
    }

    fn maybe_deactivate(&mut self, name: &str) {
        let state = self.projects.get_mut(name).unwrap();
        let idle_since = *state.idle_since.get_or_insert_with(Instant::now);
        if idle_since.elapsed() < IDLE_TEARDOWN {
            return;
        }
        info!(
            "Deactivating project '{}' (idle {}s, no active tasks)",
            name, idle_since.elapsed().as_secs()
        );
        self.stop_project(name);
    }

    fn stop_project(&mut self, name: &str) {
        let state = self.projects.get_mut(name).unwrap();
        if let Some(handle) = state.handle.take() {
            handle.abort();
        }
        state.shutdown_tx = None;
        state.idle_since = None;
        self.registry.write().unwrap().remove(name);
    }

    async fn shutdown_all(&mut self) {
        let names: Vec<String> = self.projects.keys().cloned().collect();
        info!("Shutting down {} project(s)", names.len());
        for name in &names {
            self.stop_project(name);
        }
    }
}

fn is_runtime_alive(state: &ProjectState) -> bool {
    state.handle.as_ref().is_some_and(|h| !h.is_finished())
}

async fn has_active_tasks(db_path: &std::path::Path) -> bool {
    if !db_path.exists() {
        return false;
    }
    let db = match Database::open(db_path).await {
        Ok(db) => db,
        Err(_) => return false,
    };
    matches!(
        db.list_tasks(None, None).await,
        Ok(tasks) if tasks.iter().any(|t|
            !matches!(t.status.as_str(), "completed" | "done" | "closed")
        )
    )
}

pub fn db_path_for_project(project: &str) -> std::path::PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
    base.join("agent-orchestrator").join(project).join("tasks.db")
}
