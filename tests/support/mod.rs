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

// Re-export test utilities
mod test_scenario;
pub use test_scenario::{AuditFinding, PerformanceMetrics, TestOutcome, TestScenario};

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

    pub fn with_errors(errors: Vec<llm_sdk::Error>) -> Self {
        let responses = errors.into_iter().map(|e| Err(e)).collect();
        Self::new(responses)
    }

    pub fn with_mixed_responses(responses: Vec<Result<&str, &str>>) -> Self {
        let results = responses
            .into_iter()
            .map(|r| match r {
                Ok(text) => Ok(fake_output(text)),
                Err(msg) => Err(llm_sdk::Error::Parse(msg.to_string())),
            })
            .collect();
        Self::new(results)
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
pub fn test_config(role: AgentRole, _index: u8, initial_task: Option<&str>) -> AgentConfig {
    let agent_id = match role {
        AgentRole::TaskAgent => AgentId::for_task(&format!("test-{}", uuid::Uuid::new_v4())),
        AgentRole::Merger => AgentId::merger(),
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

/// Test utilities for benchmarking and performance testing
pub struct TestBench {
    start_time: std::time::Instant,
    operations: usize,
}

impl TestBench {
    pub fn new() -> Self {
        Self {
            start_time: std::time::Instant::now(),
            operations: 0,
        }
    }

    pub fn record_operation(&mut self) {
        self.operations += 1;
    }

    pub fn throughput(&self) -> f64 {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            self.operations as f64 / elapsed
        } else {
            0.0
        }
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }
}

impl Default for TestBench {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper for creating test agents with specific behaviors
pub struct TestAgentBuilder {
    role: AgentRole,
    index: u8,
    responses: Vec<String>,
    initial_task: Option<String>,
    should_fail: bool,
}

impl TestAgentBuilder {
    pub fn new(role: AgentRole) -> Self {
        Self {
            role,
            index: 0,
            responses: vec!["ok".to_string()],
            initial_task: None,
            should_fail: false,
        }
    }

    pub fn with_index(mut self, index: u8) -> Self {
        self.index = index;
        self
    }

    pub fn with_responses(mut self, responses: Vec<&str>) -> Self {
        self.responses = responses.into_iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_initial_task(mut self, task: &str) -> Self {
        self.initial_task = Some(task.to_string());
        self
    }

    pub fn should_fail(mut self, fail: bool) -> Self {
        self.should_fail = fail;
        self
    }

    pub fn build(self, bus: &Bus) -> Result<Agent> {
        let config = test_config(self.role, self.index, self.initial_task.as_deref());
        let mailbox = bus.register(&config.agent_id.bus_name()).unwrap();

        let completer = if self.should_fail {
            FakeCompleter::with_mixed_responses(vec![Err("simulated failure"), Ok("recovered")])
        } else {
            FakeCompleter::with_texts(self.responses.iter().map(|s| s.as_str()).collect())
        };

        Ok(Agent::with_completer(config, mailbox, Box::new(completer)))
    }
}

/// Assertion helpers for testing
pub fn assert_agent_registered(bus: &Bus, agent_name: &str) {
    let registered = bus.list_registered();
    assert!(
        registered.contains(&agent_name.to_string()),
        "Agent '{}' should be registered. Found: {:?}",
        agent_name,
        registered
    );
}

pub fn assert_agent_not_registered(bus: &Bus, agent_name: &str) {
    let registered = bus.list_registered();
    assert!(
        !registered.contains(&agent_name.to_string()),
        "Agent '{}' should not be registered. Found: {:?}",
        agent_name,
        registered
    );
}

pub fn assert_message_count_gte(counter: &Arc<AtomicUsize>, expected: usize) {
    let actual = counter.load(Ordering::SeqCst);
    assert!(
        actual >= expected,
        "Expected at least {} messages, got {}",
        expected,
        actual
    );
}

pub fn assert_elapsed_within(elapsed: std::time::Duration, max: std::time::Duration) {
    assert!(
        elapsed <= max,
        "Operation took {:?}, should be within {:?}",
        elapsed,
        max
    );
}
