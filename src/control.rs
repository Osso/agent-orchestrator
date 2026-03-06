use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use agent_bus::Bus;
use peercred_ipc::Server;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

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
    SendMessage { project: String, to: String, content: String },
    StartTask { project: String, task: String },
    SetDevelopers { project: String, count: u8 },
    NotifyTaskCreated { project: String, task_id: String },
    Abort { project: String },
    Status { project: String },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ControlResponse {
    Ok,
    Error { message: String },
    Status { agents: Vec<AgentStatus>, project: String },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AgentStatus {
    pub name: String,
    pub role: String,
}

pub async fn run_control_server(
    registry: ProjectRegistry,
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
                match result {
                    Ok((mut conn, caller)) => {
                        info!("Control connection from pid={} uid={}", caller.pid, caller.uid);
                        let registry = registry.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(&mut conn, &registry, &shutdown_tx).await {
                                warn!("Control connection error: {e}");
                            }
                        });
                    }
                    Err(e) => warn!("Control accept error: {e}"),
                }
            }
            _ = shutdown_rx.changed() => {
                info!("Control server shutting down");
                break;
            }
        }
    }
}

async fn handle_connection(
    conn: &mut peercred_ipc::Connection,
    registry: &ProjectRegistry,
    shutdown_tx: &watch::Sender<bool>,
) -> anyhow::Result<()> {
    let request: ControlRequest = conn.read().await?;
    let response = handle_request(request, registry, shutdown_tx);
    conn.write(&response).await?;
    Ok(())
}

fn handle_request(
    request: ControlRequest,
    registry: &ProjectRegistry,
    shutdown_tx: &watch::Sender<bool>,
) -> ControlResponse {
    match request {
        ControlRequest::SendMessage { project, to, content } => {
            with_bus(registry, &project, |bus| send_to_agent(bus, &to, &content))
        }
        ControlRequest::StartTask { project, task } => {
            with_bus(registry, &project, |bus| send_to_agent(bus, "manager", &task))
        }
        ControlRequest::SetDevelopers { project, count } => {
            with_bus(registry, &project, |bus| {
                send_bus_message(bus, "runtime", "set_crew", serde_json::json!({ "count": count }))
            })
        }
        ControlRequest::NotifyTaskCreated { project, task_id } => {
            with_bus(registry, &project, |bus| {
                send_bus_message(bus, "runtime", "task_created", serde_json::json!({ "task_id": task_id }))
            })
        }
        ControlRequest::Abort { project } => {
            with_bus(registry, &project, |_bus| {
                let _ = shutdown_tx.send(true);
                ControlResponse::Ok
            })
        }
        ControlRequest::Status { project } => {
            let p = project.clone();
            with_bus(registry, &project, |bus| {
                let agents = bus
                    .list_registered()
                    .into_iter()
                    .filter(|n| n != "runtime" && !n.starts_with("relay-"))
                    .map(|name| {
                        let role = role_from_name(&name).to_string();
                        AgentStatus { name, role }
                    })
                    .collect();
                ControlResponse::Status { agents, project: p }
            })
        }
    }
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
    send_bus_message(bus, to, "external_message", serde_json::json!({ "content": content }))
}

fn send_bus_message(bus: &Bus, to: &str, kind: &str, payload: serde_json::Value) -> ControlResponse {
    let name = format!("control-{}", std::process::id());
    let mailbox = match bus.register(&name) {
        Ok(m) => m,
        Err(_) => {
            let name = format!("control-{}-{}", std::process::id(), timestamp_suffix());
            match bus.register(&name) {
                Ok(m) => m,
                Err(e) => return ControlResponse::Error { message: format!("Bus register: {e}") },
            }
        }
    };
    match mailbox.send(to, kind, payload) {
        Ok(_) => ControlResponse::Ok,
        Err(e) => ControlResponse::Error { message: format!("Send failed: {e}") },
    }
}

fn role_from_name(name: &str) -> &str {
    if name.starts_with("developer") {
        "developer"
    } else if name == "manager" {
        "manager"
    } else if name == "architect" {
        "architect"
    } else if name == "auditor" {
        "auditor"
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
