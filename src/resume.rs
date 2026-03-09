//! Resume in-progress tasks from a previous daemon session.
//!
//! On startup, finds tasks that were `in_progress` when the daemon last shut down,
//! spawns agents with their preserved worktrees (partial work intact), and
//! re-dispatches with a resume prompt that includes the git diff.

use std::path::{Path, PathBuf};
use std::process::Command;

use std::sync::atomic::Ordering;

use anyhow::Result;

use crate::agent::{AgentConfig, BackendKind};
use crate::runtime::OrchestratorRuntime;
use crate::runtime_support as support;
use crate::types::{AgentId, AgentRole};
use crate::worktree::{self, WorktreeConfig};

impl OrchestratorRuntime {
    /// Resume in_progress tasks from a previous session.
    pub async fn resume_in_progress_tasks(&mut self) {
        let tasks = match self.db.list_tasks(Some("in_progress"), None).await {
            Ok(t) if !t.is_empty() => t,
            Ok(_) => return,
            Err(e) => {
                tracing::error!("Failed to query in_progress tasks for resume: {}", e);
                return;
            }
        };
        tracing::info!("Resuming {} in_progress tasks from previous session", tasks.len());
        for task in &tasks {
            self.resume_one_task(task).await;
        }
    }

    async fn resume_one_task(&mut self, task: &llm_tasks::db::Task) {
        let Some(assignee) = task.assignee.as_deref() else {
            self.reset_task_to_ready(&task.id).await;
            return;
        };
        // Assignee should be "task-{id}" format
        if !assignee.starts_with("task-") {
            tracing::warn!("Cannot resume non-task assignee '{}'", assignee);
            self.reset_task_to_ready(&task.id).await;
            return;
        }
        if let Err(e) = self.spawn_resuming_agent(assignee, task) {
            tracing::error!("Failed to resume {} on task {}: {}", assignee, task.id, e);
            self.reset_task_to_ready(&task.id).await;
        }
    }

    async fn reset_task_to_ready(&self, task_id: &str) {
        let updates = llm_tasks::db::TaskUpdates { status: Some("ready"), ..Default::default() };
        let _ = self.db.update_task(task_id, updates, "runtime").await;
        let _ = self.db.clear_assignee(task_id, "runtime").await;
    }

    fn spawn_resuming_agent(&mut self, bus_name: &str, task: &llm_tasks::db::Task) -> Result<()> {
        let task_id = bus_name.strip_prefix("task-").unwrap_or(&task.id);
        let agent_id = AgentId::for_task(task_id);
        let target_branch = task.target_branch.as_deref().unwrap_or("master");
        let (working_dir, sandbox_prefix, diff) = self.resume_worktree(bus_name, target_branch)?;
        let prompt = build_task_resume_prompt(task, bus_name, &diff);
        let config = self.resume_agent_config(agent_id, working_dir, sandbox_prefix);

        self.spawn_agent_with_config(config)?;
        self.global_limits.active_agents.fetch_add(1, Ordering::Relaxed);
        self.dispatcher.register_active(task.id.clone(), bus_name.to_string());
        let payload = serde_json::json!({"content": prompt, "task_id": task.id});
        if let Err(e) = self.dispatcher.notify(bus_name, "task_assignment", payload) {
            tracing::error!("Failed to send resume assignment to {}: {}", bus_name, e);
        }
        tracing::info!("Resumed {} on task {} (diff: {} bytes)", bus_name, task.id, diff.len());
        Ok(())
    }

    fn resume_worktree(&self, bus_name: &str, target_branch: &str) -> Result<(String, Vec<String>, String)> {
        let project_path = PathBuf::from(&self.working_dir);
        let wt_cfg = WorktreeConfig {
            project_dir: project_path.clone(),
            agent_name: bus_name.to_string(),
            target_branch: target_branch.to_string(),
        };
        let wt_path = worktree::create_or_resume_worktree(&wt_cfg)?;
        let diff = worktree_diff(&wt_path, target_branch);
        let use_sandbox = !self.no_sandbox && llm_sdk::sandbox::is_available();
        let (wd, sp) = support::resolve_sandbox(AgentRole::TaskAgent, &project_path, Ok(wt_path), use_sandbox);
        Ok((wd, sp, diff))
    }

    fn resume_agent_config(
        &self,
        agent_id: AgentId,
        working_dir: String,
        sandbox_prefix: Vec<String>,
    ) -> AgentConfig {
        let bus_name = agent_id.bus_name();
        let bus = match self.backend {
            BackendKind::OpenRouter { .. } | BackendKind::Codex { .. } => Some(self.bus.clone()),
            BackendKind::Claude => None,
        };
        AgentConfig {
            agent_id,
            working_dir,
            system_prompt: AgentRole::TaskAgent.system_prompt().to_string(),
            initial_task: None,
            mcp_config: Some(support::build_mcp_config(&bus_name, &self.project)),
            fresh_session_per_task: true,
            backend: self.backend.clone(),
            session_store: self.session_store.clone(),
            bus,
            sandbox_prefix,
        }
    }
}

/// Get the diff between the target branch and the worktree's current state.
fn worktree_diff(wt_path: &Path, target_branch: &str) -> String {
    let output = Command::new("git")
        .args(["diff", target_branch, "--stat"])
        .current_dir(wt_path)
        .output();
    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    }
}

fn build_task_resume_prompt(task: &llm_tasks::db::Task, bus_name: &str, diff: &str) -> String {
    let desc = task.description.as_deref().unwrap_or("");
    let branch = format!("agent/{}", bus_name);
    let target = task.target_branch.as_deref().unwrap_or("master");
    let diff_section = if diff.is_empty() {
        "No changes were committed yet on this branch.".to_string()
    } else {
        let d = if diff.len() > 4000 { &diff[..4000] } else { diff };
        format!("## Work already done (git diff {target} --stat)\n\n```\n{}\n```", d)
    };
    format!(
        "## RESUMING Task {id}\n\n{title}\n\n{desc}\n\n\
         This task was in progress in a previous session. The worktree on branch `{branch}` \
         has been preserved with any partial work.\n\n\
         {diff_section}\n\n\
         Review the existing changes, then continue where the previous session left off. \
         Commit your changes on branch `{branch}`.",
        id = task.id, title = task.title,
    )
}
