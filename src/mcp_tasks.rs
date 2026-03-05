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
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};

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
struct SetDevelopersParams {
    /// Number of developer agents (1-3)
    count: u8,
}

// --- MCP server ---

struct TasksMcp {
    db: Arc<Database>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl TasksMcp {
    #[tool(description = "List tasks. Optionally filter by status or assignee.")]
    async fn list_tasks(&self, Parameters(p): Parameters<ListTasksParams>) -> String {
        match self.db.list_tasks(p.status.as_deref(), p.assignee.as_deref()).await {
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
        let deps = self.db.get_dependencies(&p.id).await.unwrap_or_default();
        let blocked_by = self.db.get_reverse_dependencies(&p.id).await.unwrap_or_default();
        to_json(&serde_json::json!({
            "task": task,
            "events": events,
            "blocks": deps,
            "blocked_by": blocked_by,
        }))
    }

    #[tool(description = "Add a new task. Auto-dispatches to an idle developer if the orchestrator is running.")]
    async fn add_task(&self, Parameters(p): Parameters<AddTaskParams>) -> String {
        match self.db.create_task(&p.title, p.description.as_deref(), p.priority.unwrap_or(0), "user").await {
            Ok(task) => {
                notify_runtime(&task.id);
                to_json(&task)
            }
            Err(e) => err(e),
        }
    }

    #[tool(description = "Update a task's status, title, description, or priority. Use status 'pending_delete' to mark for safe deletion by the orchestrator.")]
    async fn update_task(&self, Parameters(p): Parameters<UpdateTaskParams>) -> String {
        let updates = TaskUpdates {
            status: p.status.as_deref(),
            title: p.title.as_deref(),
            description: p.description.as_deref(),
            priority: p.priority,
            assignee: None,
        };
        match self.db.update_task(&p.id, updates, "user").await {
            Ok(()) => match self.db.get_task(&p.id).await {
                Ok(task) => to_json(&task),
                Err(e) => err(e),
            },
            Err(e) => err(e),
        }
    }

    #[tool(description = "Delete a task. Immediate if pending/done/pending_delete, otherwise sets to 'pending_delete' for the orchestrator to clean up.")]
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
            let updates = TaskUpdates { status: Some("pending_delete"), ..Default::default() };
            match self.db.update_task(&p.id, updates, "user").await {
                Ok(()) => format!("Marked as pending_delete (currently {})", task.status),
                Err(e) => err(e),
            }
        }
    }

    #[tool(description = "Add a dependency: task_id is blocked by depends_on.")]
    async fn add_dependency(&self, Parameters(p): Parameters<DependencyParams>) -> String {
        let dep_type = p.dep_type.as_deref().unwrap_or("blocks");
        match self.db.add_dependency(&p.task_id, &p.depends_on, dep_type).await {
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

    #[tool(description = "Set the number of developer agents (1-3). Requires a running orchestrator.")]
    async fn set_developers(&self, Parameters(p): Parameters<SetDevelopersParams>) -> String {
        let count = p.count.clamp(1, 3);
        let socket_path = control::control_socket_path();
        let req = control::ControlRequest::SetDevelopers { count };
        match tokio::task::spawn_blocking(move || {
            peercred_ipc::Client::call::<_, control::ControlRequest, control::ControlResponse>(
                &socket_path, &req,
            )
        })
        .await
        {
            Ok(Ok(control::ControlResponse::Ok)) => format!("Developer count set to {count}"),
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

fn can_delete_immediately(status: &str) -> bool {
    matches!(status, "pending" | "done" | "completed" | "pending_delete")
}

fn to_json<T: Serialize>(val: &T) -> String {
    serde_json::to_string_pretty(val).unwrap_or_else(|e| format!("JSON error: {e}"))
}

fn err(e: impl std::fmt::Display) -> String {
    format!("Error: {e}")
}

/// Notify the running orchestrator that a task was created.
/// Silently fails if no orchestrator is running.
fn notify_runtime(task_id: &str) {
    let socket_path = control::control_socket_path();
    let req = control::ControlRequest::NotifyTaskCreated { task_id: task_id.to_string() };
    let _ = peercred_ipc::Client::call::<_, control::ControlRequest, control::ControlResponse>(
        &socket_path, &req,
    );
}

pub async fn run(db_path: &std::path::Path) -> Result<()> {
    let db = Database::open(db_path).await?;
    let service = TasksMcp {
        db: Arc::new(db),
        tool_router: TasksMcp::tool_router(),
    };
    let server = service.serve(rmcp::transport::io::stdio()).await?;
    server.waiting().await?;
    Ok(())
}
