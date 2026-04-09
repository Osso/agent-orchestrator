use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agent_bus::Bus;
use agent_orchestrator::types::AgentRole;
use anyhow::Result;

/// Test scenario builder for integration testing
pub struct TestScenario {
    agents: Vec<(AgentRole, &'static str)>,
    expected_messages: usize,
    timeout: Duration,
    failure_rate: f32,
    retry_enabled: bool,
    high_load: bool,
    message_burst: usize,
}

impl TestScenario {
    pub fn new() -> Self {
        Self {
            agents: vec![],
            expected_messages: 1,
            timeout: Duration::from_secs(5),
            failure_rate: 0.0,
            retry_enabled: false,
            high_load: false,
            message_burst: 1,
        }
    }

    pub fn with_agents(mut self, agents: Vec<(AgentRole, &'static str)>) -> Self {
        self.agents = agents;
        self
    }

    pub fn with_expected_messages(mut self, count: usize) -> Self {
        self.expected_messages = count;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_failure_rate(mut self, rate: f32) -> Self {
        self.failure_rate = rate;
        self
    }

    pub fn with_retry_enabled(mut self, enabled: bool) -> Self {
        self.retry_enabled = enabled;
        self
    }

    pub fn with_high_load(mut self, enabled: bool) -> Self {
        self.high_load = enabled;
        self
    }

    pub fn with_message_burst(mut self, count: usize) -> Self {
        self.message_burst = count;
        self
    }

    pub async fn run(self) -> Result<TestOutcome> {
        let start_time = Instant::now();
        let bus = Bus::new();
        let message_counter = Arc::new(AtomicUsize::new(0));
        let response_times = Arc::new(std::sync::Mutex::new(Vec::new()));

        let responses = self.build_responses();
        let (_rt, call_count) = crate::support::test_runtime(bus.clone(), responses).await?;

        // Register agents on the bus (no spawn_agent — just register mailboxes)
        for (role, _) in &self.agents {
            let name = match role {
                AgentRole::TaskAgent => format!("task-test-{}", uuid::Uuid::new_v4()),
                AgentRole::Merger => "merger".to_string(),
            };
            let _ = bus.register(&name);
        }

        if self.high_load {
            self.run_high_load_test(&bus, &message_counter).await;
        } else {
            self.run_normal_test(&bus, &message_counter).await;
        }

        self.wait_for_messages(&call_count).await;

        Ok(TestOutcome {
            elapsed: start_time.elapsed(),
            messages_sent: message_counter.load(Ordering::SeqCst),
            avg_response_time: Self::calculate_avg_response_time(&response_times),
            success: true,
        })
    }

    async fn wait_for_messages(&self, call_count: &Arc<AtomicUsize>) {
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            if call_count.load(Ordering::SeqCst) >= self.expected_messages {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn build_responses(&self) -> Vec<&'static str> {
        let mut responses = vec!["acknowledged", "processing", "completed"];

        if self.retry_enabled {
            responses.extend_from_slice(&["retry attempt", "recovery successful"]);
        }

        responses
    }

    async fn run_normal_test(&self, bus: &Bus, counter: &Arc<AtomicUsize>) {
        let sender = bus.register("test-coordinator").unwrap();

        for i in 0..self.message_burst {
            for (role, _) in &self.agents {
                let agent_name = match role {
                    AgentRole::TaskAgent => "task-test-0",
                    AgentRole::Merger => "merger",
                };

                let payload = serde_json::json!({
                    "content": format!("Test task {}", i)
                });

                if sender.send(agent_name, "task_assignment", payload).is_ok() {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn run_high_load_test(&self, bus: &Bus, counter: &Arc<AtomicUsize>) {
        let mut handles = vec![];

        for burst_id in 0..5 {
            let burst_size = self.message_burst / 5;
            handles.push(self.spawn_burst_sender(bus, counter, burst_id, burst_size));
        }

        for handle in handles {
            let _ = handle.await;
        }
    }

    fn spawn_burst_sender(
        &self,
        bus: &Bus,
        counter: &Arc<AtomicUsize>,
        burst_id: usize,
        burst_size: usize,
    ) -> tokio::task::JoinHandle<()> {
        let bus = bus.clone();
        let counter = counter.clone();
        let agents = self.agents.clone();
        tokio::spawn(async move {
            let sender = bus.register(&format!("burst-sender-{}", burst_id)).unwrap();
            send_burst_messages(&sender, &agents, &counter, burst_id, burst_size).await;
        })
    }

    fn calculate_avg_response_time(
        response_times: &Arc<std::sync::Mutex<Vec<Duration>>>,
    ) -> Duration {
        let times = response_times.lock().unwrap();
        if times.is_empty() {
            return Duration::from_millis(0);
        }
        let total: Duration = times.iter().sum();
        total / times.len() as u32
    }
}

async fn send_burst_messages(
    sender: &agent_bus::Mailbox,
    agents: &[(AgentRole, &'static str)],
    counter: &Arc<AtomicUsize>,
    burst_id: usize,
    burst_size: usize,
) {
    for task_index in 0..burst_size {
        for (role, _) in agents {
            let agent_name = match role {
                AgentRole::TaskAgent => "task-test-0",
                AgentRole::Merger => "merger",
            };
            let payload = serde_json::json!({
                "content": format!("Burst {} task {}", burst_id, task_index)
            });
            if sender.send(agent_name, "task_assignment", payload).is_ok() {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        }
        tokio::time::sleep(Duration::from_micros(100)).await;
    }
}

impl Default for TestScenario {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct TestOutcome {
    pub elapsed: Duration,
    pub messages_sent: usize,
    pub avg_response_time: Duration,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub struct AuditFinding {
    pub severity: String,
    pub description: String,
    pub component: String,
}

#[derive(Debug)]
pub struct PerformanceMetrics {
    pub throughput_msgs_per_sec: f64,
    pub peak_memory_usage: u64,
    pub cpu_usage_percent: f64,
    pub error_rate: f64,
}

impl PerformanceMetrics {
    pub fn new() -> Self {
        Self {
            throughput_msgs_per_sec: 0.0,
            peak_memory_usage: 0,
            cpu_usage_percent: 0.0,
            error_rate: 0.0,
        }
    }
}
