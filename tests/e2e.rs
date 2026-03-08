mod support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_bus::Bus;
use agent_orchestrator::agent::{permission_mode_for_role, role_has_tools, Agent};
use agent_orchestrator::bus_tools::bus_tools_for_role;
use agent_orchestrator::types::AgentRole;
use support::{test_config, test_runtime, FakeCompleter};

// ---------------------------------------------------------------------------
// Agent-level tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn agent_processes_bus_messages() {
    let bus = Bus::new();
    let config = test_config(AgentRole::TaskAgent, 0, None);
    let bus_name = config.agent_id.bus_name();
    let agent_mailbox = bus.register(&bus_name).unwrap();
    let sender = bus.register("sender").unwrap();
    let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut fake = FakeCompleter::with_texts(vec!["resp1", "resp2"]);
    fake.call_count = call_count.clone();

    let agent = Agent::with_completer(config, agent_mailbox, Box::new(fake));

    let bus_name_clone = bus_name.clone();
    let handle = tokio::spawn(agent.run());

    sender
        .send(&bus_name_clone, "task", serde_json::json!({"content": "first task"}))
        .unwrap();
    sender
        .send(&bus_name_clone, "task", serde_json::json!({"content": "second task"}))
        .unwrap();

    tokio::time::sleep(Duration::from_millis(50)).await;
    bus.deregister(&bus_name_clone);

    let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(result.is_ok(), "agent should exit after deregister");
    assert_eq!(call_count.load(Ordering::SeqCst), 2);
}

// ---------------------------------------------------------------------------
// Tool restriction tests
// ---------------------------------------------------------------------------

fn tool_names(set: &llm_sdk::tools::ToolSet) -> Vec<String> {
    let mut names: Vec<String> = set.definitions().iter().map(|d| d.name.clone()).collect();
    names.sort();
    names
}

#[test]
fn role_has_tools_for_task_agent_and_merger() {
    assert!(role_has_tools(AgentRole::TaskAgent));
    assert!(role_has_tools(AgentRole::Merger));
}

#[test]
fn permission_modes_match_roles() {
    assert_eq!(permission_mode_for_role(AgentRole::TaskAgent), "bypassPermissions");
    assert_eq!(permission_mode_for_role(AgentRole::Merger), "bypassPermissions");
}

#[test]
fn bus_tools_task_agent_gets_send_message_only() {
    let bus = Bus::new();
    let mailbox = std::sync::Arc::new(bus.register("test-agent").unwrap());
    let set = bus_tools_for_role(AgentRole::TaskAgent, mailbox);
    let names = tool_names(&set);
    assert_eq!(names, vec!["send_message"]);
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
// Task event tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn task_created_spawns_validation() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus, vec!["ok"]).await.unwrap();

    let db = rt.db();
    let task = db.create_task("test task", Some("description"), 1, "test").await.unwrap();

    let payload = serde_json::json!({"task_id": task.id});
    rt.handle_message("task_created", &payload, "external").await;

    // Give the background validation task time to run and auto-approve
    tokio::time::sleep(Duration::from_millis(200)).await;

    let t = db.get_task(&task.id).await.unwrap();
    assert_eq!(t.status, "ready", "task should be auto-approved when daemon is unavailable");
}

#[tokio::test]
async fn ready_task_dispatches_after_watchdog_clears_stale_assignee() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus.clone(), vec!["ok"]).await.unwrap();

    let db = rt.db();
    let task = db.create_task("test task", Some("do something"), 1, "test").await.unwrap();
    let updates = llm_tasks::db::TaskUpdates {
        status: Some("ready"),
        assignee: Some("ghost-agent"),
        ..Default::default()
    };
    db.update_task(&task.id, updates, "test").await.unwrap();

    // Watchdog should clear the stale assignee
    rt.run_watchdog_and_dispatch().await;

    let t = db.get_task(&task.id).await.unwrap();
    assert!(t.assignee.as_deref() != Some("ghost-agent"), "watchdog must clear stale assignee");
}

#[tokio::test]
async fn handle_message_ignores_unknown_kind() {
    let bus = Bus::new();
    let (mut rt, _) = test_runtime(bus, vec!["ok"]).await.unwrap();

    let payload = serde_json::json!({});
    let should_exit = rt.handle_message("unknown_kind", &payload, "someone").await;
    assert!(!should_exit);
}
