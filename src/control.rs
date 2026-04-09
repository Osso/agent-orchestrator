use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, RwLock};

use agent_bus::Bus;
use peercred_ipc::{CallerInfo, Connection, IpcError, Server};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::runtime::GlobalLimits;

/// Returns the path for the global control Unix socket.
/// Uses ~/.claude/orchestrator/ so it's accessible inside bwrap sandboxes.
pub fn control_socket_path() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    home.join(".claude/orchestrator/control.sock")
}

/// Thread-safe registry of project name → Bus.
pub type ProjectRegistry = Arc<RwLock<HashMap<String, Bus>>>;

pub fn new_registry() -> ProjectRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ControlRequest {
    SendMessage {
        project: String,
        to: String,
        content: String,
    },
    NotifyTaskCreated {
        project: String,
        task_id: String,
    },
    SetConcurrency {
        max: u8,
    },
    Abort {
        project: String,
    },
    Status {
        project: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ControlResponse {
    Ok,
    Error {
        message: String,
    },
    Status {
        agents: Vec<AgentStatus>,
        project: String,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentStatus {
    pub name: String,
    pub role: String,
}

pub async fn run_control_server(
    registry: ProjectRegistry,
    global_limits: Arc<GlobalLimits>,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let socket_path = control_socket_path();
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_file(&socket_path);
    let server = match Server::bind(&socket_path) {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to bind control socket: {e}");
            return;
        }
    };
    info!("Control socket listening on {}", socket_path.display());

    loop {
        tokio::select! {
            result = server.accept() => {
                handle_accept_result(result, &registry, &global_limits, &shutdown_tx);
            }
            _ = shutdown_rx.changed() => {
                info!("Control server shutting down");
                break;
            }
        }
    }
}

fn handle_accept_result(
    result: Result<(Connection, CallerInfo), IpcError>,
    registry: &ProjectRegistry,
    global_limits: &Arc<GlobalLimits>,
    shutdown_tx: &watch::Sender<bool>,
) {
    let (conn, caller) = match result {
        Ok(connection) => connection,
        Err(error) => {
            warn!("Control accept error: {error}");
            return;
        }
    };
    info!(
        "Control connection from pid={} uid={}",
        caller.pid, caller.uid
    );
    spawn_control_connection(conn, registry, global_limits, shutdown_tx);
}

fn spawn_control_connection(
    mut conn: Connection,
    registry: &ProjectRegistry,
    global_limits: &Arc<GlobalLimits>,
    shutdown_tx: &watch::Sender<bool>,
) {
    let registry = registry.clone();
    let global_limits = global_limits.clone();
    let shutdown_tx = shutdown_tx.clone();
    tokio::spawn(async move {
        if let Err(error) =
            handle_connection(&mut conn, &registry, &global_limits, &shutdown_tx).await
        {
            warn!("Control connection error: {error}");
        }
    });
}

async fn handle_connection(
    conn: &mut peercred_ipc::Connection,
    registry: &ProjectRegistry,
    global_limits: &Arc<GlobalLimits>,
    shutdown_tx: &watch::Sender<bool>,
) -> anyhow::Result<()> {
    let request: ControlRequest = conn.read().await?;
    let response = handle_request(request, registry, global_limits, shutdown_tx);
    conn.write(&response).await?;
    Ok(())
}

fn handle_request(
    request: ControlRequest,
    registry: &ProjectRegistry,
    global_limits: &Arc<GlobalLimits>,
    shutdown_tx: &watch::Sender<bool>,
) -> ControlResponse {
    match request {
        ControlRequest::SendMessage {
            project,
            to,
            content,
        } => with_bus(registry, &project, |bus| send_to_agent(bus, &to, &content)),
        ControlRequest::SetConcurrency { max } => set_concurrency(global_limits, max),
        ControlRequest::NotifyTaskCreated { project, task_id } => {
            notify_task_created(registry, &project, task_id)
        }
        ControlRequest::Abort { project } => with_bus(registry, &project, |_bus| {
            let _ = shutdown_tx.send(true);
            ControlResponse::Ok
        }),
        ControlRequest::Status { project } => status_response(registry, project),
    }
}

fn set_concurrency(global_limits: &Arc<GlobalLimits>, max: u8) -> ControlResponse {
    let max = (max as usize).clamp(1, 20);
    let prev = global_limits.max_concurrent.swap(max, Ordering::Relaxed);
    info!("Global max concurrency set to {} (was {})", max, prev);
    ControlResponse::Ok
}

fn notify_task_created(
    registry: &ProjectRegistry,
    project: &str,
    task_id: String,
) -> ControlResponse {
    with_bus(registry, project, |bus| {
        let payload = serde_json::json!({ "task_id": task_id });
        send_bus_message(bus, "runtime", "task_created", payload)
    })
}

fn status_response(registry: &ProjectRegistry, project: String) -> ControlResponse {
    let agents = project_agents(registry, &project);
    ControlResponse::Status { agents, project }
}

fn project_agents(registry: &ProjectRegistry, project: &str) -> Vec<AgentStatus> {
    let guard = registry.read().unwrap();
    let Some(bus) = guard.get(project) else {
        return Vec::new();
    };
    bus.list_registered()
        .into_iter()
        .filter(|name| name != "runtime" && !name.starts_with("relay-"))
        .map(agent_status)
        .collect()
}

fn agent_status(name: String) -> AgentStatus {
    let role = role_from_name(&name).to_string();
    AgentStatus { name, role }
}

fn with_bus(
    registry: &ProjectRegistry,
    project: &str,
    f: impl FnOnce(&Bus) -> ControlResponse,
) -> ControlResponse {
    let guard = registry.read().unwrap();
    match guard.get(project) {
        Some(bus) => f(bus),
        None => ControlResponse::Error {
            message: format!("unknown project: {project}"),
        },
    }
}

fn send_to_agent(bus: &Bus, to: &str, content: &str) -> ControlResponse {
    send_bus_message(
        bus,
        to,
        "external_message",
        serde_json::json!({ "content": content }),
    )
}

fn send_bus_message(
    bus: &Bus,
    to: &str,
    kind: &str,
    payload: serde_json::Value,
) -> ControlResponse {
    let mailbox = match register_control_mailbox(bus) {
        Ok(mailbox) => mailbox,
        Err(message) => return ControlResponse::Error { message },
    };
    match mailbox.send(to, kind, payload) {
        Ok(_) => ControlResponse::Ok,
        Err(error) => ControlResponse::Error {
            message: format!("Send failed: {error}"),
        },
    }
}

fn register_control_mailbox(bus: &Bus) -> Result<agent_bus::Mailbox, String> {
    let base_name = format!("control-{}", std::process::id());
    if let Ok(mailbox) = bus.register(&base_name) {
        return Ok(mailbox);
    }

    let fallback_name = format!("{base_name}-{}", timestamp_suffix());
    bus.register(&fallback_name)
        .map_err(|error| format!("Bus register: {error}"))
}

fn role_from_name(name: &str) -> &str {
    if name.starts_with("task-") {
        "task_agent"
    } else if name == "merger" {
        "merger"
    } else {
        "unknown"
    }
}

fn timestamp_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}
