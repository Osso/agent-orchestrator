use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    TaskAgent,
    Merger,
}

impl AgentRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentRole::TaskAgent => "task_agent",
            AgentRole::Merger => "merger",
        }
    }

    pub fn system_prompt(&self) -> &'static str {
        match self {
            AgentRole::TaskAgent => include_str!("../../prompts/developer.md"),
            AgentRole::Merger => include_str!("../../prompts/merger.md"),
        }
    }
}

impl std::fmt::Display for AgentRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Unique identifier for an agent instance.
/// TaskAgents use a task-id-based name. Merger is a singleton.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub role: AgentRole,
    pub name: String,
}

impl AgentId {
    pub fn for_task(task_id: &str) -> Self {
        Self {
            role: AgentRole::TaskAgent,
            name: task_id.to_string(),
        }
    }

    pub fn merger() -> Self {
        Self {
            role: AgentRole::Merger,
            name: "merger".to_string(),
        }
    }

    pub fn bus_name(&self) -> String {
        if self.role == AgentRole::TaskAgent {
            format!("task-{}", self.name)
        } else {
            self.name.clone()
        }
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.bus_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_expose_names_prompts_and_display_values() {
        assert_eq!(AgentRole::TaskAgent.as_str(), "task_agent");
        assert_eq!(AgentRole::Merger.as_str(), "merger");
        assert_eq!(AgentRole::TaskAgent.to_string(), "task_agent");
        assert_eq!(AgentRole::Merger.to_string(), "merger");
        assert!(AgentRole::TaskAgent.system_prompt().contains("Task agent"));
        assert!(AgentRole::Merger.system_prompt().contains("Merger agent"));
    }

    #[test]
    fn agent_ids_build_expected_bus_names() {
        let task = AgentId::for_task("abc123");
        let merger = AgentId::merger();

        assert_eq!(task.role, AgentRole::TaskAgent);
        assert_eq!(task.name, "abc123");
        assert_eq!(task.bus_name(), "task-abc123");
        assert_eq!(task.to_string(), "task-abc123");
        assert_eq!(merger.role, AgentRole::Merger);
        assert_eq!(merger.name, "merger");
        assert_eq!(merger.bus_name(), "merger");
        assert_eq!(merger.to_string(), "merger");
    }
}
