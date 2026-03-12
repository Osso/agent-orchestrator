use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use llm_sdk::session::SessionStore;
use llm_tasks::db::Database;

use crate::relay;
use crate::types::AgentRole;

pub struct CommandTimers {
    pub poll: tokio::time::Interval,
    pub timeout: tokio::time::Interval,
    pub watchdog: tokio::time::Interval,
    pub sigint: tokio::signal::unix::Signal,
    pub sigterm: tokio::signal::unix::Signal,
}

impl CommandTimers {
    pub fn new() -> Result<Self> {
        let now = tokio::time::Instant::now();
        Ok(Self {
            poll: tokio::time::interval_at(now + Duration::from_secs(10), Duration::from_secs(10)),
            timeout: tokio::time::interval_at(
                now + Duration::from_secs(60),
                Duration::from_secs(60),
            ),
            watchdog: tokio::time::interval_at(
                now + Duration::from_secs(30),
                Duration::from_secs(600),
            ),
            sigint: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            sigterm: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        })
    }
}

pub async fn open_test_stores() -> Result<(Database, SessionStore)> {
    let tmp = std::env::temp_dir().join(format!(
        "orch-test-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::create_dir_all(&tmp);
    let db = Database::open(&tmp.join("tasks.db"))
        .await
        .context("Failed to open test database")?;
    Ok((db, SessionStore::load(tmp.join("sessions"))))
}

pub fn payload_str(payload: &serde_json::Value, key: &str) -> String {
    payload
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn is_worktree_role(bus_name: &str) -> bool {
    bus_name.starts_with("task-")
}

/// Determine working directory and sandbox prefix for an agent.
pub fn resolve_sandbox(
    role: AgentRole,
    project_path: &Path,
    worktree_result: Result<PathBuf>,
    use_sandbox: bool,
) -> (String, Vec<String>) {
    let is_dev = matches!(role, AgentRole::TaskAgent | AgentRole::Merger);

    if is_dev {
        let dev_path = worktree_result.unwrap_or_else(|_| project_path.to_path_buf());
        if use_sandbox {
            let git_dir = find_git_dir(project_path);
            let mut prefix = llm_sdk::sandbox::developer_prefix(&dev_path, git_dir.as_deref());
            add_support_mounts(&mut prefix, project_path, &dev_path);
            return (llm_sdk::sandbox::REPO_MOUNT.to_string(), prefix);
        }
        return (dev_path.to_string_lossy().into_owned(), Vec::new());
    }

    if use_sandbox {
        let prefix = llm_sdk::sandbox::readonly_prefix(project_path);
        (llm_sdk::sandbox::REPO_MOUNT.to_string(), prefix)
    } else {
        (project_path.to_string_lossy().into_owned(), Vec::new())
    }
}

/// Find the `.git` directory for a project (resolves worktree indirection).
fn find_git_dir(project_path: &Path) -> Option<PathBuf> {
    let git_path = project_path.join(".git");
    if git_path.is_dir() {
        return Some(git_path.canonicalize().unwrap_or(git_path));
    }
    if git_path.is_file() {
        if let Ok(content) = std::fs::read_to_string(&git_path) {
            if let Some(gitdir) = content.strip_prefix("gitdir: ") {
                let p = Path::new(gitdir.trim());
                if let Some(parent) = p.parent().and_then(|p| p.parent()) {
                    if parent.is_dir() {
                        return Some(
                            parent
                                .canonicalize()
                                .unwrap_or_else(|_| parent.to_path_buf()),
                        );
                    }
                }
            }
        }
    }
    None
}

pub fn build_mcp_config(agent_name: &str, project: &str) -> String {
    let socket_path = relay::relay_socket_path(project);
    let exe = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from("agent-orchestrator"))
        .to_string_lossy()
        .into_owned();
    serde_json::json!({
        "mcpServers": {
            "orchestrator": {
                "command": exe,
                "args": ["mcp-serve", "--socket", socket_path.to_string_lossy(), "--agent", agent_name]
            }
        }
    })
    .to_string()
}

fn add_support_mounts(prefix: &mut Vec<String>, project_path: &Path, dev_path: &Path) {
    for (host_path, mount_path) in support_mounts(project_path, dev_path) {
        insert_rw_bind(prefix, &host_path, &mount_path);
    }
}

fn support_mounts(project_path: &Path, dev_path: &Path) -> Vec<(String, String)> {
    let mut mounts = Vec::new();
    let Some(project_name) = project_path.file_name().and_then(|n| n.to_str()) else {
        return mounts;
    };
    let Some(alias_name) = project_name.strip_suffix("-phpstan-fixes") else {
        return mounts;
    };
    if !project_path.join(".worktrees").join(alias_name).exists() {
        return mounts;
    }
    let host = dev_path
        .canonicalize()
        .unwrap_or_else(|_| dev_path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    mounts.push((host, format!("/tmp/{alias_name}")));
    mounts
}

fn insert_rw_bind(prefix: &mut Vec<String>, host_path: &str, mount_path: &str) {
    let insert_at = prefix
        .iter()
        .position(|arg| arg == "--chdir")
        .unwrap_or(prefix.len().saturating_sub(1));
    prefix.splice(
        insert_at..insert_at,
        [
            "--bind".to_string(),
            host_path.to_string(),
            mount_path.to_string(),
        ],
    );
}
