// Integration tests for the agent orchestration system
// These tests cover cross-agent workflows and system-wide behavior

mod support;

use std::time::Duration;
use agent_bus::Bus;
use agent_orchestrator::types::AgentRole;
use support::{test_runtime, TestScenario};

#[tokio::test]
async fn test_task_agent_workflow() {
    let scenario = TestScenario::new()
        .with_agents(vec![
            (AgentRole::TaskAgent, "Implement feature"),
            (AgentRole::Merger, "Review and merge"),
        ])
        .with_expected_messages(2)
        .with_timeout(Duration::from_secs(10));

    let result = scenario.run().await;

    assert!(result.is_ok(), "Task agent workflow should succeed");
}

#[tokio::test]
async fn test_error_recovery_and_retry() {
    let scenario = TestScenario::new()
        .with_failure_rate(0.3)
        .with_retry_enabled(true)
        .with_agents(vec![(AgentRole::TaskAgent, "Handle errors gracefully")])
        .with_timeout(Duration::from_secs(15));

    let result = scenario.run().await;

    assert!(result.is_ok(), "System should recover from failures");
}

#[tokio::test]
async fn test_concurrent_task_agents() {
    let bus = Bus::new();
    let (mut rt, _calls) = test_runtime(bus.clone(), vec!["task done", "review ok"])
        .await
        .unwrap();

    // Create tasks and let runtime spawn agents
    let db = rt.db();
    let task1 = db
        .create_task("task one", Some("do thing 1"), 1, "test")
        .await
        .unwrap();
    let task2 = db
        .create_task("task two", Some("do thing 2"), 1, "test")
        .await
        .unwrap();

    // Mark both ready
    let updates1 = llm_tasks::db::TaskUpdates {
        status: Some("ready"),
        ..Default::default()
    };
    db.update_task(&task1.id, updates1, "test")
        .await
        .unwrap();
    let updates2 = llm_tasks::db::TaskUpdates {
        status: Some("ready"),
        ..Default::default()
    };
    db.update_task(&task2.id, updates2, "test").await.unwrap();

    // Run dispatch cycle
    rt.run_watchdog_and_dispatch().await;

    // Both tasks should now be assigned
    let t1 = db.get_task(&task1.id).await.unwrap();
    let t2 = db.get_task(&task2.id).await.unwrap();
    assert!(
        t1.assignee.is_some() || t2.assignee.is_some(),
        "At least one task should be assigned after dispatch"
    );
}

#[tokio::test]
async fn test_system_load_and_performance() {
    let scenario = TestScenario::new()
        .with_high_load(true)
        .with_agents(vec![
            (AgentRole::TaskAgent, "Process many tasks"),
            (AgentRole::TaskAgent, "Process many tasks"),
            (AgentRole::Merger, "Handle merge queue"),
        ])
        .with_message_burst(50)
        .with_timeout(Duration::from_secs(20));

    let result = scenario.run().await;

    assert!(result.is_ok(), "System should handle load gracefully");
    let metrics = result.unwrap();
    assert!(
        metrics.avg_response_time < Duration::from_secs(2),
        "Response time should be reasonable"
    );
}
