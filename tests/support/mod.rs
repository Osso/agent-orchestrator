use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_bus::Bus;
use agent_orchestrator::agent::{Agent, AgentConfig, BackendKind, Completer};
use agent_orchestrator::runtime::{AgentFactory, OrchestratorRuntime};
use agent_orchestrator::types::{AgentId, AgentRole};
use anyhow::Result;
use async_trait::async_trait;
use llm_sdk::session::SessionStore;

/// Test completer that returns pre-configured responses.
pub struct FakeCompleter {
    responses: Arc<Mutex<VecDeque<Result<llm_sdk::Output, llm_sdk::Error>>>>,
    pub call_count: Arc<AtomicUsize>,
}

impl FakeCompleter {
    pub fn new(responses: Vec<Result<llm_sdk::Output, llm_sdk::Error>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            call_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn with_texts(texts: Vec<&str>) -> Self {
        let responses = texts.into_iter().map(|t| Ok(fake_output(t))).collect();
        Self::new(responses)
    }
}

#[async_trait]
impl Completer for FakeCompleter {
    async fn complete(&mut self, _prompt: &str) -> Result<llm_sdk::Output, llm_sdk::Error> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let mut q = self.responses.lock().unwrap();
        q.pop_front().unwrap_or_else(|| Ok(fake_output("")))
    }
}

pub fn fake_output(text: &str) -> llm_sdk::Output {
    llm_sdk::Output {
        text: text.to_string(),
        usage: None,
        session_id: None,
        cost_usd: None,
    }
}

fn test_session_store() -> SessionStore {
    let tmp = std::env::temp_dir().join(format!(
        "orch-test-support-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    SessionStore::load(tmp)
}

/// Build an AgentConfig for testing.
pub fn test_config(role: AgentRole, index: u8, initial_task: Option<&str>) -> AgentConfig {
    let agent_id = match role {
        AgentRole::Developer => AgentId::new_developer(index),
        _ => AgentId::new_singleton(role),
    };
    AgentConfig {
        agent_id,
        working_dir: "/tmp".to_string(),
        system_prompt: "test".to_string(),
        initial_task: initial_task.map(|s| s.to_string()),
        mcp_config: None,
        fresh_session_per_task: false,
        backend: BackendKind::Claude,
        session_store: test_session_store(),
        bus: None,
        sandbox_prefix: Vec::new(),
    }
}

/// Build an agent factory where every spawned agent uses a FakeCompleter.
/// All agents share the same call counter. Each agent gets `responses` copies.
pub fn fake_factory(responses: Vec<&str>) -> (AgentFactory, Arc<AtomicUsize>) {
    let call_count = Arc::new(AtomicUsize::new(0));
    let shared: Arc<Vec<String>> = Arc::new(responses.into_iter().map(|s| s.to_string()).collect());
    let counter = call_count.clone();

    let factory: AgentFactory = Arc::new(move |config, mailbox| {
        let resps: Vec<Result<llm_sdk::Output, llm_sdk::Error>> =
            shared.iter().map(|t| Ok(fake_output(t))).collect();
        let mut fake = FakeCompleter::new(resps);
        fake.call_count = counter.clone();
        Ok(Agent::with_completer(config, mailbox, Box::new(fake)))
    });

    (factory, call_count)
}

/// Build a test runtime with a fake factory.
pub async fn test_runtime(
    bus: Bus,
    responses: Vec<&str>,
) -> Result<(OrchestratorRuntime, Arc<AtomicUsize>)> {
    let (factory, calls) = fake_factory(responses);
    let rt = OrchestratorRuntime::new_test(bus, "/tmp/test-project", factory, BackendKind::Claude)
        .await?;
    Ok((rt, calls))
}
