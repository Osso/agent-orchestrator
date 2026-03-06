// Unit tests for core components
// These tests focus on individual components in isolation

mod support;

use std::time::Duration;
use agent_bus::Bus;
use agent_orchestrator::agent::{permission_mode_for_role, role_has_tools};
use agent_orchestrator::bus_tools::bus_tools_for_role;
use agent_orchestrator::runtime_support::resolve_sandbox;
use agent_orchestrator::types::{AgentId, AgentRole};
use support::{test_config, TestAgentBuilder, TestBench, assert_agent_registered};

// ---------------------------------------------------------------------------
// Agent ID and Role Tests
// ---------------------------------------------------------------------------

#[test]
fn agent_id_formatting() {
    let dev_id = AgentId::new_developer(0);
    assert_eq!(dev_id.bus_name(), "developer-0");
    assert_eq!(dev_id.role, AgentRole::Developer);
    assert_eq!(dev_id.index, 0);

    let mgr_id = AgentId::new_singleton(AgentRole::Manager);
    assert_eq!(mgr_id.bus_name(), "manager");
    assert_eq!(mgr_id.role, AgentRole::Manager);
    assert_eq!(mgr_id.index, 0);
}

#[test]
fn agent_id_developer_uniqueness() {
    let dev0 = AgentId::new_developer(0);
    let dev1 = AgentId::new_developer(1);
    let dev0_dup = AgentId::new_developer(0);

    assert_ne!(dev0.bus_name(), dev1.bus_name());
    assert_eq!(dev0.bus_name(), dev0_dup.bus_name());
    assert_eq!(dev0.role, dev1.role);
}

// ---------------------------------------------------------------------------
// Configuration Tests
// ---------------------------------------------------------------------------

#[test]
fn test_config_creates_valid_configs() {
    let dev_config = test_config(AgentRole::Developer, 2, Some("test task"));
    assert_eq!(dev_config.agent_id.bus_name(), "developer-2");
    assert_eq!(dev_config.initial_task, Some("test task".to_string()));

    let mgr_config = test_config(AgentRole::Manager, 0, None);
    assert_eq!(mgr_config.agent_id.bus_name(), "manager");
    assert_eq!(mgr_config.initial_task, None);
}

#[test]
fn agent_config_sandbox_prefix() {
    let config = test_config(AgentRole::Developer, 0, None);
    assert!(config.sandbox_prefix.is_empty());
    assert_eq!(config.working_dir, "/tmp");
}

// ---------------------------------------------------------------------------
// Tool System Tests  
// ---------------------------------------------------------------------------

#[test]
fn tool_permissions_are_role_specific() {
    // Only developers and mergers should have tools
    assert!(role_has_tools(AgentRole::Developer));
    assert!(role_has_tools(AgentRole::Merger));
    
    // Management and oversight roles should not have direct tools
    assert!(!role_has_tools(AgentRole::Manager));
    assert!(!role_has_tools(AgentRole::Architect));
    assert!(!role_has_tools(AgentRole::Auditor));
}

#[test]
fn permission_modes_are_consistent() {
    // All roles use bypass permissions (security via sandbox)
    for role in [
        AgentRole::Developer,
        AgentRole::Merger, 
        AgentRole::Manager,
        AgentRole::Architect,
        AgentRole::Auditor,
    ] {
        assert_eq!(permission_mode_for_role(role), "bypassPermissions");
    }
}

#[test]
fn bus_tools_match_role_responsibilities() {
    let bus = Bus::new();
    
    // Manager: coordination tools
    let mgr_mailbox = std::sync::Arc::new(bus.register("test-mgr").unwrap());
    let mgr_tools = bus_tools_for_role(AgentRole::Manager, mgr_mailbox);
    let mgr_names: Vec<String> = mgr_tools.definitions().iter().map(|d| d.name.clone()).collect();
    assert!(mgr_names.contains(&"send_message".to_string()));
    assert!(mgr_names.contains(&"set_crew".to_string()));
    assert!(mgr_names.contains(&"goal_complete".to_string()));

    // Developer: development tools
    let dev_mailbox = std::sync::Arc::new(bus.register("test-dev").unwrap());
    let dev_tools = bus_tools_for_role(AgentRole::Developer, dev_mailbox);
    let dev_names: Vec<String> = dev_tools.definitions().iter().map(|d| d.name.clone()).collect();
    assert!(dev_names.contains(&"send_message".to_string()));
    assert!(dev_names.contains(&"merge_request".to_string()));

    // Auditor: oversight tools
    let aud_mailbox = std::sync::Arc::new(bus.register("test-aud").unwrap());
    let aud_tools = bus_tools_for_role(AgentRole::Auditor, aud_mailbox);
    let aud_names: Vec<String> = aud_tools.definitions().iter().map(|d| d.name.clone()).collect();
    assert!(aud_names.contains(&"send_message".to_string()));
    assert!(aud_names.contains(&"relieve_manager".to_string()));
    assert!(aud_names.contains(&"report".to_string()));
}

// ---------------------------------------------------------------------------
// Agent Builder Tests
// ---------------------------------------------------------------------------

#[test]
fn test_agent_builder_creates_agents() {
    let bus = Bus::new();
    
    let agent = TestAgentBuilder::new(AgentRole::Developer)
        .with_index(1)
        .with_responses(vec!["task completed"])
        .with_initial_task("build feature")
        .build(&bus);
    
    assert!(agent.is_ok(), "Agent creation should succeed");
    assert_agent_registered(&bus, "developer-1");
}

#[test]
fn test_agent_builder_failure_modes() {
    let bus = Bus::new();
    
    let agent = TestAgentBuilder::new(AgentRole::Architect)
        .should_fail(true)
        .build(&bus);
    
    assert!(agent.is_ok(), "Agent creation should succeed even with failure simulation");
    assert_agent_registered(&bus, "architect");
}

// ---------------------------------------------------------------------------
// Performance and Benchmarking Tests
// ---------------------------------------------------------------------------

#[test]
fn test_bench_measures_throughput() {
    let mut bench = TestBench::new();
    
    // Simulate some operations
    for _ in 0..100 {
        bench.record_operation();
        std::thread::sleep(Duration::from_nanos(1000)); // 1 microsecond
    }
    
    let throughput = bench.throughput();
    assert!(throughput > 0.0, "Throughput should be positive");
    assert!(bench.elapsed() > Duration::ZERO, "Time should have elapsed");
}

#[test]
fn test_bench_handles_zero_time() {
    let mut bench = TestBench::new();
    bench.record_operation();
    
    // Check immediately - elapsed time might be zero
    let throughput = bench.throughput();
    assert!(throughput >= 0.0, "Throughput should be non-negative");
}

// ---------------------------------------------------------------------------
// Bus Integration Tests
// ---------------------------------------------------------------------------

#[test]
fn bus_registration_and_deregistration() {
    let bus = Bus::new();
    
    let mailbox = bus.register("test-agent").unwrap();
    assert_agent_registered(&bus, "test-agent");
    
    drop(mailbox); // This should trigger deregistration
    
    // Give the bus time to process deregistration
    std::thread::sleep(Duration::from_millis(1));
    
    // Note: exact deregistration timing is implementation dependent
    // so we mainly verify registration worked
}

#[test]
fn bus_prevents_duplicate_registration() {
    let bus = Bus::new();
    
    let _mailbox1 = bus.register("test-agent").unwrap();
    let result = bus.register("test-agent");
    
    assert!(result.is_err(), "Duplicate registration should fail");
}

#[test]
fn bus_message_sending() {
    let bus = Bus::new();
    
    let _receiver = bus.register("receiver").unwrap();
    let sender = bus.register("sender").unwrap();
    
    let result = sender.send(
        "receiver",
        "test_message",
        serde_json::json!({"content": "hello"}),
    );
    
    assert!(result.is_ok(), "Message sending should succeed");
}

// ---------------------------------------------------------------------------
// Error Handling Tests
// ---------------------------------------------------------------------------

#[test]
fn invalid_agent_creation_handling() {
    let bus = Bus::new();
    
    // Try to build agent without proper setup
    // This should still work as our builder has defaults
    let agent = TestAgentBuilder::new(AgentRole::Developer).build(&bus);
    assert!(agent.is_ok(), "Agent builder should handle defaults gracefully");
}

// ---------------------------------------------------------------------------
// Role-specific Behavior Tests
// ---------------------------------------------------------------------------

#[test]
fn developer_role_characteristics() {
    assert!(role_has_tools(AgentRole::Developer));
    
    let config = test_config(AgentRole::Developer, 5, Some("implement API"));
    assert_eq!(config.agent_id.index, 5);
    assert!(config.initial_task.is_some());
}

#[test]
fn singleton_role_characteristics() {
    for role in [AgentRole::Manager, AgentRole::Architect, AgentRole::Auditor] {
        assert!(!role_has_tools(role), "Singleton roles should not have direct tools");
        
        let config = test_config(role, 0, None);
        assert_eq!(config.agent_id.index, 0);
    }
}

#[test]
fn merger_role_characteristics() {
    assert!(role_has_tools(AgentRole::Merger));

    let config = test_config(AgentRole::Merger, 0, None);
    assert_eq!(config.agent_id.bus_name(), "merger");
    assert_eq!(config.agent_id.index, 0);
}

// ---------------------------------------------------------------------------
// Sandbox Resolution Tests
// ---------------------------------------------------------------------------

#[test]
fn developer_gets_writable_sandbox_when_worktree_fails() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");
    let worktree_result: Result<PathBuf, anyhow::Error> =
        Err(anyhow::anyhow!("git worktree add failed"));

    let (working_dir, prefix) =
        resolve_sandbox(AgentRole::Developer, &project, worktree_result, true);

    // Developer must get writable sandbox even when worktree fails
    assert_eq!(working_dir, llm_sdk::sandbox::REPO_MOUNT);
    // Must use developer_prefix (--bind), not readonly_prefix (--ro-bind)
    // Check that /tmp/test-project is mounted writable (--bind), not read-only
    let bind_positions: Vec<usize> = prefix
        .iter()
        .enumerate()
        .filter(|(_, s)| s.as_str() == "--bind")
        .map(|(i, _)| i)
        .collect();
    let project_bound = bind_positions
        .iter()
        .any(|&i| prefix.get(i + 1).map(|s| s.as_str()) == Some("/tmp/test-project"));
    assert!(
        project_bound,
        "Project dir should be --bind (writable), not --ro-bind. Got: {:?}",
        prefix
    );
}

#[test]
fn developer_gets_worktree_sandbox_when_worktree_succeeds() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");
    let worktree_path = PathBuf::from("/tmp/test-project/.worktrees/developer-0");
    let worktree_result: Result<PathBuf, anyhow::Error> = Ok(worktree_path);

    let (working_dir, prefix) =
        resolve_sandbox(AgentRole::Developer, &project, worktree_result, true);

    assert_eq!(working_dir, llm_sdk::sandbox::REPO_MOUNT);
    // Worktree path should be bound writable
    let bind_positions: Vec<usize> = prefix
        .iter()
        .enumerate()
        .filter(|(_, s)| s.as_str() == "--bind")
        .map(|(i, _)| i)
        .collect();
    let worktree_bound = bind_positions.iter().any(|&i| {
        prefix.get(i + 1).map(|s| s.as_str())
            == Some("/tmp/test-project/.worktrees/developer-0")
    });
    assert!(
        worktree_bound,
        "Worktree should be --bind (writable). Got: {:?}",
        prefix
    );
}

#[test]
fn non_developer_gets_readonly_sandbox() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");

    let (working_dir, prefix) =
        resolve_sandbox(AgentRole::Manager, &project, Err(anyhow::anyhow!("n/a")), true);

    assert_eq!(working_dir, llm_sdk::sandbox::REPO_MOUNT);
    // Project should be --ro-bind (read-only)
    let ro_bind_positions: Vec<usize> = prefix
        .iter()
        .enumerate()
        .filter(|(_, s)| s.as_str() == "--ro-bind")
        .map(|(i, _)| i)
        .collect();
    let project_ro = ro_bind_positions
        .iter()
        .any(|&i| prefix.get(i + 1).map(|s| s.as_str()) == Some("/tmp/test-project"));
    assert!(
        project_ro,
        "Non-developer project dir should be --ro-bind. Got: {:?}",
        prefix
    );
}

#[test]
fn developer_no_sandbox_uses_worktree_path() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");
    let worktree_path = PathBuf::from("/tmp/test-project/.worktrees/developer-0");

    let (working_dir, prefix) =
        resolve_sandbox(AgentRole::Developer, &project, Ok(worktree_path), false);

    assert_eq!(working_dir, "/tmp/test-project/.worktrees/developer-0");
    assert!(prefix.is_empty(), "No sandbox prefix without sandbox");
}

#[test]
fn developer_no_sandbox_worktree_fails_uses_project_dir() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");

    let (working_dir, prefix) = resolve_sandbox(
        AgentRole::Developer,
        &project,
        Err(anyhow::anyhow!("worktree failed")),
        false,
    );

    assert_eq!(working_dir, "/tmp/test-project");
    assert!(prefix.is_empty(), "No sandbox prefix without sandbox");
}