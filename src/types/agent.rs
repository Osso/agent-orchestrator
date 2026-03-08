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
