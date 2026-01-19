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
User → Manager → Architect → Developer
           ↑          ↓           ↓
           ←──────────←───────────←

       Scorer (observes all, no decision power)
```

- **Manager**: Receives user requests, breaks into tasks, tracks progress
- **Architect**: Reviews task approaches for simplicity/safety, approves or rejects
- **Developer**: Implements approved tasks, reports completion or blockers
- **Scorer**: Observes progress, evaluates direction (no decision power)

### Communication Protocol

Agents communicate via structured output lines that get parsed and routed:

| Prefix | From | To | Purpose |
|--------|------|-----|---------|
| `TASK:` | Manager | Architect | New task for review |
| `APPROVED:` | Architect | Developer | Task approved, start work |
| `REJECTED:` | Architect | Manager | Approach rejected |
| `COMPLETE:` | Developer | Manager | Task finished |
| `BLOCKED:` | Developer | Manager | Task stuck, needs help |
| `INTERRUPT:` | Architect | Developer | Stop current work |
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
│   └── agent.rs        # AgentRole enum
├── agent.rs            # Agent runtime (backend + transport)
└── main.rs             # CLI entrypoint
```

### Key Abstractions

**AgentBackend trait** (`backend/mod.rs`):
- Abstracts AI provider (Claude, Gemini, Codex)
- `spawn(prompt, working_dir, session_id)` → handle + output stream
- Converts provider-specific output to generic `AgentOutput`

**Transport** (`transport/`):
- Unix sockets at `/tmp/claude/orchestrator/{role}.sock`
- `SO_PEERCRED` for same-user verification
- Length-prefixed JSON messages

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
├── developer.sock
└── scorer.sock
```
