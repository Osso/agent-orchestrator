// Unit tests for core components

mod support;

use agent_bus::Bus;
use agent_orchestrator::agent::{permission_mode_for_role, role_has_tools};
use agent_orchestrator::bus_tools::bus_tools_for_role;
use agent_orchestrator::config;
use agent_orchestrator::runtime_support::resolve_sandbox;
use agent_orchestrator::types::{AgentId, AgentRole};
use agent_orchestrator::worktree::link_shared_dependency_dirs;
use std::time::Duration;
use support::{TestAgentBuilder, TestBench, assert_agent_registered, test_config};

// ---------------------------------------------------------------------------
// Agent ID and Role Tests
// ---------------------------------------------------------------------------

#[test]
fn agent_id_formatting() {
    let task_id = AgentId::for_task("lt-abc123");
    assert_eq!(task_id.bus_name(), "task-lt-abc123");
    assert_eq!(task_id.role, AgentRole::TaskAgent);

    let merger_id = AgentId::merger();
    assert_eq!(merger_id.bus_name(), "merger");
    assert_eq!(merger_id.role, AgentRole::Merger);
}

#[test]
fn agent_id_task_uniqueness() {
    let task0 = AgentId::for_task("lt-aaa");
    let task1 = AgentId::for_task("lt-bbb");
    let task0_dup = AgentId::for_task("lt-aaa");

    assert_ne!(task0.bus_name(), task1.bus_name());
    assert_eq!(task0.bus_name(), task0_dup.bus_name());
    assert_eq!(task0.role, task1.role);
}

// ---------------------------------------------------------------------------
// Configuration Tests
// ---------------------------------------------------------------------------

#[test]
fn test_config_creates_valid_configs() {
    let task_config = test_config(AgentRole::TaskAgent, 0, Some("test task"));
    assert!(task_config.agent_id.bus_name().starts_with("task-"));
    assert_eq!(task_config.initial_task, Some("test task".to_string()));

    let merger_config = test_config(AgentRole::Merger, 0, None);
    assert_eq!(merger_config.agent_id.bus_name(), "merger");
    assert_eq!(merger_config.initial_task, None);
}

#[test]
fn agent_config_sandbox_prefix() {
    let config = test_config(AgentRole::TaskAgent, 0, None);
    assert!(config.sandbox_prefix.is_empty());
    assert_eq!(config.working_dir, "/tmp");
}

#[test]
fn ensure_project_registered_persists_new_project() {
    let temp = std::env::temp_dir().join(format!("orch-config-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temp).unwrap();
    let old = std::env::var_os("XDG_CONFIG_HOME");
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", &temp);
    }

    config::ensure_project_registered("gc-phpstan-fixes", "/tmp/gc-phpstan-fixes").unwrap();
    let projects = config::load_config().unwrap();

    assert_eq!(
        projects.get("gc-phpstan-fixes").map(|p| p.dir.as_str()),
        Some("/tmp/gc-phpstan-fixes")
    );

    match old {
        Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
        None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
    }
    let _ = std::fs::remove_dir_all(&temp);
}

// ---------------------------------------------------------------------------
// Tool System Tests
// ---------------------------------------------------------------------------

#[test]
fn tool_permissions_are_role_specific() {
    assert!(role_has_tools(AgentRole::TaskAgent));
    assert!(role_has_tools(AgentRole::Merger));
}

#[test]
fn permission_modes_are_consistent() {
    for role in [AgentRole::TaskAgent, AgentRole::Merger] {
        assert_eq!(permission_mode_for_role(role), "bypassPermissions");
    }
}

#[test]
fn bus_tools_match_role_responsibilities() {
    let bus = Bus::new();

    let task_mailbox = std::sync::Arc::new(bus.register("test-task").unwrap());
    let task_tools = bus_tools_for_role(AgentRole::TaskAgent, task_mailbox);
    let task_names: Vec<String> = task_tools
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(task_names.contains(&"send_message".to_string()));
    assert_eq!(task_names.len(), 1);

    let merger_mailbox = std::sync::Arc::new(bus.register("test-merger").unwrap());
    let merger_tools = bus_tools_for_role(AgentRole::Merger, merger_mailbox);
    let merger_names: Vec<String> = merger_tools
        .definitions()
        .iter()
        .map(|d| d.name.clone())
        .collect();
    assert!(merger_names.contains(&"send_message".to_string()));
    assert_eq!(merger_names.len(), 1);
}

// ---------------------------------------------------------------------------
// Agent Builder Tests
// ---------------------------------------------------------------------------

#[test]
fn test_agent_builder_creates_agents() {
    let bus = Bus::new();

    let agent = TestAgentBuilder::new(AgentRole::TaskAgent)
        .with_responses(vec!["task completed"])
        .with_initial_task("build feature")
        .build(&bus);

    assert!(agent.is_ok(), "Agent creation should succeed");
}

#[test]
fn test_agent_builder_failure_modes() {
    let bus = Bus::new();

    let agent = TestAgentBuilder::new(AgentRole::Merger)
        .should_fail(true)
        .build(&bus);

    assert!(
        agent.is_ok(),
        "Agent creation should succeed even with failure simulation"
    );
    assert_agent_registered(&bus, "merger");
}

// ---------------------------------------------------------------------------
// Performance and Benchmarking Tests
// ---------------------------------------------------------------------------

#[test]
fn test_bench_measures_throughput() {
    let mut bench = TestBench::new();

    for _ in 0..100 {
        bench.record_operation();
        std::thread::sleep(Duration::from_nanos(1000));
    }

    let throughput = bench.throughput();
    assert!(throughput > 0.0, "Throughput should be positive");
    assert!(bench.elapsed() > Duration::ZERO, "Time should have elapsed");
}

// ---------------------------------------------------------------------------
// Bus Integration Tests
// ---------------------------------------------------------------------------

#[test]
fn bus_registration_and_deregistration() {
    let bus = Bus::new();

    let mailbox = bus.register("test-agent").unwrap();
    assert_agent_registered(&bus, "test-agent");

    drop(mailbox);
    std::thread::sleep(Duration::from_millis(1));
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
// Working Directory Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_tool_uses_worktree_cwd_without_sandbox() {
    // When not sandboxed, the ToolSet should set Bash's working_dir to the agent's working_dir.
    // This ensures git commands run in the worktree, not the daemon's cwd.
    let worktree = std::env::temp_dir().join(format!("orch-cwd-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&worktree);
    let marker = worktree.join("cwd-marker.txt");
    std::fs::write(&marker, "found").unwrap();

    let tool_set = llm_sdk::tools::ToolSet::standard_with_cwd(&worktree);
    let call = llm_sdk::tools::ToolCall {
        id: "1".into(),
        name: "Bash".into(),
        arguments: r#"{"command": "cat cwd-marker.txt"}"#.into(),
    };
    let result = tool_set.execute(&call).await;
    assert_eq!(
        result.trim(),
        "found",
        "Bash should run in the worktree dir, got: {result}"
    );

    let _ = std::fs::remove_dir_all(&worktree);
}

#[tokio::test]
async fn bash_tool_pwd_matches_working_dir() {
    let worktree = std::env::temp_dir().join(format!("orch-pwd-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&worktree);

    let tool_set = llm_sdk::tools::ToolSet::standard_with_cwd(&worktree);
    let call = llm_sdk::tools::ToolCall {
        id: "1".into(),
        name: "Bash".into(),
        arguments: r#"{"command": "pwd"}"#.into(),
    };
    let result = tool_set.execute(&call).await;
    let expected = worktree.canonicalize().unwrap_or(worktree.clone());
    assert_eq!(
        result.trim(),
        expected.to_string_lossy(),
        "pwd should match the worktree path"
    );

    let _ = std::fs::remove_dir_all(&worktree);
}

// ---------------------------------------------------------------------------
// Sandbox Resolution Tests
// ---------------------------------------------------------------------------

#[test]
fn task_agent_gets_writable_sandbox_when_worktree_fails() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");
    let worktree_result: Result<PathBuf, anyhow::Error> =
        Err(anyhow::anyhow!("git worktree add failed"));

    let (working_dir, prefix) =
        resolve_sandbox(AgentRole::TaskAgent, &project, worktree_result, true);

    assert_eq!(working_dir, llm_sdk::sandbox::REPO_MOUNT);
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
        "Project dir should be --bind (writable). Got: {:?}",
        prefix
    );
}

#[test]
fn task_agent_gets_worktree_sandbox_when_worktree_succeeds() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");
    let worktree_path = PathBuf::from("/tmp/test-project/.worktrees/task-lt-abc");
    let worktree_result: Result<PathBuf, anyhow::Error> = Ok(worktree_path);

    let (working_dir, prefix) =
        resolve_sandbox(AgentRole::TaskAgent, &project, worktree_result, true);

    assert_eq!(working_dir, llm_sdk::sandbox::REPO_MOUNT);
    let bind_positions: Vec<usize> = prefix
        .iter()
        .enumerate()
        .filter(|(_, s)| s.as_str() == "--bind")
        .map(|(i, _)| i)
        .collect();
    let worktree_bound = bind_positions.iter().any(|&i| {
        prefix.get(i + 1).map(|s| s.as_str()) == Some("/tmp/test-project/.worktrees/task-lt-abc")
    });
    assert!(
        worktree_bound,
        "Worktree should be --bind (writable). Got: {:?}",
        prefix
    );
}

#[test]
fn task_agent_no_sandbox_uses_worktree_path() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");
    let worktree_path = PathBuf::from("/tmp/test-project/.worktrees/task-lt-abc");

    let (working_dir, prefix) =
        resolve_sandbox(AgentRole::TaskAgent, &project, Ok(worktree_path), false);

    assert_eq!(working_dir, "/tmp/test-project/.worktrees/task-lt-abc");
    assert!(prefix.is_empty(), "No sandbox prefix without sandbox");
}

#[test]
fn task_agent_no_sandbox_worktree_fails_uses_project_dir() {
    use std::path::PathBuf;

    let project = PathBuf::from("/tmp/test-project");

    let (working_dir, prefix) = resolve_sandbox(
        AgentRole::TaskAgent,
        &project,
        Err(anyhow::anyhow!("worktree failed")),
        false,
    );

    assert_eq!(working_dir, "/tmp/test-project");
    assert!(prefix.is_empty(), "No sandbox prefix without sandbox");
}

#[cfg(unix)]
#[test]
fn resumed_worktree_links_shared_vendor_dir() {
    let root = std::env::temp_dir().join(format!("orch-worktree-test-{}", uuid::Uuid::new_v4()));
    let project = root.join("project");
    let worktree = project.join(".worktrees").join("task-test");
    let vendor_bin = project.join("vendor").join("bin");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::create_dir_all(&vendor_bin).unwrap();
    std::fs::write(vendor_bin.join("phpstan"), "#!/bin/sh\n").unwrap();

    link_shared_dependency_dirs(&project, &worktree);
    let linked = worktree.join("vendor");
    let meta = std::fs::symlink_metadata(&linked).unwrap();

    assert!(
        meta.file_type().is_symlink(),
        "vendor should be symlinked into resumed worktree"
    );
    assert_eq!(std::fs::read_link(&linked).unwrap(), project.join("vendor"));

    let _ = std::fs::remove_dir_all(&root);
}
