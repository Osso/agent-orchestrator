//! Client for the external claude-architect daemon.
//!
//! Replaces the in-process architect agent. The runtime calls the systemd-managed
//! claude-architect service via Unix socket IPC for task validation and completion review.

use std::sync::Arc;
use std::time::Duration;

use agent_bus::Bus;
use claude_architect::{
    Request, Response, build_assessment_prompt, contains_incomplete, contains_needs_changes,
    socket_path, truncate,
};
use llm_tasks::db::Database;
use peercred_ipc::Client;

pub enum ValidateResult {
    Approved(String),
    NeedsChanges(String),
}

pub enum ReviewResult {
    Accomplished(String),
    Incomplete(String),
}

/// Validate a pending task via the external architect daemon.
pub async fn validate_task(
    project: &str,
    title: &str,
    description: &str,
    cwd: &str,
) -> Result<ValidateResult, String> {
    let request = build_validate_request(project, title, description, cwd);
    tokio::task::spawn_blocking(move || dispatch_validate(request))
        .await
        .map_err(|e| format!("join error: {e}"))?
}

/// Assess task completion via Haiku and report to the daemon.
pub async fn review_completion(
    project: &str,
    task_title: &str,
    dev_output: &str,
    diff: &str,
    cwd: &str,
) -> Result<ReviewResult, String> {
    let truncated_output = truncate(dev_output, 2000);
    let truncated_diff = truncate(diff, 4000);
    let combined = if truncated_diff.is_empty() {
        truncated_output
    } else {
        format!("{truncated_output}\n\n## Git diff\n```\n{truncated_diff}\n```")
    };
    let prompt = build_assessment_prompt(task_title, &combined);
    let assessment = call_haiku(&prompt).await?;

    report_to_daemon(project, task_title, &assessment, cwd);

    if contains_incomplete(&assessment) {
        Ok(ReviewResult::Incomplete(assessment))
    } else {
        Ok(ReviewResult::Accomplished(assessment))
    }
}

/// Run validation in background, update DB and notify via bus when done.
pub fn spawn_validation(db: Arc<Database>, bus: Bus, project: String, cwd: String, task: llm_tasks::db::Task) {
    let task_id = task.id.clone();
    tokio::spawn(async move {
        let desc = task.description.as_deref().unwrap_or("");
        let result = validate_task(&project, &task.title, desc, &cwd).await;
        apply_validation_result(&db, &bus, &task_id, result).await;
    });
}

/// Run completion review in background, update DB and notify via bus.
pub fn spawn_review(db: Arc<Database>, bus: Bus, project: String, cwd: String, task_id: String, dev_output: String, branch: String) {
    tokio::spawn(async move {
        let title = match db.get_task(&task_id).await {
            Ok(t) => t.title,
            Err(e) => {
                tracing::error!("Failed to get task {task_id} for review: {e}");
                return;
            }
        };
        let diff = get_branch_diff(&cwd, &branch).await;
        let result = review_completion(&project, &title, &dev_output, &diff, &cwd).await;
        apply_review_result(&db, &bus, &task_id, &title, result).await;
    });
}

// --- Internal helpers ---

fn build_validate_request(project: &str, title: &str, description: &str, cwd: &str) -> Request {
    let task_summary = if description.is_empty() {
        title.to_string()
    } else {
        format!("{title}: {description}")
    };
    Request::Validate {
        project: project.to_string(),
        goal: title.to_string(),
        tasks: vec![task_summary],
        cwd: cwd.to_string(),
    }
}

fn dispatch_validate(request: Request) -> Result<ValidateResult, String> {
    let path = socket_path();
    match Client::call_timeout::<_, Request, Response>(&path, &request, Duration::from_secs(180)) {
        Ok(Response::Verdict(v)) if contains_needs_changes(&v) => {
            Ok(ValidateResult::NeedsChanges(v))
        }
        Ok(Response::Verdict(v)) => Ok(ValidateResult::Approved(v)),
        Ok(Response::Error(e)) => Err(format!("architect error: {e}")),
        Ok(Response::Pong) => Err("unexpected pong".to_string()),
        Err(e) => Err(format!("architect IPC error: {e}")),
    }
}

async fn apply_validation_result(db: &Database, bus: &Bus, task_id: &str, result: Result<ValidateResult, String>) {
    // Never override pending_delete — close the task immediately
    if matches!(db.get_task(task_id).await, Ok(t) if t.status == "pending_delete") {
        tracing::info!("Task {task_id} is pending_delete, closing instead of applying validation");
        let _ = db.close_task(task_id, "runtime").await;
        return;
    }
    match result {
        Ok(ValidateResult::Approved(verdict)) => {
            approve_task(db, task_id, &verdict).await;
            notify_bus(bus, task_id, "runtime", "task_ready");
        }
        Ok(ValidateResult::NeedsChanges(verdict)) => {
            reject_task(db, task_id, &verdict).await;
            notify_bus(bus, task_id, "runtime", "task_rejected");
        }
        Err(e) => {
            tracing::error!("Architect validation failed for {task_id}: {e}");
            approve_task_fallback(db, task_id).await;
            notify_bus(bus, task_id, "runtime", "task_ready");
        }
    }
}

async fn apply_review_result(
    db: &Database,
    bus: &Bus,
    task_id: &str,
    title: &str,
    result: Result<ReviewResult, String>,
) {
    // Never override pending_delete — close the task immediately
    if matches!(db.get_task(task_id).await, Ok(t) if t.status == "pending_delete") {
        tracing::info!("Task {task_id} is pending_delete, closing instead of applying review");
        let _ = db.close_task(task_id, "runtime").await;
        return;
    }
    match result {
        Ok(ReviewResult::Accomplished(assessment)) => {
            complete_task(db, bus, task_id, title, &assessment).await;
        }
        Ok(ReviewResult::Incomplete(assessment)) => {
            reject_completion(db, bus, task_id, &assessment).await;
        }
        Err(e) => {
            tracing::error!("Completion review failed for {task_id}: {e}");
            complete_task_fallback(db, bus, task_id, title).await;
        }
    }
}

async fn approve_task(db: &Database, task_id: &str, verdict: &str) {
    let updates = llm_tasks::db::TaskUpdates { status: Some("ready"), ..Default::default() };
    let _ = db.update_task(task_id, updates, "architect").await;
    let short = truncate(verdict, 200);
    let _ = db.add_comment(task_id, "architect", &format!("Approved: {short}")).await;
    tracing::info!("Task {task_id} approved by external architect");
}

async fn approve_task_fallback(db: &Database, task_id: &str) {
    let updates = llm_tasks::db::TaskUpdates { status: Some("ready"), ..Default::default() };
    let _ = db.update_task(task_id, updates, "runtime").await;
    tracing::warn!("Auto-approved task {task_id} due to architect error");
}

async fn reject_task(db: &Database, task_id: &str, verdict: &str) {
    let short = truncate(verdict, 200);
    let _ = db.add_comment(task_id, "architect", &format!("Rejected: {short}")).await;
    tracing::warn!("Task {task_id} rejected by external architect");
}

async fn complete_task(db: &Database, bus: &Bus, task_id: &str, _title: &str, assessment: &str) {
    let _ = db.close_task(task_id, "architect").await;
    let short = truncate(assessment, 200);
    let _ = db.add_comment(task_id, "architect", &format!("Completed: {short}")).await;
    tracing::info!("Task {task_id} completed (review passed)");
    notify_bus(bus, task_id, "runtime", "task_done");
}

async fn complete_task_fallback(db: &Database, bus: &Bus, task_id: &str, _title: &str) {
    let _ = db.close_task(task_id, "runtime").await;
    tracing::warn!("Auto-completed task {task_id} due to review error");
    notify_bus(bus, task_id, "runtime", "task_done");
}

async fn reject_completion(db: &Database, bus: &Bus, task_id: &str, assessment: &str) {
    let updates = llm_tasks::db::TaskUpdates { status: Some("ready"), ..Default::default() };
    let _ = db.update_task(task_id, updates, "architect").await;
    let _ = db.clear_assignee(task_id, "architect").await;
    let short = truncate(assessment, 200);
    let _ = db.add_comment(task_id, "architect", &format!("Rejected: {short}")).await;
    tracing::warn!("Task {task_id} completion rejected");
    notify_bus(bus, task_id, "runtime", "task_ready");
}

fn notify_bus(bus: &Bus, task_id: &str, to: &str, kind: &str) {
    let tag = &task_id[..8.min(task_id.len())];
    if let Ok(mb) = bus.register(&format!("arch-{tag}")) {
        let _ = mb.send(to, kind, serde_json::json!({"task_id": task_id}));
    }
}

/// Fire-and-forget report to the daemon for context.
fn report_to_daemon(project: &str, task_title: &str, assessment: &str, cwd: &str) {
    let report_req = Request::Report {
        project: project.to_string(),
        task_description: task_title.to_string(),
        assessment: assessment.to_string(),
        cwd: cwd.to_string(),
    };
    tokio::task::spawn_blocking(move || {
        let path = socket_path();
        let _ = Client::call_timeout::<_, Request, Response>(
            &path,
            &report_req,
            Duration::from_secs(30),
        );
    });
}

async fn get_branch_diff(cwd: &str, branch: &str) -> String {
    let output = tokio::process::Command::new("git")
        .args(["diff", &format!("master..{branch}"), "--stat", "-p"])
        .current_dir(cwd)
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).to_string()
        }
        Ok(o) => {
            tracing::warn!("git diff failed: {}", String::from_utf8_lossy(&o.stderr));
            String::new()
        }
        Err(e) => {
            tracing::warn!("Failed to run git diff: {e}");
            String::new()
        }
    }
}

async fn call_haiku(prompt: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("claude")
        .arg("-p")
        .arg(prompt)
        .arg("--model")
        .arg("haiku")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .output()
        .await
        .map_err(|e| format!("failed to run claude: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("claude haiku exited {}: {stderr}", output.status));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon_available() -> bool {
        let path = socket_path();
        Client::call::<_, Request, Response>(&path, &Request::Ping)
            .is_ok_and(|r| matches!(r, Response::Pong))
    }

    #[tokio::test]
    #[ignore] // requires live claude-architect daemon
    async fn validate_task_returns_verdict() {
        assert!(daemon_available(), "claude-architect daemon not running");
        let result = validate_task(
            "agent-orchestrator",
            "Add retry logic to API calls",
            "Wrap HTTP calls in architect_client.rs with exponential backoff",
            "/syncthing/Sync/Projects/claude/agent-orchestrator",
        )
        .await;
        match result {
            Ok(ValidateResult::Approved(v)) => {
                assert!(v.contains("VERDICT"), "verdict missing VERDICT line: {v}");
            }
            Ok(ValidateResult::NeedsChanges(v)) => {
                assert!(v.contains("VERDICT"), "verdict missing VERDICT line: {v}");
                assert!(v.contains("needs-changes"));
            }
            Err(e) => panic!("validate_task failed: {e}"),
        }
    }

    #[tokio::test]
    #[ignore] // requires live daemon + claude CLI (cannot run inside Claude Code session)
    async fn review_completion_returns_assessment() {
        assert!(daemon_available(), "claude-architect daemon not running");
        let result = review_completion(
            "agent-orchestrator",
            "Add retry logic to API calls",
            "Added retry with exponential backoff to all HTTP calls. Tests pass.",
            "",
            "/syncthing/Sync/Projects/claude/agent-orchestrator",
        )
        .await;
        match result {
            Ok(ReviewResult::Accomplished(a)) => {
                assert!(a.contains("ACCOMPLISHED"), "unexpected assessment: {a}");
            }
            Ok(ReviewResult::Incomplete(a)) => {
                assert!(a.contains("INCOMPLETE"), "unexpected assessment: {a}");
            }
            Err(e) => panic!("review_completion failed: {e}"),
        }
    }

    #[test]
    fn build_validate_request_with_description() {
        let req = build_validate_request("proj", "Fix bug", "null pointer in parser", "/tmp");
        match req {
            Request::Validate { project, goal, tasks, cwd } => {
                assert_eq!(project, "proj");
                assert_eq!(goal, "Fix bug");
                assert_eq!(tasks, vec!["Fix bug: null pointer in parser"]);
                assert_eq!(cwd, "/tmp");
            }
            _ => panic!("expected Validate"),
        }
    }

    #[test]
    fn build_validate_request_empty_description() {
        let req = build_validate_request("proj", "Fix bug", "", "/tmp");
        match req {
            Request::Validate { tasks, .. } => {
                assert_eq!(tasks, vec!["Fix bug"]);
            }
            _ => panic!("expected Validate"),
        }
    }
}
