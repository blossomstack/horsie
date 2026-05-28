# agentcore Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the agentcore crate — the stateless agent execution loop and its minimal supporting abstractions — as specified in `docs/superpowers/specs/2026-05-26-agentcore-design.md`.

**Architecture:** Fluorite-generated protocol types (`Message`, `ContentPart`, `AgentEvent`) live in the `models` crate. `agentcore` owns the `LlmProvider` and `Toolbox` traits (no concrete impls), and the stateless `Agent` struct whose `run()` takes history + input and returns `RunOutput`. History persistence and concrete providers are out of scope.

**Tech Stack:** Rust 2024, fluorite 0.6 (codegen), async-trait, tokio (rt + sync), tokio-util (CancellationToken), serde_json (Value), uuid, thiserror.

---

## File Map

```
fluorite/
  agent.fl                         ← new: Message, ContentPart, Role, Usage
  events.fl                        ← new: AgentEvent and sub-structs

models/
  src/lib.rs                       ← modify: expose generated agent + events modules

agentcore/
  Cargo.toml                       ← modify: add dependencies
  src/
    lib.rs                         ← modify: public re-exports
    error.rs                       ← new: AgentError, LlmError, ToolCallError
    events.rs                      ← new: EventSink trait
    tool.rs                        ← new: ToolSpec, ToolCallError, Toolbox trait
    provider.rs                    ← new: LlmProvider, CompletionRequest/Response, StopReason, ToolChoice
    agent.rs                       ← new: AgentConfig, Agent, AgentBuilder, AgentInput, RunOutput, loop
  tests/
    support/mod.rs                 ← new: MockProvider, MockToolbox, CollectingEventSink
    agent_test.rs                  ← new: integration tests

.rustfmt.toml                          ← already added: fmt config
Cargo.toml (workspace)             ← modify: add shared deps
```

---

## Task 1: Create feature branch

**Files:** none

- [ ] **Step 1: Create branch**

```bash
git checkout -b feat/agentcore
```

Expected: `Switched to a new branch 'feat/agentcore'`

---

## Task 2: Fluorite schemas

**Files:**
- Create: `fluorite/agent.fl`
- Create: `fluorite/events.fl`

> **Fluorite syntax notes:** `union` for tagged ADTs (not `enum`); variants wrap named structs; `#[type_tag = "type"]` sets the JSON discriminator field name; `enum` is for simple C-style enums only; cross-package imports use dot notation (`use agent.Message;`); `Any` maps to `serde_json::Value`.

- [ ] **Step 1: Write agent.fl**

```
/// Protocol types for agent messages
package agent;

/// Role of the message sender
enum Role {
    User,
    Assistant,
    Tool,
}

/// Plain text content
struct TextPart {
    text: String,
}

/// A tool call requested by the model
struct ToolCallPart {
    id: String,
    name: String,
    input: Any,
}

/// The result of executing a tool call
struct ToolResultPart {
    tool_call_id: String,
    output: String,
    is_error: bool,
}

/// Extended thinking content
struct ThinkingPart {
    text: String,
}

/// Content variant within a message
#[type_tag = "type"]
union ContentPart {
    Text(TextPart),
    ToolCall(ToolCallPart),
    ToolResult(ToolResultPart),
    Thinking(ThinkingPart),
}

/// A single message in the conversation
struct Message {
    role: Role,
    parts: Vec<ContentPart>,
}

/// Token usage for a model turn
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}
```

- [ ] **Step 2: Write events.fl**

```
/// Agent event types
package events;

use agent.Message;
use agent.Usage;
use agent.Role;

struct MessageStartEvent    { id: String, role: Role }
struct MessageCompleteEvent { id: String, message: Message }
struct TextChunkEvent       { text: String }
struct ThinkingEvent        { text: String }
struct ToolCallStartEvent   { id: String, name: String }
struct ToolCallInputDeltaEvent { id: String, delta: String }
struct ToolCallInputDoneEvent  { id: String }
struct ToolExecutingEvent   { id: String }
struct ToolCompleteEvent    { id: String, output: String, is_error: bool }
struct RunCompleteEvent     { usage: Usage, iterations: u32 }

/// Events emitted during agent execution for UX, streaming, and history.
///
/// Protocol:
///   MessageStart → (TextChunk|Thinking|ToolCallStart→ToolCallInputDelta*→ToolCallInputDone)* → MessageComplete
///   ToolExecuting → ToolComplete
///   RunComplete
#[type_tag = "type"]
union AgentEvent {
    MessageStart(MessageStartEvent),
    MessageComplete(MessageCompleteEvent),
    TextChunk(TextChunkEvent),
    Thinking(ThinkingEvent),
    ToolCallStart(ToolCallStartEvent),
    ToolCallInputDelta(ToolCallInputDeltaEvent),
    ToolCallInputDone(ToolCallInputDoneEvent),
    ToolExecuting(ToolExecutingEvent),
    ToolComplete(ToolCompleteEvent),
    RunComplete(RunCompleteEvent),
}
```

- [ ] **Step 3: Verify schemas compile**

```bash
cargo build -p models 2>&1 | tail -20
```

Expected: compiles without error. If fluorite reports a syntax error, cross-reference the agentx `.fl` files for the correct syntax.

- [ ] **Step 4: Discover generated output paths**

```bash
find $(cargo metadata --no-deps --format-version 1 | python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])") \
  -path "*/models-*/out/models" -type d 2>/dev/null | head -3
```

List the discovered directory to see what files fluorite generated:

```bash
find $(cargo metadata --no-deps --format-version 1 | python3 -c "import sys,json; print(json.load(sys.stdin)['target_directory'])") \
  -path "*/models-*/out/models" -type d -exec ls {} \; 2>/dev/null | head -20
```

Note the directory names and whether they contain `mod.rs` or `<name>.rs`. Use these paths in Task 3.

---

## Task 3: Models crate — expose generated modules

**Files:**
- Modify: `models/src/lib.rs`

- [ ] **Step 1: Update lib.rs with correct include! paths**

Based on the paths discovered in Task 2 Step 4. The typical pattern (per the existing comment) is `/models/<package_name>/mod.rs`:

```rust
#[allow(clippy::doc_markdown, clippy::too_many_arguments)]
pub mod models {
    pub mod agent {
        include!(concat!(env!("OUT_DIR"), "/models/agent/mod.rs"));
    }
    pub mod events {
        include!(concat!(env!("OUT_DIR"), "/models/events/mod.rs"));
    }
}
```

If the discovered paths differ (e.g., `agent.rs` instead of `agent/mod.rs`), adjust accordingly.

- [ ] **Step 2: Verify**

```bash
cargo build -p models
```

Expected: compiles. If it errors on the `include!` path, fix the path to match the discovered output.

---

## Task 4: Workspace + agentcore dependencies

**Files:**
- Modify: `Cargo.toml` (workspace)
- Modify: `agentcore/Cargo.toml`

- [ ] **Step 1: Add workspace-level shared deps**

In `Cargo.toml`, update `[workspace.dependencies]`:

```toml
[workspace.dependencies]
fluorite         = "0.6.0"
fluorite_codegen = "0.6.0"
serde            = { version = "1.0", features = ["derive"] }
serde_json       = "1.0"
schemars         = "1.0"
async-trait      = "0.1"
thiserror        = "1.0"
tokio            = { version = "1", features = ["rt", "rt-multi-thread", "macros", "sync"] }
tokio-util       = { version = "0.7", features = ["sync"] }
uuid             = { version = "1", features = ["v4"] }
```

- [ ] **Step 2: Update agentcore/Cargo.toml**

```toml
[package]
name = "agentcore"
version = "0.1.0"
edition = "2024"

[dependencies]
models      = { path = "../models" }
async-trait = { workspace = true }
thiserror   = { workspace = true }
tokio       = { workspace = true }
tokio-util  = { workspace = true }
serde_json  = { workspace = true }
uuid        = { workspace = true }

[lints]
workspace = true
```

- [ ] **Step 3: Create empty source modules**

```bash
touch agentcore/src/error.rs \
      agentcore/src/events.rs \
      agentcore/src/tool.rs \
      agentcore/src/provider.rs \
      agentcore/src/agent.rs
```

- [ ] **Step 4: Stub lib.rs to declare modules**

```rust
// agentcore/src/lib.rs
mod error;
mod events;
mod tool;
mod provider;
mod agent;
```

- [ ] **Step 5: Verify**

```bash
cargo check -p agentcore
```

Expected: compiles (empty modules).

---

## Task 5: Error types

**Files:**
- Modify: `agentcore/src/error.rs`
- Create: `agentcore/tests/error_test.rs`

- [ ] **Step 1: Write failing test**

```rust
// agentcore/tests/error_test.rs
#![cfg_attr(test, allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
))]

use agentcore::{AgentError, LlmError, ToolCallError};

#[test]
fn agent_error_cancelled_display() {
    let e = AgentError::Cancelled;
    assert_eq!(e.to_string(), "cancelled");
}

#[test]
fn agent_error_max_iterations_display() {
    let e = AgentError::MaxIterationsExceeded { max: 50 };
    assert!(e.to_string().contains("50"));
}

#[test]
fn agent_error_stuck_display() {
    let e = AgentError::StuckInLoop { tool_name: "search".into(), count: 5 };
    assert!(e.to_string().contains("search"));
    assert!(e.to_string().contains("5"));
}

#[test]
fn tool_call_error_invalid_input_display() {
    let e = ToolCallError::InvalidInput("bad json".into());
    assert!(e.to_string().contains("bad json"));
}

#[test]
fn llm_error_api_error_display() {
    let e = LlmError::ApiError { status: 429, message: "rate limit".into() };
    assert!(e.to_string().contains("429"));
}
```

- [ ] **Step 2: Run test — verify it fails**

```bash
cargo test -p agentcore --test error_test 2>&1 | tail -10
```

Expected: compile error (types not exported yet).

- [ ] **Step 3: Implement error.rs**

```rust
// agentcore/src/error.rs
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("max iterations exceeded (max={max})")]
    MaxIterationsExceeded { max: u32 },

    #[error("stuck in loop: tool '{tool_name}' called identically {count} times")]
    StuckInLoop { tool_name: String, count: usize },

    #[error("provider error: {0}")]
    Provider(#[from] LlmError),

    #[error("tool '{name}' failed: {source}")]
    Tool { name: String, source: ToolCallError },

    #[error("cancelled")]
    Cancelled,
}

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimit { retry_after: Option<std::time::Duration> },

    #[error("provider overloaded")]
    Overloaded,

    #[error("api error {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("network error: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Error)]
pub enum ToolCallError {
    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("execution error: {0}")]
    Execution(#[source] Box<dyn std::error::Error + Send + Sync>),
}
```

- [ ] **Step 4: Export from lib.rs**

```rust
// agentcore/src/lib.rs
mod error;
mod events;
mod tool;
mod provider;
mod agent;

pub use error::{AgentError, LlmError, ToolCallError};
```

- [ ] **Step 5: Run test — verify it passes**

```bash
cargo test -p agentcore --test error_test
```

Expected: all 5 tests pass.

---

## Task 6: EventSink trait

**Files:**
- Modify: `agentcore/src/events.rs`
- Modify: `agentcore/src/lib.rs`

- [ ] **Step 1: Implement events.rs**

```rust
// agentcore/src/events.rs
use models::models::events::AgentEvent;

/// Sync observer for agent events. Implement this to receive real-time
/// streaming chunks, message boundaries, and run lifecycle signals.
///
/// `emit` is synchronous — callers who need async delivery should implement
/// this with an `mpsc::Sender::try_send` internally.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}
```

- [ ] **Step 2: Export from lib.rs**

```rust
// agentcore/src/lib.rs
mod error;
mod events;
mod tool;
mod provider;
mod agent;

pub use error::{AgentError, LlmError, ToolCallError};
pub use events::EventSink;
pub use models::models::agent::{ContentPart, Message, Role, TextPart, ToolCallPart, ToolResultPart, ThinkingPart, Usage};
pub use models::models::events::{
    AgentEvent, MessageStartEvent, MessageCompleteEvent, TextChunkEvent, ThinkingEvent,
    ToolCallStartEvent, ToolCallInputDeltaEvent, ToolCallInputDoneEvent,
    ToolExecutingEvent, ToolCompleteEvent, RunCompleteEvent,
};
```

> **Note:** The exact struct names from the generated code depend on fluorite's naming conventions. If the generated names differ (e.g., `MessageStart` instead of `MessageStartEvent`), adjust the re-exports to match. Run `cargo doc -p models --open` or inspect the generated file to confirm names.

- [ ] **Step 3: Verify**

```bash
cargo check -p agentcore
```

Expected: compiles.

---

## Task 7: Tool abstraction

**Files:**
- Modify: `agentcore/src/tool.rs`
- Modify: `agentcore/src/lib.rs`

- [ ] **Step 1: Implement tool.rs**

```rust
// agentcore/src/tool.rs
use async_trait::async_trait;
use serde_json::Value;
use crate::error::ToolCallError;

/// Schema and metadata for a tool — sent to the LLM in the completion request.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name:         String,
    pub description:  String,
    pub input_schema: Value,
}

/// Provides a set of tools to the agent loop.
/// The agent calls `specs()` to build the LLM prompt and `execute()` to
/// dispatch tool calls. All other tool concerns (typed inputs, schema
/// generation, MCP bridging) belong in the `agent-tools` crate.
#[async_trait]
pub trait Toolbox: Send + Sync {
    /// Returns the tool definitions sent to the LLM on each completion request.
    fn specs(&self) -> Vec<ToolSpec>;

    /// Executes a tool by name with the given JSON input.
    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError>;
}
```

- [ ] **Step 2: Export from lib.rs**

Add to exports:

```rust
pub use tool::{ToolSpec, Toolbox};
```

- [ ] **Step 3: Verify**

```bash
cargo check -p agentcore
```

---

## Task 8: Provider abstraction

**Files:**
- Modify: `agentcore/src/provider.rs`
- Modify: `agentcore/src/lib.rs`

- [ ] **Step 1: Implement provider.rs**

```rust
// agentcore/src/provider.rs
use async_trait::async_trait;
use crate::{error::LlmError, events::EventSink, tool::ToolSpec};
use models::models::agent::{ContentPart, Message, Usage};

/// Input to a single LLM completion call.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub messages:    Vec<Message>,
    pub system:      Option<String>,
    pub tools:       Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens:  Option<u32>,
}

/// Output from a single LLM completion call.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub parts:       Vec<ContentPart>,
    pub stop_reason: StopReason,
    pub usage:       Usage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone)]
pub enum ToolChoice {
    /// Model decides whether to call a tool.
    Auto,
    /// Model must call some tool.
    Any,
    /// Model must call this specific tool.
    Required(String),
}

/// Abstraction over LLM providers. No concrete implementations live in agentcore.
/// Provider-specific concerns (caching, retry, backoff, token budgets) are handled
/// entirely inside concrete implementations.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_id(&self) -> &str;

    /// Send a completion request. Streaming events (TextChunk, Thinking,
    /// ToolCallStart/InputDelta/InputDone) are emitted on `events` as they arrive.
    async fn complete(
        &self,
        request: CompletionRequest,
        events:  &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError>;
}
```

- [ ] **Step 2: Export from lib.rs**

Add to exports:

```rust
pub use provider::{CompletionRequest, CompletionResponse, LlmProvider, StopReason, ToolChoice};
```

- [ ] **Step 3: Verify**

```bash
cargo check -p agentcore
```

---

## Task 9: Agent structs + builder (no loop yet)

**Files:**
- Modify: `agentcore/src/agent.rs`
- Modify: `agentcore/src/lib.rs`

- [ ] **Step 1: Implement agent.rs — structs only**

```rust
// agentcore/src/agent.rs
use std::sync::Arc;
use serde_json::Value;
use models::models::agent::{ContentPart, Message, Role, TextPart, ToolResultPart, Usage};
use models::models::events::AgentEvent;
use tokio_util::sync::CancellationToken;
use crate::{
    error::AgentError,
    events::EventSink,
    provider::LlmProvider,
    tool::Toolbox,
};

/// Tuning parameters for the agent loop.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Maximum number of LLM call iterations. Default: 100.
    pub max_iterations: u32,
    /// Abort with StuckInLoop after this many identical consecutive tool-call
    /// fingerprints. Default: 5.
    pub stuck_threshold: usize,
    /// Inject a nudge message after this many identical consecutive fingerprints
    /// (must be < stuck_threshold). Default: 3.
    pub nudge_threshold: usize,
    /// Override max_tokens sent to the provider. None uses the provider default.
    pub max_tokens: Option<u32>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iterations:  100,
            stuck_threshold: 5,
            nudge_threshold: 3,
            max_tokens:      None,
        }
    }
}

/// Immutable, stateless agent. Reusable across sessions — holds config only.
/// History is owned by the caller; `run()` takes it as input and returns new messages.
pub struct Agent {
    pub(crate) provider:      Arc<dyn LlmProvider>,
    pub(crate) system_prompt: String,
    pub(crate) toolbox:       Option<Arc<dyn Toolbox>>,
    /// Tool name that signals a human-in-the-loop handoff.
    /// When the LLM calls this tool, `run()` returns `AgentResult::Handoff`
    /// instead of executing the tool.
    pub(crate) handoff_tool:  Option<String>,
    pub(crate) config:        AgentConfig,
}

/// Input to a single `Agent::run` call.
pub enum AgentInput {
    /// A new user message — starts a new turn.
    UserMessage(String),
    /// Resume after a human-in-the-loop handoff with the tool result.
    ToolResult { tool_call_id: String, output: String, is_error: bool },
}

impl AgentInput {
    pub(crate) fn into_message(self) -> Message {
        match self {
            AgentInput::UserMessage(text) => Message {
                role: Role::User,
                parts: vec![ContentPart::Text(TextPart { text })],
            },
            AgentInput::ToolResult { tool_call_id, output, is_error } => Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResultPart {
                    tool_call_id,
                    output,
                    is_error,
                })],
            },
        }
    }
}

/// Output from a completed `Agent::run` call.
pub struct RunOutput {
    pub result: AgentResult,
    pub usage:  Usage,
}

/// Terminal state of an agent run.
pub enum AgentResult {
    /// The agent finished — final text response.
    Completed { text: String },
    /// The agent called the configured handoff tool — caller should process
    /// `data` and resume with `AgentInput::ToolResult` if needed.
    Handoff { tool_name: String, data: Value },
}

/// Builder for `Agent`.
pub struct AgentBuilder {
    provider:      Arc<dyn LlmProvider>,
    system_prompt: String,
    toolbox:       Option<Arc<dyn Toolbox>>,
    handoff_tool:  Option<String>,
    config:        AgentConfig,
}

impl AgentBuilder {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            system_prompt: String::new(),
            toolbox:       None,
            handoff_tool:  None,
            config:        AgentConfig::default(),
        }
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_toolbox(mut self, toolbox: Arc<dyn Toolbox>) -> Self {
        self.toolbox = Some(toolbox);
        self
    }

    pub fn with_handoff_tool(mut self, name: impl Into<String>) -> Self {
        self.handoff_tool = Some(name.into());
        self
    }

    pub fn with_config(mut self, config: AgentConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> Agent {
        Agent {
            provider:      self.provider,
            system_prompt: self.system_prompt,
            toolbox:       self.toolbox,
            handoff_tool:  self.handoff_tool,
            config:        self.config,
        }
    }
}

impl Agent {
    pub fn builder(provider: Arc<dyn LlmProvider>) -> AgentBuilder {
        AgentBuilder::new(provider)
    }

    pub async fn run(
        &self,
        _history: Vec<Message>,
        _input:   AgentInput,
        _events:  &dyn EventSink,
        _cancel:  CancellationToken,
    ) -> Result<RunOutput, AgentError> {
        // implemented in Task 11
        Err(AgentError::Cancelled)
    }
}
```

- [ ] **Step 2: Export from lib.rs**

```rust
// agentcore/src/lib.rs
mod error;
mod events;
mod tool;
mod provider;
mod agent;

pub use agent::{Agent, AgentBuilder, AgentConfig, AgentInput, AgentResult, RunOutput};
pub use error::{AgentError, LlmError, ToolCallError};
pub use events::EventSink;
pub use provider::{CompletionRequest, CompletionResponse, LlmProvider, StopReason, ToolChoice};
pub use tool::{ToolSpec, Toolbox};

// Protocol types from generated models
pub use models::models::agent::{
    ContentPart, Message, Role, TextPart, ToolCallPart, ToolResultPart, ThinkingPart, Usage,
};
pub use models::models::events::{
    AgentEvent, MessageStartEvent, MessageCompleteEvent, TextChunkEvent, ThinkingEvent,
    ToolCallStartEvent, ToolCallInputDeltaEvent, ToolCallInputDoneEvent,
    ToolExecutingEvent, ToolCompleteEvent, RunCompleteEvent,
};
```

- [ ] **Step 3: Verify**

```bash
cargo check -p agentcore
```

---

## Task 10: Test infrastructure

**Files:**
- Create: `agentcore/tests/support/mod.rs`

- [ ] **Step 1: Create directory**

```bash
mkdir -p agentcore/tests/support
```

- [ ] **Step 2: Write support/mod.rs**

```rust
// agentcore/tests/support/mod.rs
#![cfg_attr(test, allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
))]

use agentcore::*;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

// ── Mock LLM Provider ────────────────────────────────────────────────────────

pub struct MockProvider {
    /// Responses returned in order; cycles when exhausted.
    responses: Vec<CompletionResponse>,
    call_index: Mutex<usize>,
}

impl MockProvider {
    pub fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self { responses, call_index: Mutex::new(0) })
    }

    /// Convenience: single text-only response.
    pub fn text(text: &str) -> Arc<Self> {
        Self::new(vec![CompletionResponse {
            parts: vec![ContentPart::Text(TextPart { text: text.to_string() })],
            stop_reason: StopReason::EndTurn,
            usage: Usage { input_tokens: 10, output_tokens: 5 },
        }])
    }

    /// Convenience: first response calls a tool, second returns text.
    pub fn tool_then_text(tool_id: &str, tool_name: &str, input: Value, reply: &str) -> Arc<Self> {
        Self::new(vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id:    tool_id.to_string(),
                    name:  tool_name.to_string(),
                    input,
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage { input_tokens: 20, output_tokens: 10 },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart { text: reply.to_string() })],
                stop_reason: StopReason::EndTurn,
                usage: Usage { input_tokens: 30, output_tokens: 8 },
            },
        ])
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_id(&self) -> &str { "mock-model" }

    async fn complete(
        &self,
        _request: CompletionRequest,
        _events:  &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        let mut idx = self.call_index.lock().unwrap();
        let response = self.responses[*idx % self.responses.len()].clone();
        *idx += 1;
        Ok(response)
    }
}

// ── Mock Toolbox ─────────────────────────────────────────────────────────────

pub struct MockToolbox {
    specs:   Vec<ToolSpec>,
    handler: Arc<dyn Fn(&str, Value) -> Result<Value, ToolCallError> + Send + Sync>,
}

impl MockToolbox {
    pub fn new(
        specs:   Vec<ToolSpec>,
        handler: impl Fn(&str, Value) -> Result<Value, ToolCallError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self { specs, handler: Arc::new(handler) })
    }

    /// Toolbox with a single tool that echoes its input back.
    pub fn echo(name: &str) -> Arc<Self> {
        let spec = ToolSpec {
            name:         name.to_string(),
            description:  "echo tool".to_string(),
            input_schema: json!({ "type": "object" }),
        };
        Self::new(vec![spec], |_, input| Ok(input))
    }
}

#[async_trait]
impl Toolbox for MockToolbox {
    fn specs(&self) -> Vec<ToolSpec> { self.specs.clone() }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        (self.handler)(name, input)
    }
}

// ── Collecting EventSink ─────────────────────────────────────────────────────

pub struct CollectingEventSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl CollectingEventSink {
    pub fn new() -> Self { Self { events: Mutex::new(Vec::new()) } }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }

    pub fn message_complete_ids(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::MessageComplete(mc) => Some(mc.id),
                _ => None,
            })
            .collect()
    }
}

impl EventSink for CollectingEventSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}
```

- [ ] **Step 3: Verify support compiles**

```bash
cargo test -p agentcore --test agent_test 2>&1 | head -5
```

Expected: error about missing `agent_test.rs` — that's fine, support module syntax is valid.

---

## Task 11: Failing tests

**Files:**
- Create: `agentcore/tests/agent_test.rs`

- [ ] **Step 1: Write failing tests**

```rust
// agentcore/tests/agent_test.rs
#![cfg_attr(test, allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm,
))]

mod support;
use support::{CollectingEventSink, MockProvider, MockToolbox};

use agentcore::*;
use serde_json::json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

// ── Simple completion ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_simple_text_completion() {
    let agent  = Agent::builder(MockProvider::text("Hello, world!")).build();
    let sink   = CollectingEventSink::new();
    let output = agent.run(vec![], AgentInput::UserMessage("hi".into()), &sink, CancellationToken::new()).await.unwrap();

    match output.result {
        AgentResult::Completed { text } => assert_eq!(text, "Hello, world!"),
        other => panic!("expected Completed, got {:?}", std::mem::discriminant(&other)),
    }
    assert_eq!(output.usage.output_tokens, 5);
}

#[tokio::test]
async fn test_completion_emits_message_complete_events() {
    let agent = Agent::builder(MockProvider::text("done")).build();
    let sink  = CollectingEventSink::new();
    agent.run(vec![], AgentInput::UserMessage("go".into()), &sink, CancellationToken::new()).await.unwrap();

    // At minimum: user MessageComplete + assistant MessageComplete
    let ids = sink.message_complete_ids();
    assert!(ids.len() >= 2, "expected at least 2 MessageComplete events, got {}", ids.len());
}

#[tokio::test]
async fn test_run_complete_event_emitted() {
    let agent = Agent::builder(MockProvider::text("ok")).build();
    let sink  = CollectingEventSink::new();
    agent.run(vec![], AgentInput::UserMessage("x".into()), &sink, CancellationToken::new()).await.unwrap();

    let run_complete_count = sink.events().iter().filter(|e| matches!(e, AgentEvent::RunComplete(_))).count();
    assert_eq!(run_complete_count, 1);
}

// ── Tool call cycle ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_tool_call_cycle() {
    let provider = MockProvider::tool_then_text("tc1", "search", json!({"q": "rust"}), "found it");
    let toolbox  = MockToolbox::echo("search");
    let agent    = Agent::builder(provider).with_toolbox(toolbox).build();
    let sink     = CollectingEventSink::new();

    let output = agent.run(vec![], AgentInput::UserMessage("search rust".into()), &sink, CancellationToken::new()).await.unwrap();

    match output.result {
        AgentResult::Completed { text } => assert_eq!(text, "found it"),
        other => panic!("expected Completed, got {:?}", std::mem::discriminant(&other)),
    }

    // ToolExecuting and ToolComplete must be emitted
    let events = sink.events();
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolExecuting(_))));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::ToolComplete(_))));
}

#[tokio::test]
async fn test_tool_result_added_to_history() {
    // After tool execution, the agent must have appended a tool-result message
    // (MessageComplete with role=Tool comes from ToolComplete, not a separate event,
    //  but we verify the loop continues correctly by checking Completed is returned).
    let provider = MockProvider::tool_then_text("tc1", "calc", json!({"x": 1}), "result: 1");
    let toolbox  = MockToolbox::echo("calc");
    let agent    = Agent::builder(provider).with_toolbox(toolbox).build();
    let sink     = CollectingEventSink::new();

    let output = agent.run(vec![], AgentInput::UserMessage("calc".into()), &sink, CancellationToken::new()).await.unwrap();
    assert!(matches!(output.result, AgentResult::Completed { .. }));
}

// ── Handoff tool ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_handoff_tool_returns_handoff_result() {
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id:    "hc1".to_string(),
            name:  "handoff".to_string(),
            input: json!({"answer": 42}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage { input_tokens: 10, output_tokens: 5 },
    }]);
    let agent = Agent::builder(provider).with_handoff_tool("handoff").build();
    let sink  = CollectingEventSink::new();

    let output = agent.run(vec![], AgentInput::UserMessage("go".into()), &sink, CancellationToken::new()).await.unwrap();

    match output.result {
        AgentResult::Handoff { tool_name, data } => {
            assert_eq!(tool_name, "handoff");
            assert_eq!(data["answer"], 42);
        }
        other => panic!("expected Handoff, got {:?}", std::mem::discriminant(&other)),
    }
}

// ── AgentInput::ToolResult (resume) ──────────────────────────────────────────

#[tokio::test]
async fn test_resume_with_tool_result() {
    // Simulate resuming after a handoff by passing existing history + ToolResult input
    let history = vec![
        Message { role: Role::User, parts: vec![ContentPart::Text(TextPart { text: "question".into() })] },
        Message { role: Role::Assistant, parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "hc1".into(), name: "handoff".into(), input: json!({}),
        })] },
    ];
    let provider = MockProvider::text("thanks for the answer");
    let agent    = Agent::builder(provider).build();
    let sink     = CollectingEventSink::new();

    let output = agent.run(
        history,
        AgentInput::ToolResult { tool_call_id: "hc1".into(), output: "42".into(), is_error: false },
        &sink,
        CancellationToken::new(),
    ).await.unwrap();

    assert!(matches!(output.result, AgentResult::Completed { .. }));
}

// ── Max iterations ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_max_iterations_exceeded() {
    // Provider always requests a tool call → loop never exits naturally
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "t1".into(), name: "loop_tool".into(), input: json!({}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage { input_tokens: 5, output_tokens: 2 },
    }]);
    let toolbox = MockToolbox::echo("loop_tool");
    let config  = AgentConfig { max_iterations: 3, stuck_threshold: 10, nudge_threshold: 8, max_tokens: None };
    let agent   = Agent::builder(provider).with_toolbox(toolbox).with_config(config).build();
    let sink    = CollectingEventSink::new();

    let err = agent.run(vec![], AgentInput::UserMessage("go".into()), &sink, CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, AgentError::MaxIterationsExceeded { max: 3 }));
}

// ── Stuck detection ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_stuck_detection() {
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "s1".into(), name: "stuck_tool".into(), input: json!({"x": 1}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage { input_tokens: 5, output_tokens: 2 },
    }]);
    let toolbox = MockToolbox::echo("stuck_tool");
    let config  = AgentConfig { max_iterations: 20, stuck_threshold: 3, nudge_threshold: 2, max_tokens: None };
    let agent   = Agent::builder(provider).with_toolbox(toolbox).with_config(config).build();
    let sink    = CollectingEventSink::new();

    let err = agent.run(vec![], AgentInput::UserMessage("go".into()), &sink, CancellationToken::new()).await.unwrap_err();
    assert!(matches!(err, AgentError::StuckInLoop { .. }));
}

// ── Cancellation ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_cancellation() {
    let provider = MockProvider::new(vec![CompletionResponse {
        parts: vec![ContentPart::ToolCall(ToolCallPart {
            id: "c1".into(), name: "some_tool".into(), input: json!({}),
        })],
        stop_reason: StopReason::ToolUse,
        usage: Usage { input_tokens: 5, output_tokens: 2 },
    }]);
    let toolbox = MockToolbox::echo("some_tool");
    let agent   = Agent::builder(provider).with_toolbox(toolbox).build();
    let sink    = CollectingEventSink::new();
    let token   = CancellationToken::new();
    token.cancel();  // pre-cancelled

    let err = agent.run(vec![], AgentInput::UserMessage("go".into()), &sink, token).await.unwrap_err();
    assert!(matches!(err, AgentError::Cancelled));
}
```

- [ ] **Step 2: Run tests — verify they fail**

```bash
cargo test -p agentcore --test agent_test 2>&1 | tail -15
```

Expected: tests compile but fail — `run()` returns `Err(Cancelled)` for everything (stub implementation).

---

## Task 12: Implement agent loop

**Files:**
- Modify: `agentcore/src/agent.rs`

- [ ] **Step 1: Add imports to agent.rs**

At the top of `agent.rs`, replace the existing imports with:

```rust
use std::collections::VecDeque;
use std::sync::Arc;
use serde_json::Value;
use uuid::Uuid;
use tokio_util::sync::CancellationToken;
use models::models::agent::{ContentPart, Message, Role, TextPart, ToolCallPart, ToolResultPart, Usage};
use models::models::events::{
    AgentEvent, MessageStartEvent, MessageCompleteEvent,
    ToolExecutingEvent, ToolCompleteEvent, RunCompleteEvent,
};
use crate::{
    error::AgentError,
    events::EventSink,
    provider::{CompletionRequest, LlmProvider, StopReason, ToolChoice},
    tool::Toolbox,
};
```

- [ ] **Step 2: Add private helpers after the structs**

Add these free functions inside `agent.rs`:

```rust
fn extract_tool_calls(parts: &[ContentPart]) -> Vec<(String, String, Value)> {
    parts.iter()
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc) => Some((tc.id.clone(), tc.name.clone(), tc.input.clone())),
            _ => None,
        })
        .collect()
}

fn extract_text(parts: &[ContentPart]) -> String {
    parts.iter()
        .filter_map(|p| match p { ContentPart::Text(t) => Some(t.text.as_str()), _ => None })
        .collect::<Vec<_>>()
        .join("")
}

fn tool_fingerprint(tool_calls: &[(String, String, Value)]) -> String {
    tool_calls.iter()
        .map(|(_, name, input)| format!("{}:{}", name, input))
        .collect::<Vec<_>>()
        .join("|")
}

fn emit_message(events: &dyn EventSink, message: Message) {
    let id = Uuid::new_v4().to_string();
    events.emit(AgentEvent::MessageStart(MessageStartEvent { id: id.clone(), role: message.role.clone() }));
    events.emit(AgentEvent::MessageComplete(MessageCompleteEvent { id, message }));
}
```

- [ ] **Step 3: Replace the stub `run` with the real implementation**

Replace the `run` method body in the `impl Agent` block:

```rust
pub async fn run(
    &self,
    mut history:  Vec<Message>,
    input:        AgentInput,
    events:       &dyn EventSink,
    cancel:       CancellationToken,
) -> Result<RunOutput, AgentError> {
    // ── 1. Convert input → message and emit ─────────────────────────────────
    let input_msg = input.into_message();
    emit_message(events, input_msg.clone());
    history.push(input_msg);

    let mut total_usage = Usage { input_tokens: 0, output_tokens: 0 };
    let mut iteration:   u32 = 0;
    let mut recent_fingerprints: VecDeque<String> = VecDeque::new();

    loop {
        // ── 2. Cancellation check ────────────────────────────────────────────
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        // ── 3. Iteration limit ───────────────────────────────────────────────
        if iteration >= self.config.max_iterations {
            return Err(AgentError::MaxIterationsExceeded { max: self.config.max_iterations });
        }
        iteration += 1;

        // ── 4. Build and send completion request ─────────────────────────────
        let tools = self.toolbox.as_ref().map(|t| t.specs()).unwrap_or_default();
        let request = CompletionRequest {
            messages:    history.clone(),
            system:      if self.system_prompt.is_empty() { None } else { Some(self.system_prompt.clone()) },
            tools,
            tool_choice: ToolChoice::Auto,
            max_tokens:  self.config.max_tokens,
        };

        let response = self.provider.complete(request, events).await
            .map_err(AgentError::Provider)?;

        total_usage.input_tokens  += response.usage.input_tokens;
        total_usage.output_tokens += response.usage.output_tokens;

        // ── 5. Emit and record assistant message ─────────────────────────────
        let assistant_msg = Message { role: Role::Assistant, parts: response.parts.clone() };
        emit_message(events, assistant_msg.clone());
        history.push(assistant_msg);

        let tool_calls = extract_tool_calls(&response.parts);

        // ── 6. No tool calls → done ──────────────────────────────────────────
        if tool_calls.is_empty() {
            events.emit(AgentEvent::RunComplete(RunCompleteEvent {
                usage: total_usage.clone(), iterations: iteration,
            }));
            return Ok(RunOutput {
                result: AgentResult::Completed { text: extract_text(&response.parts) },
                usage:  total_usage,
            });
        }

        // ── 7. Handoff tool check ────────────────────────────────────────────
        if let Some(ref handoff_name) = self.handoff_tool {
            if let Some((_, name, data)) = tool_calls.iter().find(|(_, n, _)| n == handoff_name) {
                events.emit(AgentEvent::RunComplete(RunCompleteEvent {
                    usage: total_usage.clone(), iterations: iteration,
                }));
                return Ok(RunOutput {
                    result: AgentResult::Handoff { tool_name: name.clone(), data: data.clone() },
                    usage:  total_usage,
                });
            }
        }

        // ── 8. Stuck / nudge detection ───────────────────────────────────────
        let fingerprint = tool_fingerprint(&tool_calls);
        recent_fingerprints.push_back(fingerprint.clone());
        if recent_fingerprints.len() > self.config.stuck_threshold {
            recent_fingerprints.pop_front();
        }

        if recent_fingerprints.len() >= self.config.stuck_threshold
            && recent_fingerprints.iter().all(|f| f == &fingerprint)
        {
            return Err(AgentError::StuckInLoop {
                tool_name: tool_calls[0].1.clone(),
                count:     self.config.stuck_threshold,
            });
        }

        let should_nudge = recent_fingerprints.len() >= self.config.nudge_threshold
            && recent_fingerprints.iter().all(|f| f == &fingerprint);

        if should_nudge {
            for (id, _, _) in &tool_calls {
                history.push(Message {
                    role: Role::Tool,
                    parts: vec![ContentPart::ToolResult(ToolResultPart {
                        tool_call_id: id.clone(),
                        output: "You have called this tool with identical arguments multiple times. Please try a different approach.".to_string(),
                        is_error: false,
                    })],
                });
            }
            continue;
        }

        // ── 9. Execute tools ─────────────────────────────────────────────────
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        for (id, name, input) in &tool_calls {
            events.emit(AgentEvent::ToolExecuting(ToolExecutingEvent { id: id.clone() }));

            let (output, is_error) = match &self.toolbox {
                None => (format!("no toolbox available to execute tool '{}'", name), true),
                Some(toolbox) => match toolbox.execute(name, input.clone()).await {
                    Ok(v)  => (v.to_string(), false),
                    Err(e) => (e.to_string(), true),
                },
            };

            events.emit(AgentEvent::ToolComplete(ToolCompleteEvent {
                id: id.clone(), output: output.clone(), is_error,
            }));

            history.push(Message {
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: id.clone(), output, is_error,
                })],
            });
        }
    }
}
```

- [ ] **Step 4: Run all tests — verify they pass**

```bash
cargo test -p agentcore
```

Expected: all tests pass. If any fail, read the failure carefully — likely a type name mismatch between the plan and the actual generated fluorite types. Fix the type names to match the generated output.

---

## Task 13: Lint, format, full workspace check

**Files:** None (fixes only)

- [ ] **Step 1: Format**

```bash
cargo fmt --all
```

- [ ] **Step 2: Clippy**

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Fix any warnings before proceeding.

- [ ] **Step 3: Full workspace test**

```bash
cargo test --workspace
```

Expected: all tests pass.

---

## Task 14: GitHub Actions CI

**Files:**
- Create: `.github/workflows/ci.yml`

- [ ] **Step 1: Create workflow directory**

```bash
mkdir -p .github/workflows
```

- [ ] **Step 2: Write CI workflow**

```yaml
# .github/workflows/ci.yml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

env:
  CARGO_TERM_COLOR: always

jobs:
  check:
    name: Check
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust stable
        uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy, rustfmt

      - name: Cache cargo
        uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-cargo-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: ${{ runner.os }}-cargo-

      - name: Format check
        run: cargo fmt --all -- --check

      - name: Clippy
        run: cargo clippy --all-targets --all-features -- -D warnings

      - name: Tests
        run: cargo test --workspace
```

- [ ] **Step 3: Verify workflow file is valid YAML**

```bash
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/ci.yml'))" && echo "valid"
```

Expected: `valid`

---

## Task 15: Squash, push, create PR, monitor CI

- [ ] **Step 1: Verify no confidential references remain in tracked files**

```bash
git grep -r "agentx" -- ':(exclude).git' 2>/dev/null
```

Expected: no output. If any matches appear, remove them before continuing.

- [ ] **Step 2: Squash all work into one commit**

```bash
git add -A
git reset --soft 76db94e
git add -A
git commit -m "feat(agentcore): implement agentcore design"
```

`76db94e` is the last baseline commit before this feature work began. This squashes the spec doc, CI workflow, all implementation, and tests into a single commit.

- [ ] **Step 3: Push branch**

```bash
git push -u origin feat/agentcore
```

- [ ] **Step 4: Create PR**

```bash
gh pr create \
  --title "feat(agentcore): implement agentcore" \
  --body "$(cat <<'EOF'
Implements the agentcore crate.

- Fluorite schemas for `Message`/`ContentPart`/`AgentEvent` protocol types
- `LlmProvider` and `Toolbox` traits (no concrete implementations)
- Stateless `Agent` with unified `run(history, AgentInput, events, cancel)` entry point
- Typed `AgentError` / `LlmError` / `ToolCallError`
- Sync `EventSink` with `MessageStart`/`MessageComplete` scope pairs for UX replacement
- Stuck detection + nudge loop guard
- GitHub Actions CI (fmt, clippy, test)
EOF
)"
```

- [ ] **Step 5: Watch CI until all checks pass**

```bash
gh pr checks --watch
```

Expected: all checks show `pass`. If any check fails, read the failure with `gh run view --log-failed`, fix the issue, amend the commit, and force-push:

```bash
# After fixing:
git add -A
git commit --amend --no-edit
git push --force-with-lease origin feat/agentcore
gh pr checks --watch
```
