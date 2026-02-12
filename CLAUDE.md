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

This is a multi-agent orchestration system that coordinates AI coding assistants (Claude, Gemini, Codex) through Unix sockets with peercred authentication.

### Agent Roles and Message Flow

```
User → Manager → Architect → Developer-0..2
           ↑          ↓            ↓
           ←──────────←────────────←

       Scorer (observes all, can RELIEVE manager)
       Runtime (spawns/kills agents, maintains state)
```

- **Manager**: Receives user requests, breaks into tasks, decides crew size (1-3 devs)
- **Architect**: Reviews task approaches for simplicity/safety, approves with developer target
- **Developer-N**: Implements approved tasks, reports completion or blockers (N = 0-2)
- **Scorer**: Observes progress, evaluates direction, can fire manager via RELIEVE
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
| `RELIEVE:` | Scorer | Runtime | Fire manager, spawn replacement |
| `EVALUATION:` | Scorer | (logged) | Progress assessment |
| `OBSERVATION:` | Scorer | (logged) | Issue noticed |

### Module Structure

```
src/
├── backend/
│   ├── mod.rs          # AgentBackend trait (provider-agnostic)
│   └── claude.rs       # Claude Code implementation (stream-json)
├── transport/
│   ├── mod.rs
│   ├── message.rs      # Wire protocol (length-prefixed JSON)
│   └── unix.rs         # Unix sockets + SO_PEERCRED
├── types/
│   └── agent.rs        # AgentRole, AgentId (role + index)
├── agent.rs            # Agent runtime, ParsedOutput, output parsing
├── runtime.rs          # OrchestratorRuntime, RuntimeCommand, state management
└── main.rs             # CLI entrypoint
```

### Key Abstractions

**AgentBackend trait** (`backend/mod.rs`):
- Abstracts AI provider (Claude, Gemini, Codex)
- `spawn(prompt, working_dir, session_id)` → handle + output stream
- Converts provider-specific output to generic `AgentOutput`

**Transport** (`transport/`):
- Unix sockets at `/tmp/claude/orchestrator/{agent_id}.sock`
- Singletons: `manager.sock`, `architect.sock`, `scorer.sock`
- Developers: `developer-0.sock`, `developer-1.sock`, `developer-2.sock`
- `SO_PEERCRED` for same-user verification
- Length-prefixed JSON messages

**OrchestratorRuntime** (`runtime.rs`):
- Spawns/tracks agent processes via `HashMap<AgentId, JoinHandle>`
- Processes `RuntimeCommand`s from agents via mpsc channel
- `CREW:` commands dynamically spawn/kill developer agents (1-3)
- `RELIEVE:` fires manager, spawns replacement with state briefing (60s cooldown)
- Maintains `task_log` for manager briefings on replacement

### Adding a New Backend

1. Create `src/backend/gemini.rs` (or codex.rs)
2. Implement `AgentBackend` trait
3. Convert provider output to `AgentOutput` enum
4. Add to `src/backend/mod.rs` exports

```rust
// src/backend/gemini.rs
pub struct GeminiBackend {
    api_key: String,
    model: String,  // e.g. "gemini-2.5-flash"
    client: reqwest::Client,
}

#[async_trait]
impl AgentBackend for GeminiBackend {
    fn name(&self) -> &'static str { "gemini" }

    async fn spawn(
        &self,
        prompt: &str,
        _working_dir: &str,  // Gemini API doesn't have filesystem access
        _session_id: Option<String>,
    ) -> Result<(Box<dyn AgentHandle>, Receiver<AgentOutput>)> {
        let (tx, rx) = mpsc::channel(256);

        // POST https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse&key={key}
        // Body: {"contents":[{"parts":[{"text":"prompt"}]}]}
        // Response: SSE chunks with candidates[0].content.parts[0].text
    }
}
```

Note: Gemini API is text-only (no filesystem/tool access). For coding tasks, you'd need to
implement tool calling yourself or use a wrapper that provides code execution.

## Usage

```bash
# Run a single agent
agent-orchestrator agent developer .

# Run all four agents
agent-orchestrator orchestrate ~/project

# Send message to manager
agent-orchestrator send manager "Add login button"

# Check socket status
agent-orchestrator status
```

## Socket Layout

```
/tmp/claude/orchestrator/
├── manager.sock
├── architect.sock
├── scorer.sock
├── developer-0.sock      # Always present
├── developer-1.sock      # When CREW >= 2
└── developer-2.sock      # When CREW == 3
```
