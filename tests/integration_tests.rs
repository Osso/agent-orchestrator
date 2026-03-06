// Integration tests for the agent orchestration system
// These tests cover cross-agent workflows and system-wide behavior

mod support;

use std::time::Duration;
use agent_bus::Bus;
use agent_orchestrator::types::AgentRole;
use support::{test_runtime, TestScenario};

#[tokio::test]
async fn test_complete_development_workflow() {
    let scenario = TestScenario::new()
        .with_agents(vec![
            (AgentRole::Manager, "Plan and coordinate task"),
            (AgentRole::Developer, "Implement feature"),
            (AgentRole::Merger, "Review and merge"),
        ])
        .with_expected_messages(5)
        .with_timeout(Duration::from_secs(10));

    let result = scenario.run().await;
    
    assert!(result.is_ok(), "Complete workflow should succeed");
    assert!(result.unwrap().messages_sent >= 2, "Should have cross-agent communication");
}

#[tokio::test]
async fn test_error_recovery_and_retry() {
    let scenario = TestScenario::new()
        .with_failure_rate(0.3) // 30% of operations fail initially
        .with_retry_enabled(true)
        .with_agents(vec![(AgentRole::Developer, "Handle errors gracefully")])
        .with_timeout(Duration::from_secs(15));

    let result = scenario.run().await;
    
    assert!(result.is_ok(), "System should recover from failures");
}

#[tokio::test]
async fn test_concurrent_developer_coordination() {
    let bus = Bus::new();
    let (mut rt, _calls) = test_runtime(bus.clone(), vec!["task done", "review ok"]).await.unwrap();

    // Spawn developer-0 (handle_crew_size only adds indices above current count)
    rt.spawn_agent(AgentRole::Developer, 0, None).unwrap();

    // Scale up to 3 developers (adds developer-1 and developer-2)
    rt.handle_crew_size(3);

    // Spawn manager to coordinate
    rt.spawn_agent(AgentRole::Manager, 0, Some("Coordinate parallel tasks".into())).unwrap();

    // Brief simulation of parallel work
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify all developers are registered and active
    let registered = bus.list_registered();
    assert!(registered.contains(&"developer-0".to_string()));
    assert!(registered.contains(&"developer-1".to_string()));
    assert!(registered.contains(&"developer-2".to_string()));
    assert!(registered.contains(&"manager".to_string()));
}

#[tokio::test]
async fn test_audit_and_quality_control() {
    let scenario = TestScenario::new()
        .with_agents(vec![
            (AgentRole::Developer, "Implement with issues"),
            (AgentRole::Auditor, "Find and report issues"),
            (AgentRole::Manager, "Handle audit findings"),
        ])
        .with_audit_checks_enabled(true)
        .with_quality_gates(vec!["code_review", "security_check"])
        .with_timeout(Duration::from_secs(8));

    let result = scenario.run().await;
    
    assert!(result.is_ok(), "Audit workflow should complete");
    let outcome = result.unwrap();
    assert!(outcome.audit_findings.len() > 0, "Auditor should find issues");
}

#[tokio::test]
async fn test_manager_relief_and_handover() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["handover complete"]).await.unwrap();

    // Start with initial manager
    rt.spawn_agent(AgentRole::Manager, 0, Some("Initial work".into())).unwrap();
    rt.spawn_agent(AgentRole::Auditor, 0, None).unwrap();

    assert_eq!(rt.state.manager_generation, 0);

    // Auditor relieves manager
    rt.handle_relieve_manager("Performance issues").await;
    
    assert_eq!(rt.state.manager_generation, 1, "New manager should be appointed");
    
    // New manager should be on the bus
    let registered = bus.list_registered();
    assert!(registered.contains(&"manager".to_string()));
}

#[tokio::test]
async fn test_system_load_and_performance() {
    let scenario = TestScenario::new()
        .with_high_load(true)
        .with_agents(vec![
            (AgentRole::Manager, "Handle high load"),
            (AgentRole::Developer, "Process many tasks"),
            (AgentRole::Developer, "Process many tasks"),
            (AgentRole::Merger, "Handle merge queue"),
        ])
        .with_message_burst(50) // Send 50 messages rapidly
        .with_timeout(Duration::from_secs(20));

    let result = scenario.run().await;
    
    assert!(result.is_ok(), "System should handle load gracefully");
    let metrics = result.unwrap();
    assert!(metrics.avg_response_time < Duration::from_secs(2), "Response time should be reasonable");
}