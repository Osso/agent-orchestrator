use agent_bus::Bus;
use peercred_ipc::Server;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

/// Returns the path for the control Unix socket.
/// Uses ~/.claude/orchestrator/ so it's accessible inside bwrap sandboxes.
fn control_socket_path() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    home.join(".claude/orchestrator/control.sock")
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ControlRequest {
    SendMessage { to: String, content: String },
    StartTask { task: String },
    Abort,
    Status,
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
    bus: Bus,
    project: String,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let socket_path = control_socket_path();
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
                        let bus = bus.clone();
                        let project = project.clone();
                        let shutdown_tx = shutdown_tx.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(&mut conn, &bus, &project, &shutdown_tx).await {
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
    bus: &Bus,
    project: &str,
    shutdown_tx: &watch::Sender<bool>,
) -> anyhow::Result<()> {
    let request: ControlRequest = conn.read().await?;
    let response = handle_request(request, bus, project, shutdown_tx);
    conn.write(&response).await?;
    Ok(())
}

fn handle_request(
    request: ControlRequest,
    bus: &Bus,
    project: &str,
    shutdown_tx: &watch::Sender<bool>,
) -> ControlResponse {
    match request {
        ControlRequest::SendMessage { to, content } => send_to_agent(bus, &to, &content),
        ControlRequest::StartTask { task } => send_to_agent(bus, "manager", &task),
        ControlRequest::Abort => {
            let _ = shutdown_tx.send(true);
            ControlResponse::Ok
        }
        ControlRequest::Status => {
            let agents = bus
                .list_registered()
                .into_iter()
                .filter(|n| n != "runtime" && !n.starts_with("relay-"))
                .map(|name| {
                    let role = role_from_name(&name).to_string();
                    AgentStatus { name, role }
                })
                .collect();
            ControlResponse::Status {
                agents,
                project: project.to_string(),
            }
        }
    }
}

fn send_to_agent(bus: &Bus, to: &str, content: &str) -> ControlResponse {
    // Use a unique name so we don't conflict with other control connections
    let name = format!("control-{}", std::process::id());
    let mailbox = match bus.register(&name) {
        Ok(m) => m,
        Err(_) => {
            // If name taken, try with timestamp suffix
            let name = format!("control-{}-{}", std::process::id(), timestamp_suffix());
            match bus.register(&name) {
                Ok(m) => m,
                Err(e) => return ControlResponse::Error { message: format!("Bus register: {e}") },
            }
        }
    };
    match mailbox.send(to, "external_message", serde_json::json!({ "content": content })) {
        Ok(_) => ControlResponse::Ok,
        Err(e) => ControlResponse::Error { message: format!("Send failed: {e}") },
    }
    // mailbox dropped here → auto-deregisters from bus
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
