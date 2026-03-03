//! Git worktree management for agent isolation.
//!
//! Each Developer/Merger agent gets its own worktree at
//! `{project_dir}/.worktrees/{agent_name}` on branch `agent/{agent_name}`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

pub struct WorktreeConfig {
    pub project_dir: PathBuf,
    pub agent_name: String,
}

impl WorktreeConfig {
    pub fn path(&self) -> PathBuf {
        self.project_dir.join(".worktrees").join(&self.agent_name)
    }

    pub fn branch(&self) -> String {
        format!("agent/{}", self.agent_name)
    }
}

pub fn create_worktree(cfg: &WorktreeConfig) -> Result<PathBuf> {
    ensure_head_exists(&cfg.project_dir)?;
    let path = cfg.path();
    let branch = cfg.branch();
    let status = Command::new("git")
        .args([
            "worktree",
            "add",
            "--force",
            "-B",
            &branch,
            path.to_str().context("worktree path is not valid UTF-8")?,
            "HEAD",
        ])
        .current_dir(&cfg.project_dir)
        .status()
        .context("failed to run git worktree add")?;

    if !status.success() {
        anyhow::bail!("git worktree add failed with status {}", status);
    }
    Ok(path)
}

/// Create an initial empty commit if the repo has no commits yet.
fn ensure_head_exists(project_dir: &std::path::Path) -> Result<()> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project_dir)
        .output()
        .context("failed to run git rev-parse HEAD")?;
    if output.status.success() {
        return Ok(());
    }
    Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init (agent-orchestrator)"])
        .current_dir(project_dir)
        .status()
        .context("failed to create initial commit")?;
    Ok(())
}

pub fn remove_worktree(cfg: &WorktreeConfig) -> Result<()> {
    let path = cfg.path();
    let remove_status = Command::new("git")
        .args([
            "worktree",
            "remove",
            "--force",
            path.to_str().context("worktree path is not valid UTF-8")?,
        ])
        .current_dir(&cfg.project_dir)
        .status()
        .context("failed to run git worktree remove")?;

    if !remove_status.success() {
        tracing::warn!("git worktree remove failed, continuing with prune");
    }

    let prune_status = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(&cfg.project_dir)
        .status()
        .context("failed to run git worktree prune")?;

    if !prune_status.success() {
        tracing::warn!("git worktree prune failed");
    }

    Ok(())
}
