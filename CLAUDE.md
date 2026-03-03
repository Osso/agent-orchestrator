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

Multi-agent orchestration system that coordinates AI coding assistants via an in-process message bus. Each agent runs Claude Code CLI through `llm-sdk` and communicates with peers through `agent-bus`.

### Dependencies (local crates)

- **llm-sdk** (`../../lib/llm-sdk`) — Claude CLI backend (`Claude` builder) + `SessionStore`/`Session` for persistent session lifecycle
- **agent-bus** (`../agent-bus`) — In-process message bus with named mailboxes
- **llm-tasks** (`../llm-tasks`) — Task persistence via Turso/SQLite

### Agent Roles and Message Flow

```
User → Manager → Architect → Developer-0..2
           ↑          ↓            ↓
           ←──────────←────────────←

       Auditor (patrol timer every 10min, can RELIEVE manager)
       Runtime (spawns/kills agents, maintains state)
```

- **Manager**: Receives user requests, breaks into tasks, decides crew size (1-3 devs)
- **Architect**: Reviews task approaches for simplicity/safety, approves with developer target
- **Developer-N**: Implements approved tasks, reports completion or blockers (N = 0-2)
- **Auditor**: Wakes every 10 minutes via patrol timer, evaluates task snapshot, can fire manager via RELIEVE
- **Runtime**: Spawns/kills agents, handles CREW/RELIEVE commands, maintains task log

### Communication Protocol

Agents communicate via structured output lines that get parsed and routed:

| Prefix | From | To | Purpose |
|--------|------|-----|---------|
| `TASK:` | Manager | Architect | New task for review |
| `APPROVED: developer-N` | Architect | Developer-N | Task approved, start work |
| `REJECTED:` | Architect | Manager | Approach rejected |
| `COMPLETE:` | Developer | Manager | Task finished |
| `BLOCKED:` | Developer | Manager | Task stuck, needs help |
| `INTERRUPT:` | Architect | Developer | Stop current work |
| `CREW: N` | Manager | Runtime | Set developer count (1-3) |
| `RELIEVE:` | Auditor | Runtime | Fire manager, spawn replacement |
| `EVALUATION:` | Auditor | (logged) | Progress assessment |
| `OBSERVATION:` | Auditor | (logged) | Issue noticed |

### Module Structure

```
src/
├── types/
│   ├── mod.rs
│   └── agent.rs        # AgentRole, AgentId (role + index)
├── agent.rs            # Agent struct, output parsing, message routing
├── runtime.rs          # OrchestratorRuntime, agent lifecycle, state
└── main.rs             # CLI entrypoint (run/orchestrate/send)
```

### Key Abstractions

**Agent** (`agent.rs`):
- Holds a `Session` (from llm-sdk) and a base `Claude` instance
- `session.complete(&base_claude, content)` handles resume, expiry, retry
- Parses structured output lines and routes via `agent-bus` mailbox
- Base Claude is configured per-role (permission mode, working dir)

**Session Persistence** (via `llm-sdk::session`):
- `SessionStore::new("agent-orchestrator", project)` → `~/.local/share/agent-orchestrator/{project}/sessions.json`
- Each agent gets a `Session` keyed by bus name (e.g. "manager", "developer-0")
- Sessions resume across restarts; `SessionExpired` triggers automatic retry with fresh session
- On manager RELIEVE, session is removed via `store.remove("manager")` so replacement starts fresh

**OrchestratorRuntime** (`runtime.rs`):
- Owns the `Bus`, `SessionStore`, and `Database`
- Spawns agents with `AgentConfig` + `Session` from the store
- `CREW:` commands dynamically spawn/kill developer agents (1-3)
- `RELIEVE:` fires manager, clears session, spawns replacement with state briefing (60s cooldown)
- Records task events to llm-tasks database

**Output Parsing** (`agent.rs`):
- Section-based parser: scans for recognized prefixes, collects multi-line content until next prefix
- Strips markdown bold (`**TASK:**` → `TASK:`) before matching
- Routes via table: prefix → target role + message kind
- Single-line prefixes: `CREW:`, `RELIEVE:` (no continuation lines)

## Usage

```bash
# Run agents on a task
agent-orchestrator run ~/my-project "Add login button"

# Start agents, wait for messages
agent-orchestrator orchestrate ~/my-project

# Options
agent-orchestrator --db /path/to/tasks.db run ~/project "task"
```
