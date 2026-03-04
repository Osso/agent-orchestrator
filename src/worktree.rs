//! Git worktree management for agent isolation.
//!
//! Each Developer/Merger agent gets its own worktree at
//! `{project_dir}/.worktrees/{agent_name}` on branch `agent/{agent_name}`.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

pub enum MergeResult {
    Success,
    Conflict(String),
}

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
    prune_stale_worktrees(&cfg.project_dir);
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

fn prune_stale_worktrees(project_dir: &std::path::Path) {
    let _ = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(project_dir)
        .status();
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

/// Merge a source branch into master from the main project dir.
///
/// Tries ff-merge first (works when source is ahead of master with no divergence).
/// Falls back to a temp branch + rebase when history has diverged, since the
/// source branch is typically checked out in a developer worktree and can't be
/// rebased directly.
pub fn merge_branch(project_dir: &Path, source_branch: &str) -> MergeResult {
    // Ensure main worktree is on master
    if let Err(reason) = checkout_master(project_dir) {
        return MergeResult::Conflict(reason);
    }
    // Fast path: ff-merge works when source is strictly ahead of master
    match run_git(project_dir, &["merge", "--ff-only", source_branch]) {
        Ok(()) => return MergeResult::Success,
        Err(e) => tracing::debug!("ff-merge failed ({}), trying rebase fallback", e),
    }
    // Slow path: diverged history — rebase via temp branch to avoid worktree lock
    merge_via_temp_branch(project_dir, source_branch)
}

fn checkout_master(project_dir: &Path) -> Result<(), String> {
    run_git(project_dir, &["checkout", "master"])
}

fn merge_via_temp_branch(project_dir: &Path, source_branch: &str) -> MergeResult {
    let temp = format!("temp-merge-{}", source_branch.replace('/', "-"));
    // Create temp branch at source's tip (no checkout)
    if let Err(e) = run_git(project_dir, &["branch", "-f", &temp, source_branch]) {
        return MergeResult::Conflict(format!("create temp branch: {e}"));
    }
    // Rebase temp onto master (checks out temp, which is fine — not locked)
    if let Err(e) = run_git(project_dir, &["rebase", "master", &temp]) {
        let _ = run_git(project_dir, &["rebase", "--abort"]);
        let _ = run_git(project_dir, &["branch", "-D", &temp]);
        return MergeResult::Conflict(format!("rebase failed: {e}"));
    }
    // Back to master and ff-merge
    if let Err(e) = checkout_master(project_dir) {
        let _ = run_git(project_dir, &["branch", "-D", &temp]);
        return MergeResult::Conflict(format!("checkout master after rebase: {e}"));
    }
    let result = match run_git(project_dir, &["merge", "--ff-only", &temp]) {
        Ok(()) => MergeResult::Success,
        Err(e) => MergeResult::Conflict(format!("ff-merge after rebase: {e}")),
    };
    let _ = run_git(project_dir, &["branch", "-D", &temp]);
    result
}

fn run_git(project_dir: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(project_dir)
        .output()
        .map_err(|e| format!("failed to run git {}: {e}", args[0]))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(stderr.trim().to_string())
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
