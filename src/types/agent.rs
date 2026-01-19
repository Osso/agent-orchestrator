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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    WaitingForInput,
    Interrupted,
}
