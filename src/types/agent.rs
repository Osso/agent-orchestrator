use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    Manager,
    Architect,
    Developer,
    Scorer,
}

impl AgentRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentRole::Manager => "manager",
            AgentRole::Architect => "architect",
            AgentRole::Developer => "developer",
            AgentRole::Scorer => "scorer",
        }
    }

    pub fn system_prompt(&self) -> &'static str {
        match self {
            AgentRole::Manager => include_str!("../../prompts/manager.md"),
            AgentRole::Architect => include_str!("../../prompts/architect.md"),
            AgentRole::Developer => include_str!("../../prompts/developer.md"),
            AgentRole::Scorer => include_str!("../../prompts/scorer.md"),
        }
    }
}

impl std::fmt::Display for AgentRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Unique identifier for an agent instance.
/// Singletons (manager, architect, scorer) use index 0.
/// Developers use index 0-2 for multi-developer support.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentId {
    pub role: AgentRole,
    pub index: u8,
}

impl AgentId {
    pub fn new_singleton(role: AgentRole) -> Self {
        Self { role, index: 0 }
    }

    pub fn new_developer(index: u8) -> Self {
        Self {
            role: AgentRole::Developer,
            index,
        }
    }

    pub fn socket_name(&self) -> String {
        if self.role == AgentRole::Developer {
            format!("developer-{}", self.index)
        } else {
            self.role.as_str().to_string()
        }
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.socket_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    WaitingForInput,
    Interrupted,
}
