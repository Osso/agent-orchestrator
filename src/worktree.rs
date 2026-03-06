//! Git worktree management for agent isolation.
//!
//! Each Developer agent gets its own worktree at
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
    ensure_git_repo(&cfg.project_dir)?;
    ensure_head_exists(&cfg.project_dir)?;
    prune_stale_worktrees(&cfg.project_dir);
    let path = cfg.path();

    if let Some(reused) = try_reuse_worktree(cfg, &path) {
        return Ok(reused);
    }
    add_fresh_worktree(cfg, &path)
}

fn try_reuse_worktree(cfg: &WorktreeConfig, path: &PathBuf) -> Option<PathBuf> {
    if !path.join(".git").exists() {
        return None;
    }
    tracing::info!("Reusing existing worktree at {}", path.display());
    let branch = cfg.branch();
    let reset_ok = Command::new("git")
        .args(["switch", "-C", &branch])
        .current_dir(path)
        .status()
        .is_ok_and(|s| s.success());
    if !reset_ok {
        tracing::warn!("Branch switch failed, removing and recreating worktree");
        let _ = remove_worktree(cfg);
        return None;
    }
    let _ = Command::new("git")
        .args(["reset", "--hard", "HEAD"])
        .current_dir(path)
        .status();
    Some(path.clone())
}

fn add_fresh_worktree(cfg: &WorktreeConfig, path: &PathBuf) -> Result<PathBuf> {
    let branch = cfg.branch();
    let path_str = path.to_str().context("worktree path is not valid UTF-8")?;
    let status = Command::new("git")
        .args(["worktree", "add", "--force", "-B", &branch, path_str, "HEAD"])
        .current_dir(&cfg.project_dir)
        .status()
        .context("failed to run git worktree add")?;
    if !status.success() {
        anyhow::bail!("git worktree add failed with status {}", status);
    }
    Ok(path.clone())
}

fn prune_stale_worktrees(project_dir: &std::path::Path) {
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(project_dir)
        .status();
}

/// Initialize a git repo if the project directory isn't one.
fn ensure_git_repo(project_dir: &std::path::Path) -> Result<()> {
    let status = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(project_dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("failed to run git rev-parse")?;
    if status.success() {
        return Ok(());
    }
    tracing::info!("Initializing git repo in {}", project_dir.display());
    Command::new("git")
        .args(["init"])
        .current_dir(project_dir)
        .status()
        .context("failed to run git init")?;
    Ok(())
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
