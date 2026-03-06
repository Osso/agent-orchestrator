# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
cargo build                    # Build debug
cargo build --release          # Build release
cargo run -- --help            # Show usage
cargo clippy                   # Lint
```

## Architecture

Multi-agent orchestration system that coordinates AI coding assistants via an in-process message bus. Each agent runs Claude Code CLI through `llm-sdk` and communicates with peers through `agent-bus`. Task coordination happens through a shared task database (`llm-tasks`).

### Dependencies (local crates)

- **llm-sdk** (`../../lib/llm-sdk`) — Claude CLI backend (`Claude` builder) + `SessionStore`/`Session` for persistent session lifecycle
- **agent-bus** (`../agent-bus`) — In-process message bus with named mailboxes
- **llm-tasks** (`../llm-tasks`) — Task persistence via Turso/SQLite

### Task-Queue Dispatch Model

The runtime is the central state machine. Agents signal intent via tools, the runtime handles all status transitions and dispatch.

```
User → Manager (creates tasks) → DB [pending]
                                   ↓
       Architect (reviews)      → DB [ready]
                                   ↓
       Runtime (auto-dispatches) → Developer [in_progress]
                                   ↓
       Developer (signals done) → DB [in_review]
                                   ↓
       Architect (validates)    → DB [done]
```

**Task statuses**: `pending` → `ready` → `in_progress` → `in_review` → `done` (or `needs_info` from developer)

### Agent Roles

- **Manager**: Creates tasks via `create_task` tool. Never messages developers directly.
- **Architect**: Reviews pending tasks (`approve_task`), validates completions (`complete_task`/`reject_completion`).
- **Developer-N**: Receives auto-dispatched tasks, implements them, signals completion via bus messages.
- **Auditor**: Wakes every 10 minutes, evaluates task snapshot, can fire manager via RELIEVE.
- **Runtime**: State machine — dispatches ready tasks to idle developers, handles all DB transitions.

### MCP Tools (via relay)

| Tool | Roles | Purpose |
|------|-------|---------|
| `create_task` | Manager | Create task (pending) |
| `list_tasks` | All | Query task DB |
| `approve_task` | Architect | pending → ready |
| `complete_task` | Architect | in_review → done |
| `reject_completion` | Architect | in_review → ready (re-dispatch) |
| `send_message` | All | Direct bus messages |
| `set_crew` | Manager | Set developer count (1-3) |
| `goal_complete` | Manager | Trigger shutdown |
| `merge_request` | Developer | Request branch merge |
| `relieve_manager` | Auditor | Fire manager |
| `report` | Auditor | Submit evaluation |

### Module Structure

```
src/
├── types/
│   ├── mod.rs
│   └── agent.rs        # AgentRole, AgentId (role + index)
├── agent.rs            # Agent struct, completer abstraction, message handling
├── runtime.rs          # OrchestratorRuntime, agent lifecycle, state
├── dispatch.rs         # Dispatcher: task→developer matching, DB transitions
├── task_tools.rs       # Task DB tool handlers (create, approve, complete, etc.)
├── relay.rs            # Unix socket relay: MCP subprocess → bus + DB
├── mcp.rs              # MCP stdio server (spawned by Claude CLI)
├── bus_tools.rs        # ToolDef impls for OpenRouter agents (bus-direct)
├── control.rs          # Control socket for external commands
├── worktree.rs         # Git worktree management for developers
└── main.rs             # CLI entrypoint (run/orchestrate/send)
```

### Key Abstractions

**Dispatcher** (`dispatch.rs`):
- Tracks developer→task assignments
- `try_dispatch()`: matches ready tasks to idle developers, claims via DB, sends task_assignment
- `handle_dev_complete()`: transitions task to in_review, notifies architect
- `handle_dev_needs_info()`: transitions to needs_info, notifies manager

**Agent** (`agent.rs`):
- Holds a `Completer` (Session+Claude or OpenRouter) and a bus `Mailbox`
- On task_assignment: resets session, processes prompt, auto-reports completion/blocked to runtime
- Base Claude is configured per-role (permission mode, working dir, sandbox)

**OrchestratorRuntime** (`runtime.rs`):
- Owns the `Bus`, `SessionStore`, `Database` (Arc-shared with relay), and `Dispatcher`
- Spawns agents with `AgentConfig` + `Session` from the store
- Command loop handles bus messages and delegates to dispatcher for task events

## Usage

```bash
# Run as daemon (reads project config)
agent-orchestrator daemon

# Send a message to a running agent
agent-orchestrator send --project my-project manager "Add a login button"

# Notify runtime about a new task
agent-orchestrator notify --project my-project <task-id>
```
