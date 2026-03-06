mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use agent_bus::Bus;
use agent_orchestrator::agent::{permission_mode_for_role, role_has_tools, Agent};
use agent_orchestrator::bus_tools::bus_tools_for_role;
use agent_orchestrator::runtime::RELIEVE_COOLDOWN;
use agent_orchestrator::types::AgentRole;
use support::{test_config, test_runtime, FakeCompleter};

// ---------------------------------------------------------------------------
// Agent-level tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_processes_initial_task() {
    let bus = Bus::new();
    let mailbox = bus.register("test-agent").unwrap();
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut fake = FakeCompleter::with_texts(vec!["done"]);
    fake.call_count = call_count.clone();

    let config = test_config(AgentRole::Manager, 0, Some("build a thing"));
    let agent = Agent::with_completer(config, mailbox, Box::new(fake));

    let handle = tokio::spawn(agent.run());

    // Brief yield to let agent process initial_task
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Deregister closes the sender, causing recv() to return None
    bus.deregister("test-agent");

    let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(result.is_ok(), "agent should exit after deregister");
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn agent_processes_bus_messages() {
    let bus = Bus::new();
    let agent_mailbox = bus.register("test-agent").unwrap();
    let sender = bus.register("sender").unwrap();
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut fake = FakeCompleter::with_texts(vec!["resp1", "resp2"]);
    fake.call_count = call_count.clone();

    let config = test_config(AgentRole::Developer, 0, None);
    let agent = Agent::with_completer(config, agent_mailbox, Box::new(fake));

    let handle = tokio::spawn(agent.run());

    // Send two messages
    sender
        .send(
            "test-agent",
            "task",
            serde_json::json!({"content": "first task"}),
        )
        .unwrap();
    sender
        .send(
            "test-agent",
            "task",
            serde_json::json!({"content": "second task"}),
        )
        .unwrap();

    // Brief yield so agent processes both
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Deregister the agent to close its receive channel
    bus.deregister("test-agent");

    let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(result.is_ok(), "agent should exit after deregister");
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// Runtime crew management tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runtime_crew_scale_up() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["ok"]).await.unwrap();

    rt.handle_crew_size(3);
    assert_eq!(rt.state.developer_count, 3);

    let registered = bus.list_registered();
    assert!(registered.contains(&"developer-1".to_string()), "{registered:?}");
    assert!(registered.contains(&"developer-2".to_string()), "{registered:?}");
}

#[tokio::test]
async fn runtime_crew_scale_down() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["ok"]).await.unwrap();

    rt.handle_crew_size(3);
    assert_eq!(rt.state.developer_count, 3);

    rt.handle_crew_size(1);
    assert_eq!(rt.state.developer_count, 1);

    // Abort propagation is async — yield briefly
    tokio::time::sleep(Duration::from_millis(50)).await;

    let registered = bus.list_registered();
    assert!(!registered.contains(&"developer-1".to_string()), "{registered:?}");
    assert!(!registered.contains(&"developer-2".to_string()), "{registered:?}");
}

#[tokio::test]
async fn runtime_crew_clamps_bounds() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus, vec!["ok"]).await.unwrap();

    rt.handle_crew_size(10);
    assert_eq!(rt.state.developer_count, 3, "should clamp to max 3");

    rt.handle_crew_size(0);
    assert_eq!(rt.state.developer_count, 1, "should clamp to min 1");
}

// ---------------------------------------------------------------------------
// Runtime RELIEVE tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn runtime_relieve_manager() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["ok"]).await.unwrap();

    // Spawn manager via runtime's own machinery
    rt.spawn_agent(AgentRole::Manager, 0, None).unwrap();
    assert_eq!(rt.state.manager_generation, 0);

    rt.handle_relieve_manager("poor performance").await;

    assert_eq!(rt.state.manager_generation, 1);
    assert!(rt.state.last_relieve.is_some());

    // Replacement manager should be on the bus
    let registered = bus.list_registered();
    assert!(registered.contains(&"manager".to_string()), "{registered:?}");
}

#[tokio::test]
async fn runtime_relieve_cooldown() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["ok"]).await.unwrap();

    rt.spawn_agent(AgentRole::Manager, 0, None).unwrap();

    // First relieve succeeds
    rt.handle_relieve_manager("first").await;
    assert_eq!(rt.state.manager_generation, 1);

    // Second within cooldown is rejected
    rt.handle_relieve_manager("second").await;
    assert_eq!(rt.state.manager_generation, 1, "should be rejected by cooldown");
}

#[tokio::test]
async fn runtime_relieve_after_cooldown() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["ok"]).await.unwrap();

    rt.spawn_agent(AgentRole::Manager, 0, None).unwrap();

    rt.handle_relieve_manager("reason 1").await;
    assert_eq!(rt.state.manager_generation, 1);

    // Backdate last_relieve past cooldown
    rt.state.last_relieve = Some(Instant::now() - RELIEVE_COOLDOWN - Duration::from_secs(1));

    rt.handle_relieve_manager("reason 2").await;
    assert_eq!(rt.state.manager_generation, 2);
}

// ---------------------------------------------------------------------------
// Runtime handle_message dispatch tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handle_message_dispatches_set_crew() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus, vec!["ok"]).await.unwrap();

    let payload = serde_json::json!({"count": 2});
    rt.handle_message("set_crew", &payload, "manager").await;
    assert_eq!(rt.state.developer_count, 2);
}

#[tokio::test]
async fn handle_message_dispatches_relieve() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["ok"]).await.unwrap();

    rt.spawn_agent(AgentRole::Manager, 0, None).unwrap();

    let payload = serde_json::json!({"reason": "not performing"});
    rt.handle_message("relieve_manager", &payload, "auditor").await;
    assert_eq!(rt.state.manager_generation, 1);
}

#[tokio::test]
async fn handle_message_ignores_unknown_kind() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus, vec!["ok"]).await.unwrap();

    let payload = serde_json::json!({});
    rt.handle_message("unknown_kind", &payload, "someone").await;
    assert_eq!(rt.state.developer_count, 1);
    assert_eq!(rt.state.manager_generation, 0);
}

// ---------------------------------------------------------------------------
// Task event tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn task_created_spawns_validation() {
    // With the external architect, task_created spawns a background validation task.
    // Since the daemon isn't running in tests, the validation errors and auto-approves.
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus, vec!["ok"]).await.unwrap();

    // Create a real task so the validation can look it up
    let db = rt.db();
    let task = db.create_task("test task", Some("description"), 1, "test").await.unwrap();

    let payload = serde_json::json!({"task_id": task.id});
    rt.handle_message("task_created", &payload, "manager").await;

    // Give the background validation task time to run and auto-approve
    tokio::time::sleep(Duration::from_millis(200)).await;

    let t = db.get_task(&task.id).await.unwrap();
    assert_eq!(t.status, "ready", "task should be auto-approved when daemon is unavailable");
}

#[tokio::test]
async fn ready_task_with_stale_assignee_gets_dispatched_after_watchdog() {
    let bus = Bus::new();
    let mut dev_mbox = bus.register("developer-0").unwrap();
    let (mut rt, _) = test_runtime(bus, vec!["ok"]).await.unwrap();

    // Create a task, set it to ready with a stale assignee
    let db = rt.db();
    let task = db.create_task("test task", Some("do something"), 1, "test").await.unwrap();
    let updates = llm_tasks::db::TaskUpdates {
        status: Some("ready"),
        assignee: Some("ghost-dev"),
        ..Default::default()
    };
    db.update_task(&task.id, updates, "test").await.unwrap();

    // Register a fake developer-0 handle so dispatch can find it
    // (developer-0 mailbox was already registered on the bus above)
    rt.insert_fake_handle("developer-0");

    // Dispatch alone won't find it — ready_tasks() filters out tasks with assignees
    let payload = serde_json::json!({"task_id": task.id});
    rt.handle_message("task_ready", &payload, "runtime").await;
    assert!(dev_mbox.try_recv().is_none(), "stale assignee must block dispatch");

    // Verify the assignee was NOT cleared by dispatch
    let t = db.get_task(&task.id).await.unwrap();
    assert_eq!(t.assignee.as_deref(), Some("ghost-dev"), "dispatch must not clear assignee");

    // Now run the watchdog explicitly — it should clear the stale assignee and dispatch
    rt.run_watchdog_and_dispatch().await;

    let t = db.get_task(&task.id).await.unwrap();
    assert!(t.assignee.as_deref() != Some("ghost-dev"), "watchdog must clear stale assignee");

    let msg = dev_mbox.try_recv();
    assert!(msg.is_some(), "developer-0 must receive task after watchdog clears assignee");
    assert_eq!(msg.unwrap().kind, "task_assignment");
}

// ---------------------------------------------------------------------------
// Tool restriction tests
// ---------------------------------------------------------------------------

/// Helper: extract sorted tool names from a ToolSet.
fn tool_names(set: &llm_sdk::tools::ToolSet) -> Vec<String> {
    let mut names: Vec<String> = set.definitions().iter().map(|d| d.name.clone()).collect();
    names.sort();
    names
}

#[test]
fn role_has_tools_only_for_developer_and_merger() {
    assert!(role_has_tools(AgentRole::Developer));
    assert!(role_has_tools(AgentRole::Merger));
    assert!(!role_has_tools(AgentRole::Manager));
    assert!(!role_has_tools(AgentRole::Architect));
    assert!(!role_has_tools(AgentRole::Auditor));
}

#[test]
fn permission_modes_match_roles() {
    // All agents use bypassPermissions — bwrap sandbox is the security boundary.
    assert_eq!(permission_mode_for_role(AgentRole::Developer), "bypassPermissions");
    assert_eq!(permission_mode_for_role(AgentRole::Merger), "bypassPermissions");
    assert_eq!(permission_mode_for_role(AgentRole::Manager), "bypassPermissions");
    assert_eq!(permission_mode_for_role(AgentRole::Architect), "bypassPermissions");
    assert_eq!(permission_mode_for_role(AgentRole::Auditor), "bypassPermissions");
}

#[test]
fn disallowed_tools_blocks_file_and_bash_for_non_devs() {
    // Non-developer agents must not have file editing or bash tools.
    // They communicate exclusively via MCP orchestrator tools.
    assert!(!role_has_tools(AgentRole::Manager));
    assert!(!role_has_tools(AgentRole::Architect));
    assert!(!role_has_tools(AgentRole::Auditor));
    assert!(role_has_tools(AgentRole::Developer));
    assert!(role_has_tools(AgentRole::Merger));
}

#[test]
fn bus_tools_manager_gets_send_message_set_crew_and_goal_complete() {
    let bus = Bus::new();
    let mailbox = std::sync::Arc::new(bus.register("test-mgr").unwrap());
    let set = bus_tools_for_role(AgentRole::Manager, mailbox);
    let names = tool_names(&set);
    assert_eq!(names, vec!["goal_complete", "send_message", "set_crew"]);
}

#[test]
fn bus_tools_architect_gets_send_message_only() {
    let bus = Bus::new();
    let mailbox = std::sync::Arc::new(bus.register("test-arch").unwrap());
    let set = bus_tools_for_role(AgentRole::Architect, mailbox);
    let names = tool_names(&set);
    assert_eq!(names, vec!["send_message"]);
}

#[test]
fn bus_tools_developer_gets_send_message_and_merge_request() {
    let bus = Bus::new();
    let mailbox = std::sync::Arc::new(bus.register("test-dev").unwrap());
    let set = bus_tools_for_role(AgentRole::Developer, mailbox);
    let names = tool_names(&set);
    assert_eq!(names, vec!["merge_request", "send_message"]);
}

#[test]
fn bus_tools_auditor_gets_send_message_relieve_and_report() {
    let bus = Bus::new();
    let mailbox = std::sync::Arc::new(bus.register("test-aud").unwrap());
    let set = bus_tools_for_role(AgentRole::Auditor, mailbox);
    let names = tool_names(&set);
    assert_eq!(names, vec!["relieve_manager", "report", "send_message"]);
}

#[test]
fn bus_tools_merger_gets_send_message_only() {
    let bus = Bus::new();
    let mailbox = std::sync::Arc::new(bus.register("test-merger").unwrap());
    let set = bus_tools_for_role(AgentRole::Merger, mailbox);
    let names = tool_names(&set);
    assert_eq!(names, vec!["send_message"]);
}

// ---------------------------------------------------------------------------
// Full message flow test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn full_message_flow() {
    let bus = Bus::new();
    let (mut rt, calls) =
        test_runtime(bus.clone(), vec!["task planned"]).await.unwrap();

    // Spawn manager with initial task (uses runtime's factory)
    rt.spawn_agent(AgentRole::Manager, 0, Some("build login".into()))
        .unwrap();

    // Let manager process initial_task
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Manager processed initial_task (1 call)
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "expected at least 1 completion, got {}",
        calls.load(Ordering::SeqCst)
    );
}
