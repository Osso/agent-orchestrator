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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn temp_config_home(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "agent_orchestrator_config_{name}_{}",
            std::process::id()
        ))
    }

    fn with_config_home<T>(name: &str, test: impl FnOnce(&PathBuf) -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let config_home = temp_config_home(name);
        let old_config_home = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
        }

        let result = test(&config_home);

        match old_config_home {
            Some(value) => unsafe {
                std::env::set_var("XDG_CONFIG_HOME", value);
            },
            None => unsafe {
                std::env::remove_var("XDG_CONFIG_HOME");
            },
        }
        std::fs::remove_dir_all(config_home).ok();
        result
    }

    #[test]
    fn ensure_project_registered_writes_sorted_projects() {
        with_config_home("write", |config_home| {
            assert!(ensure_project_registered("zeta", "/repo/zeta").expect("register zeta"));
            assert!(ensure_project_registered("alpha", "/repo/alpha").expect("register alpha"));

            let config_file = config_home.join("agent-orchestrator/projects.toml");
            let contents = std::fs::read_to_string(config_file).expect("read config");

            assert!(
                contents.find("[alpha]").expect("alpha") < contents.find("[zeta]").expect("zeta")
            );
            assert!(contents.contains("dir = \"/repo/alpha\""));
            assert!(contents.contains("dir = \"/repo/zeta\""));
        });
    }

    #[test]
    fn ensure_project_registered_returns_false_when_unchanged() {
        with_config_home("unchanged", |_| {
            assert!(ensure_project_registered("alpha", "/repo/alpha").expect("first register"));
            assert!(!ensure_project_registered("alpha", "/repo/alpha").expect("second register"));
            let projects = load_config().expect("load config");

            assert_eq!(projects["alpha"].dir, "/repo/alpha");
        });
    }
}
