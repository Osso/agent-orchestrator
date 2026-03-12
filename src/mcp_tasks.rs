//! MCP stdio server for task management from Claude Code.
//!
//! Opens the task DB directly (no relay needed). Project name comes from
//! CLAUDE_CODE_TASK_LIST_ID env var or --project flag.

use std::sync::Arc;

use anyhow::Result;
use llm_tasks::db::{Database, TaskUpdates};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::schemars::JsonSchema;
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::control;

// --- Param types ---

#[derive(Debug, Deserialize, JsonSchema)]
struct AddTaskParams {
    /// Task title (short, actionable)
    title: String,
    /// Detailed description with context
    description: Option<String>,
    /// Priority: 0=none, 1=low, 2=medium, 3=high
    priority: Option<u8>,
    /// Target branch for worktrees (defaults to current branch)
    target_branch: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListTasksParams {
    /// Filter by status: pending, ready, in_progress, needs_info, in_review, done, pending_delete
    status: Option<String>,
    /// Filter by assignee (e.g. "developer-0")
    assignee: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GetTaskParams {
    /// Task ID (e.g. "lt-abc123")
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateTaskParams {
    /// Task ID
    id: String,
    /// New status
    status: Option<String>,
    /// New title
    title: Option<String>,
    /// New description
    description: Option<String>,
    /// New priority: 0=none, 1=low, 2=medium, 3=high
    priority: Option<u8>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeleteTaskParams {
    /// Task ID
    id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DependencyParams {
    /// Task that depends on another
    task_id: String,
    /// Task it depends on (blocker)
    depends_on: String,
    /// Dependency type (default: "blocks")
    dep_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RemoveDependencyParams {
    /// Task that depends on another
    task_id: String,
    /// Task it depends on (blocker)
    depends_on: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddCommentParams {
    /// Task ID
    id: String,
    /// Comment text
    content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SetConcurrencyParams {
    /// Global maximum number of parallel task agents (1-20). Each agent runs in its own git worktree.
    max: u8,
}

// --- MCP server ---

struct TasksMcp {
    db: Arc<Database>,
    project: String,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl TasksMcp {
    #[tool(description = "List tasks. Optionally filter by status or assignee.")]
    async fn list_tasks(&self, Parameters(p): Parameters<ListTasksParams>) -> String {
        match self
            .db
            .list_tasks(p.status.as_deref(), p.assignee.as_deref())
            .await
        {
            Ok(tasks) => to_json(&tasks),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Get a single task with its event history.")]
    async fn get_task(&self, Parameters(p): Parameters<GetTaskParams>) -> String {
        let task = match self.db.get_task(&p.id).await {
            Ok(t) => t,
            Err(e) => return err(e),
        };
        let events = self.db.get_events(&p.id).await.unwrap_or_default();
        let comments = self.db.get_comments(&p.id).await.unwrap_or_default();
        let deps = self.db.get_dependencies(&p.id).await.unwrap_or_default();
        let blocked_by = self
            .db
            .get_reverse_dependencies(&p.id)
            .await
            .unwrap_or_default();
        to_json(&serde_json::json!({
            "task": task,
            "events": events,
            "comments": comments,
            "blocks": deps,
            "blocked_by": blocked_by,
        }))
    }

    #[tool(
        description = "Add a new task. Auto-dispatches to an idle developer if the orchestrator is running."
    )]
    async fn add_task(&self, Parameters(p): Parameters<AddTaskParams>) -> String {
        let branch = p.target_branch.unwrap_or_else(detect_current_branch);
        match self
            .db
            .create_task_with_branch(
                &p.title,
                p.description.as_deref(),
                p.priority.unwrap_or(0),
                "user",
                Some(&branch),
            )
            .await
        {
            Ok(task) => {
                notify_runtime(&self.project, &task.id);
                to_json(&task)
            }
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Update a task's status, title, description, or priority. Use status 'pending_delete' to mark for safe deletion by the orchestrator."
    )]
    async fn update_task(&self, Parameters(p): Parameters<UpdateTaskParams>) -> String {
        let updates = TaskUpdates {
            status: p.status.as_deref(),
            title: p.title.as_deref(),
            description: p.description.as_deref(),
            priority: p.priority,
            ..Default::default()
        };
        match self.db.update_task(&p.id, updates, "user").await {
            Ok(()) => {
                if is_dispatchable(p.status.as_deref()) {
                    let _ = self.db.clear_assignee(&p.id, "user").await;
                    notify_runtime(&self.project, &p.id);
                }
                match self.db.get_task(&p.id).await {
                    Ok(task) => to_json(&task),
                    Err(e) => err(e),
                }
            }
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Delete a task. Immediate if pending/done/pending_delete, otherwise sets to 'pending_delete' for the orchestrator to clean up."
    )]
    async fn delete_task(&self, Parameters(p): Parameters<DeleteTaskParams>) -> String {
        let task = match self.db.get_task(&p.id).await {
            Ok(t) => t,
            Err(e) => return err(e),
        };
        if can_delete_immediately(&task.status) {
            match self.db.close_task(&p.id, "user").await {
                Ok(()) => "Deleted".to_string(),
                Err(e) => err(e),
            }
        } else {
            let updates = TaskUpdates {
                status: Some("pending_delete"),
                ..Default::default()
            };
            match self.db.update_task(&p.id, updates, "user").await {
                Ok(()) => format!("Marked as pending_delete (currently {})", task.status),
                Err(e) => err(e),
            }
        }
    }

    #[tool(description = "Add a dependency: task_id is blocked by depends_on.")]
    async fn add_dependency(&self, Parameters(p): Parameters<DependencyParams>) -> String {
        let dep_type = p.dep_type.as_deref().unwrap_or("blocks");
        match self
            .db
            .add_dependency(&p.task_id, &p.depends_on, dep_type)
            .await
        {
            Ok(()) => "Dependency added".to_string(),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Remove a dependency between two tasks.")]
    async fn remove_dependency(&self, Parameters(p): Parameters<RemoveDependencyParams>) -> String {
        match self.db.remove_dependency(&p.task_id, &p.depends_on).await {
            Ok(()) => "Dependency removed".to_string(),
            Err(e) => err(e),
        }
    }

    #[tool(description = "Add a comment to a task. Use for context, questions, or status notes.")]
    async fn add_comment(&self, Parameters(p): Parameters<AddCommentParams>) -> String {
        match self.db.add_comment(&p.id, "user", &p.content).await {
            Ok(comment) => to_json(&comment),
            Err(e) => err(e),
        }
    }

    #[tool(
        description = "Scale the global maximum number of parallel task agents (1-20) across all projects. Requires a running orchestrator."
    )]
    async fn set_concurrency(&self, Parameters(p): Parameters<SetConcurrencyParams>) -> String {
        let max = p.max.clamp(1, 20);
        let socket_path = control::control_socket_path();
        let req = control::ControlRequest::SetConcurrency { max };
        match tokio::task::spawn_blocking(move || {
            peercred_ipc::Client::call::<_, control::ControlRequest, control::ControlResponse>(
                &socket_path,
                &req,
            )
        })
        .await
        {
            Ok(Ok(control::ControlResponse::Ok)) => format!("Max concurrency set to {max}"),
            Ok(Ok(resp)) => format!("Unexpected response: {resp:?}"),
            Ok(Err(e)) => format!("Error: {e} (is the orchestrator running?)"),
            Err(e) => format!("Error: {e}"),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TasksMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Task management for agent-orchestrator projects. \
                 Create, update, delete tasks and manage dependencies."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

fn is_dispatchable(status: Option<&str>) -> bool {
    matches!(status, Some("pending" | "ready"))
}

fn can_delete_immediately(status: &str) -> bool {
    matches!(status, "pending" | "done" | "completed" | "pending_delete")
}

fn to_json<T: Serialize>(val: &T) -> String {
    serde_json::to_string_pretty(val).unwrap_or_else(|e| format!("JSON error: {e}"))
}

fn err(e: impl std::fmt::Display) -> String {
    format!("Error: {e}")
}

/// Detect the current git branch. Falls back to "master".
fn detect_current_branch() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if s.is_empty() || s == "HEAD" {
                None
            } else {
                Some(s)
            }
        })
        .unwrap_or_else(|| "master".to_string())
}

/// Notify the running orchestrator that a task was created.
/// Silently fails if no orchestrator is running.
fn notify_runtime(project: &str, task_id: &str) {
    let socket_path = control::control_socket_path();
    let req = control::ControlRequest::NotifyTaskCreated {
        project: project.to_string(),
        task_id: task_id.to_string(),
    };
    let _ = peercred_ipc::Client::call::<_, control::ControlRequest, control::ControlResponse>(
        &socket_path,
        &req,
    );
}

pub async fn run(db_path: &std::path::Path, project: &str) -> Result<()> {
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = cwd.to_string_lossy().into_owned();
        match config::ensure_project_registered(project, &cwd) {
            Ok(true) => tracing::info!("Registered project '{}' at {}", project, cwd),
            Ok(false) => {}
            Err(e) => tracing::warn!("Failed to register project '{}': {}", project, e),
        }
    }

    let db = Database::open(db_path).await?;
    let service = TasksMcp {
        db: Arc::new(db),
        project: project.to_string(),
        tool_router: TasksMcp::tool_router(),
    };
    let server = service.serve(rmcp::transport::io::stdio()).await?;
    server.waiting().await?;
    Ok(())
}
