use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProjectConfig {
    pub dir: String,
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("agent-orchestrator/projects.toml")
}

pub fn load_config() -> Result<HashMap<String, ProjectConfig>> {
    let path = config_path();
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let projects: HashMap<String, ProjectConfig> =
        toml::from_str(&contents).with_context(|| format!("Failed to parse {}", path.display()))?;
    Ok(projects)
}

pub fn ensure_project_registered(project: &str, dir: &str) -> Result<bool> {
    let path = config_path();
    let mut projects = if path.exists() {
        load_config()?
    } else {
        HashMap::new()
    };

    if projects.get(project).is_some_and(|cfg| cfg.dir == dir) {
        return Ok(false);
    }

    projects.insert(
        project.to_string(),
        ProjectConfig {
            dir: dir.to_string(),
        },
    );
    write_config(&path, &projects)?;
    Ok(true)
}

fn write_config(path: &PathBuf, projects: &HashMap<String, ProjectConfig>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let ordered: BTreeMap<String, ProjectConfig> = projects
        .iter()
        .map(|(name, cfg)| (name.clone(), cfg.clone()))
        .collect();
    let contents =
        toml::to_string_pretty(&ordered).context("Failed to serialize project config")?;
    std::fs::write(path, contents)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    Ok(())
}
