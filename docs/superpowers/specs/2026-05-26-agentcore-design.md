# agentcore Design

**Date:** 2026-05-26
**Status:** Approved

## Context

Key design problems this spec addresses:

- Agent conflated immutable config with mutable runtime state; `run()` mutated self
- Session handler mixed two unrelated concerns: history loading and event notification
- Lifecycle not enforced by types — invalid state transitions only caught at runtime
- Streaming used `block_in_place` inside async context, fragile and deadlock-prone
- Stringly-typed errors throughout — callers couldn't distinguish error kinds
- Individual tool abstractions leaked into the agent loop unnecessarily
- Unclear terminology: `Block`, `SessionHandler`, `structured_output`

## Scope

The `agentcore` crate provides the agent execution loop and the minimal abstractions it needs:

- Protocol types (`Message`, `ContentPart`, `AgentEvent`) via the `models` crate
- `LlmProvider` trait — provider abstraction, no concrete implementations
- `Toolbox` trait — tool dispatch abstraction, no concrete implementations
- `Agent` — immutable, stateless, drives the agentic loop
- `EventSink` trait — sync event observer for streaming and history
- Typed errors

**Explicitly out of scope for agentcore:**

- Concrete LLM provider implementations → future provider crates
- Typed `Tool` trait, concrete `Toolbox` implementations, MCP bridge → future `agent-tools` crate
- Session persistence, history storage → future `session-store` crate

## Crate layout

```
october/
  fluorite/
    agent.fl        ← Message, ContentPart, Role, Usage
    events.fl       ← AgentEvent
  models/           ← generated from fluorite schemas
  agentcore/
    src/
      lib.rs
      provider.rs   ← LlmProvider trait
      tool.rs       ← Toolbox trait, ToolSpec
      agent.rs      ← Agent, AgentBuilder, AgentInput, RunOutput
      events.rs     ← EventSink trait
      error.rs      ← AgentError, LlmError, ToolCallError
```

---

## Fluorite schemas

### `fluorite/agent.fl`

```
package agent;

enum Role { User, Assistant, Tool }

enum ContentPart {
    Text       { text: String },
    ToolCall   { id: String, name: String, input: Any },
    ToolResult { tool_call_id: String, output: String, is_error: bool },
    Thinking   { text: String },
}

struct Message {
    role:  Role,
    parts: Vec<ContentPart>,
}

struct Usage {
    input_tokens:  u32,
    output_tokens: u32,
}
```

### `fluorite/events.fl`

```
package events;

enum AgentEvent {
    // Message scope — UX creates placeholder on Start, replaces on Complete
    MessageStart       { id: String, role: Role },
    MessageComplete    { id: String, message: Message },

    // Streaming content (within open message scope, UX-only)
    TextChunk          { text: String },
    Thinking           { text: String },
    ToolCallStart      { id: String, name: String },
    ToolCallInputDelta { id: String, delta: String },
    ToolCallInputDone  { id: String },

    // Tool execution (between messages)
    ToolExecuting      { id: String },
    ToolComplete       { id: String, output: String, is_error: bool },

    // Run lifecycle
    RunComplete        { usage: Usage, iterations: u32 },
}
```

### Event protocol

The event sequence for one agent iteration (UserMessage input, one tool call, final response):

```
// AgentInput converted to Message and emitted first
MessageStart       { id: "m0", role: User }
MessageComplete    { id: "m0", message }        ← no streaming for user input

// Provider streams assistant response; agent wraps with Start/Complete
MessageStart       { id: "m1", role: Assistant }
  TextChunk          { text: "..." }            ← emitted by provider, UX display only
  ToolCallStart      { id: "tc1", name: "..." } ← emitted by provider
  ToolCallInputDelta { id: "tc1", delta: "..." }
  ToolCallInputDone  { id: "tc1" }
MessageComplete    { id: "m1", message }        ← emitted by agent; authoritative; replaces chunks

// Agent executes tool; no MessageStart/Complete for tool results
ToolExecuting      { id: "tc1" }
ToolComplete       { id: "tc1", output: "...", is_error: false }

// Second iteration — provider streams final response
MessageStart       { id: "m2", role: Assistant }
  TextChunk          { text: "..." }
MessageComplete    { id: "m2", message }

RunComplete        { usage, iterations: 2 }
```

**History reconstruction:** collect `MessageComplete` events in order. Tool result messages are derived from `ToolComplete` events — each maps to `Message { role: Tool, parts: [ContentPart::ToolResult { tool_call_id, output, is_error }] }`.

**UX replacement:** `MessageStart.id` matches `MessageComplete.id` — the UX creates a placeholder on `MessageStart` and replaces it with the complete message on `MessageComplete`.

**Streaming events are display-only.** Consumers must not reconstruct history from chunks.

---

## Provider abstraction

```rust
// agentcore/src/provider.rs

pub struct CompletionRequest<'a> {
    pub messages:    &'a [Message],
    pub system:      Option<&'a str>,
    pub tools:       &'a [ToolSpec],
    pub tool_choice: ToolChoice,
    pub max_tokens:  Option<u32>,
}

pub struct CompletionResponse {
    pub parts:       Vec<ContentPart>,
    pub stop_reason: StopReason,
    pub usage:       Usage,
}

pub enum StopReason { EndTurn, ToolUse, MaxTokens }

pub enum ToolChoice {
    Auto,
    Any,              // model must call some tool
    Required(String), // model must call this specific tool
}

pub enum LlmError {
    RateLimit { retry_after: Option<Duration> },
    Overloaded,
    ApiError  { status: u16, message: String },
    Network(Box<dyn std::error::Error + Send + Sync>),
}

pub trait LlmProvider: Send + Sync {
    fn model_id(&self) -> &str;
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        events:  &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError>;
}
```

LLM-specific concerns (caching, retry logic, exponential backoff, token budgets) are handled entirely inside concrete provider implementations. `agentcore` is cache-agnostic.

---

## Tool abstraction

```rust
// agentcore/src/tool.rs

pub struct ToolSpec {
    pub name:         String,
    pub description:  String,
    pub input_schema: serde_json::Value,
}

pub enum ToolCallError {
    InvalidInput(String),
    Execution(Box<dyn std::error::Error + Send + Sync>),
}

pub trait Toolbox: Send + Sync {
    fn specs(&self) -> Vec<ToolSpec>;
    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError>;
}
```

The agent loop only ever calls `specs()` (to build the LLM prompt) and `execute()` (to dispatch a tool call). Individual tool abstractions, typed `Tool` traits, `schemars` integration, and concrete `Toolbox` implementations all live in `agent-tools`.

---

## Agent

```rust
// agentcore/src/agent.rs

pub struct AgentConfig {
    pub max_iterations:  u32,    // default: 100
    pub stuck_threshold: usize,  // abort after N identical tool calls; default: 5
    pub nudge_threshold: usize,  // inject nudge message after N identical; default: 3
    pub max_tokens:      Option<u32>,
}

impl Default for AgentConfig { ... }

pub struct Agent {
    provider:      Arc<dyn LlmProvider>,
    system_prompt: String,
    toolbox:       Option<Arc<dyn Toolbox>>,
    handoff_tool:  Option<String>, // tool name that signals handoff; triggers AgentResult::Handoff
    config:        AgentConfig,
}

pub struct AgentBuilder { ... } // collects provider, system_prompt, toolbox, config

pub enum AgentInput {
    UserMessage(String),
    ToolResult { tool_call_id: String, output: String, is_error: bool },
}

pub struct RunOutput {
    pub result: AgentResult,
    pub usage:  Usage,
}

pub enum AgentResult {
    Completed { text: String },
    Handoff   { tool_name: String, data: Value }, // terminator tool was called
}

impl Agent {
    pub fn builder(provider: Arc<dyn LlmProvider>) -> AgentBuilder;

    pub async fn run(
        &self,
        history: Vec<Message>,
        input:   AgentInput,
        events:  &dyn EventSink,
        cancel:  CancellationToken,
    ) -> Result<RunOutput, AgentError>;
}
```

### Agent is stateless

`Agent` holds only immutable configuration. `run()` takes history as input and returns `RunOutput` — no mutation of self. The caller owns history; persistence is out of scope for `agentcore`.

### Unified entry point

`AgentInput` unifies `run` and `resume` into a single method. `UserMessage` starts a new turn; `ToolResult` resumes after a human-in-the-loop handoff. Both are converted to a `Message` and appended to history before the loop starts.

### Loop behavior

1. Convert `AgentInput` → `Message`, emit `MessageStart` + `MessageComplete`
2. Call `provider.complete(request, events)` — provider emits streaming events
3. Construct assistant `Message` from `CompletionResponse.parts`
4. Emit `MessageStart` + streaming chunk events + `MessageComplete` for assistant message
5. If `stop_reason == EndTurn` → emit `RunComplete`, return `AgentResult::Completed`
6. If a tool call matches the handoff tool → return `AgentResult::Handoff`
7. For each tool call: emit `ToolExecuting`, call `toolbox.execute()`, emit `ToolComplete`
8. Construct tool result `Message` from `ToolComplete` data (no separate `MessageComplete` emitted)
9. Stuck detection: if last N iterations identical tool calls → abort with `AgentError::StuckInLoop`
10. Nudge: if last M iterations identical → inject synthetic tool result nudging model to change approach
11. Repeat from step 2

---

## Events

```rust
// agentcore/src/events.rs

pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}
```

`emit` is synchronous. Callers who need async delivery implement `EventSink` with an `mpsc::Sender::try_send` internally. This avoids calling async handlers from inside sync streaming closures, which can cause deadlocks.

---

## Errors

```rust
// agentcore/src/error.rs

pub enum AgentError {
    MaxIterationsExceeded { max: u32 },
    StuckInLoop           { tool_name: String, count: usize },
    Provider(LlmError),
    Tool                  { name: String, source: ToolCallError },
    Cancelled,
}
```

All errors are typed. Callers can pattern-match without string parsing.

---

## Design decisions summary

| Decision | Rationale |
|---|---|
| `Agent` is immutable, `run()` returns output | No hidden mutation; reusable across sessions |
| No session/history in agentcore | Clean separation; persistence is caller's concern |
| `Toolbox` trait instead of `Vec<Box<dyn AnyTool>>` | Agent only needs two operations; individual tool abstractions belong in `agent-tools` |
| `EventSink::emit` is sync | Avoids `block_in_place`; callers wrap channels if needed |
| `MessageStart`/`MessageComplete` pairs with matching id | Lets UX replace streaming chunks with authoritative message |
| `ToolComplete` instead of `MessageStart`/`Complete` for tool results | Tool results don't stream; output carried directly |
| `AgentInput` enum unifies run + resume | Same operation, different input types |
| No LLM caching in agentcore | Provider-specific optimization; agentcore is cache-agnostic |
| Typed `AgentError` enum | Callers can distinguish error kinds without string parsing |
