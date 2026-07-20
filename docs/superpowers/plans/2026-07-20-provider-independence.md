# Provider Independence Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove horsie's provider seam by running core agent flows on a second, genuinely different wire protocol (OpenAI-compatible `/v1/chat/completions`), backed by a conformance suite both providers pass.

**Architecture:** `LlmProvider` (`agentcore/src/provider.rs:35`) is already provider-neutral, so the second backend is a new crate, not a refactor. `providers/mock-llm` grows a second route so one mock server can speak both wires from the same `MockResponse` queue; a conformance suite in the `integration-tests` crate runs identical agent-loop assertions against each provider pointed at its own wire. Cross-cutting change is deliberately minimal: one capability field, consumed at exactly one site.

**Tech Stack:** Rust 2024 edition, tokio, axum (mock server), reqwest + reqwest-eventsource (OpenAI wire), serde_json, fluorite (schema codegen), React + TypeScript (Settings UI).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-20-provider-independence-design.md`. Read it first.
- Branch: `feat/openai-provider`, worktree `october/horsie-openai-provider`. Never commit to `main`.
- Rust toolchain pinned to **1.96.0** (`.github/workflows/ci.yml`, `RUST_TOOLCHAIN`).
- Every PR must pass `make check` = `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`.
- Clippy runs with `-D warnings`. The workspace lint config denies `unwrap_used`, `expect_used`, `panic` and `wildcard_enum_match_arm` in non-test code — test modules in this repo carry `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]` on the `mod tests` (see `agentcore/src/error.rs:76-82`). Copy that attribute onto every new test module.
- Workspace crates are versioned `0.1.4` and reference each other by path. New crates follow suit.
- New internal crates that must never publish carry `publish = false # internal application crate, never published to crates.io` (see `providers/mock-llm/Cargo.toml`).
- `deny.toml` allows permissive licenses only and denies unknown git sources. Do not add dependencies from new git remotes.
- Every new crate must be added to `members` in the root `Cargo.toml`.
- No new CI secrets, and no live-backend job. The conformance suite runs against `mock-llm` only.

---

## File Structure

**Phase 1 — test kit + harness**
- Create `agentcore/src/testkit.rs` — `MockProvider`, `MockToolbox`, `CollectingEventSink`, moved out of `#[cfg(test)] mod tests` so other crates can use them.
- Modify `agentcore/src/lib.rs` — declare the module behind `#[cfg(any(test, feature = "test-util"))]`.
- Modify `agentcore/Cargo.toml` — add the `test-util` feature.
- Modify `agentcore/src/agent.rs` — delete the moved support types, import them from `crate::testkit`.
- Create `tests/tests/provider_conformance.rs` — the suite, Anthropic-only at first.

**Phase 2 — OpenAI wire**
- Create `providers/mock-llm/src/openai.rs` — the `/v1/chat/completions` handler + its SSE helpers.
- Modify `providers/mock-llm/src/server.rs` — mount the route, expose shared state.
- Create `providers/openai/` — new crate: `Cargo.toml`, `src/lib.rs` (provider), `src/wire.rs` (request/response types).
- Modify `tests/tests/provider_conformance.rs` — parameterize over both kinds.

**Phase 3 — capability + bug fixes**
- Modify `agentcore/src/provider.rs` — `ProviderCapabilities`, `LlmProvider::capabilities()`.
- Modify `agentcore/src/agent.rs:266` — consult it when building `tool_choice`; read `stop_reason`.
- Modify `agentcore/src/error.rs` — `AgentError::Truncated`.
- Modify `providers/openai/src/lib.rs` — declare `supports_tool_choice`.

**Phase 4 — config**
- Modify `models/fluorite/settings.fl` — `ProviderKind` enum.
- Modify `server/src/config/store.rs` — both guards + construction dispatch.
- Modify `cli/src/config.rs` — `Openai` variant.
- Modify `clients/web/src/pages/SettingsPage.tsx` — kind selector.
- Modify `docs/` — provider guide.

---

# Phase 1 — Test kit and conformance harness (PR 1)

## Task 1: Promote the test kit out of `#[cfg(test)]`

**Files:**
- Create: `agentcore/src/testkit.rs`
- Modify: `agentcore/Cargo.toml`
- Modify: `agentcore/src/lib.rs`
- Modify: `agentcore/src/agent.rs:542-697` (delete the moved support types)

**Interfaces:**
- Consumes: nothing (first task).
- Produces: `horsie_agentcore::testkit::{MockProvider, MockToolbox, CollectingEventSink}` behind feature `test-util`. Exact signatures:
  - `MockProvider::new(responses: Vec<CompletionResponse>) -> Arc<Self>`
  - `MockProvider::text(text: &str) -> Arc<Self>`
  - `MockProvider::tool_then_text(tool_id: &str, tool_name: &str, input: Value, reply: &str) -> Arc<Self>`
  - `MockToolbox::echo(name: &str) -> Arc<Self>`
  - `CollectingEventSink::new() -> Self`, `.events() -> Vec<AgentEvent>`, `.message_complete_ids() -> Vec<String>`

- [ ] **Step 1: Add the feature to `agentcore/Cargo.toml`**

Insert immediately after the `edition = "2024"` line, before `[dependencies]`:

```toml
[features]
# Exposes `testkit` (MockProvider, MockToolbox, CollectingEventSink) to other
# crates. agentcore's own unit tests get it via `cfg(test)` without the feature.
test-util = []
```

- [ ] **Step 2: Create `agentcore/src/testkit.rs`**

This is a verbatim move of the support types from `agent.rs`, with three changes: everything becomes `pub`, the accidental duplicate `#[async_trait]` on `CollectingEventSink` is dropped, and imports are rewritten for the new module path.

```rust
//! Shared test doubles for the agent loop.
//!
//! Gated behind `cfg(any(test, feature = "test-util"))`: available to
//! agentcore's own unit tests unconditionally, and to other crates (the
//! provider conformance suite, workflow, server) when they enable
//! `horsie-agentcore/test-util`.

use crate::{
    error::{LlmError, ToolCallError},
    events::{EventSink, EventSinkError},
    provider::{CompletionRequest, CompletionResponse, LlmProvider, StopReason},
    tool::{ToolSpec, Toolbox},
};
use async_trait::async_trait;
use horsie_models::agent::{ContentPart, TextPart, ToolCallPart, Usage};
use horsie_models::events::AgentEvent;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// An `LlmProvider` that replays a fixed list of responses, cycling when the
/// list is exhausted.
pub struct MockProvider {
    responses: Vec<CompletionResponse>,
    call_index: Mutex<usize>,
}

impl MockProvider {
    pub fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses,
            call_index: Mutex::new(0),
        })
    }

    pub fn text(text: &str) -> Arc<Self> {
        Self::new(vec![CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: text.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }])
    }

    pub fn tool_then_text(
        tool_id: &str,
        tool_name: &str,
        input: Value,
        reply: &str,
    ) -> Arc<Self> {
        Self::new(vec![
            CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
                    input,
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                },
            },
            CompletionResponse {
                parts: vec![ContentPart::Text(TextPart {
                    text: reply.to_string(),
                })],
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                },
            },
        ])
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_id(&self) -> &str {
        "mock-model"
    }

    async fn complete(
        &self,
        _request: CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        let mut idx = self
            .call_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let response = self.responses[*idx % self.responses.len()].clone();
        *idx += 1;
        Ok(response)
    }
}

type ToolHandler = Arc<dyn Fn(&str, Value) -> Result<Value, ToolCallError> + Send + Sync>;

/// A `Toolbox` with one tool. `echo` returns its input unchanged.
pub struct MockToolbox {
    specs: Vec<ToolSpec>,
    handler: ToolHandler,
}

impl MockToolbox {
    pub fn echo(name: &str) -> Arc<Self> {
        let spec = ToolSpec {
            name: name.to_string(),
            description: "echo tool".to_string(),
            input_schema: json!({ "type": "object" }),
        };
        Arc::new(Self {
            specs: vec![spec],
            handler: Arc::new(|_, input| Ok(input)),
        })
    }
}

#[async_trait]
impl Toolbox for MockToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        (self.handler)(name, input)
    }
}

/// An `EventSink` that records every event for later assertion.
pub struct CollectingEventSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl Default for CollectingEventSink {
    fn default() -> Self {
        Self::new()
    }
}

impl CollectingEventSink {
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn message_complete_ids(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| match e {
                AgentEvent::MessageComplete(mc) => Some(mc.message_id),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl EventSink for CollectingEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}
```

Note the `unwrap_or_else(PoisonError::into_inner)` instead of `.unwrap()`: this module is compiled as non-test code when `test-util` is on, so the workspace's `clippy::unwrap_used` lint applies.

- [ ] **Step 3: Declare the module in `agentcore/src/lib.rs`**

Add after the `mod tool;` line:

```rust
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;
```

The `any(test, ...)` is load-bearing — without it, `cargo test -p horsie-agentcore` (no features) would fail to find the kit its own unit tests use.

- [ ] **Step 4: Delete the moved types from `agent.rs` and import them**

In `agentcore/src/agent.rs`, inside `mod tests`, delete the entire `// --- support types ---` block: `struct MockProvider` through the `impl EventSink for CollectingEventSink` block (currently lines 559-697, ending just before `// --- tests ---`).

Replace the `mod tests` import block (currently lines 543-555) with:

```rust
    use super::*;
    use crate::{
        provider::{CompletionResponse, StopReason, ToolChoice},
        testkit::{CollectingEventSink, MockProvider, MockToolbox},
        tool::{EmptyToolbox, ToolSpec, Toolbox},
    };
    use async_trait::async_trait;
    use horsie_models::agent::{ContentPart, TextPart, ToolCallPart, Usage};
    use horsie_models::events::AgentEvent;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;
```

Some of these imports are still used by test-local doubles further down the file (e.g. the `tool_choice`-recording provider at `agent.rs:1180-1200`). Do not delete imports speculatively — Step 5 tells you exactly which are unused.

- [ ] **Step 5: Run agentcore tests unfeatured**

Run: `cargo test -p horsie-agentcore`
Expected: PASS, all existing tests green. If the compiler reports unused imports, delete exactly those lines and re-run.

- [ ] **Step 6: Verify the feature-gated path compiles for external consumers**

Run: `cargo build -p horsie-agentcore --features test-util`
Expected: success, no warnings.

Run: `cargo clippy -p horsie-agentcore --all-targets --features test-util -- -D warnings`
Expected: success. This is the check that catches `unwrap_used` in `testkit.rs` — the lint fires here but not under plain `cargo test`.

- [ ] **Step 7: Commit**

```bash
git add agentcore/Cargo.toml agentcore/src/lib.rs agentcore/src/testkit.rs agentcore/src/agent.rs
git commit -m "agentcore: promote test doubles to a shared testkit module"
```

---

## Task 2: Conformance suite, Anthropic only

Establishes the harness shape before a second backend exists. The suite drives a real `AnthropicProvider` against `mock-llm`'s existing `/v1/messages` route and asserts on agent-loop behavior, not wire bytes.

**Files:**
- Create: `tests/tests/provider_conformance.rs`
- Modify: `tests/Cargo.toml`

**Interfaces:**
- Consumes: `horsie_agentcore::testkit::{CollectingEventSink, MockToolbox}` from Task 1.
- Produces: `enum ProviderKind { Anthropic }` and `fn build_provider(kind: ProviderKind, base_url: &str) -> Arc<dyn LlmProvider>` — Task 6 adds the `Openai` variant to both.

- [ ] **Step 1: Add the test-util feature to the test crate's dependency**

In `tests/Cargo.toml`, change the `horsie-agentcore` line to:

```toml
horsie-agentcore = { path = "../agentcore", features = ["test-util"] }
```

- [ ] **Step 2: Write the failing suite**

Create `tests/tests/provider_conformance.rs`:

```rust
//! Provider conformance suite.
//!
//! The same agent-loop assertions run against every `LlmProvider`
//! implementation, each pointed at its own wire on a shared `mock-llm` server.
//! Assertions are behavioral (what the agent loop concluded), never wire bytes —
//! that is what makes them portable across protocols.
//!
//! Thinking-block replay is deliberately absent: it is Anthropic-only, and
//! asserting it on an OpenAI-shaped backend would be asserting a fiction. It is
//! covered by `providers/anthropic`'s own unit tests.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use horsie_agentcore::{
    Agent, AgentError, AgentInput, AgentResult, CompletedOutput, EmptyToolbox, LlmError,
    LlmProvider,
    testkit::{CollectingEventSink, MockToolbox},
};
use horsie_anthropic::AnthropicProvider;
use horsie_mock_llm::MockLlmServer;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Anthropic,
}

/// Every kind the suite runs against. Adding a variant here is the only change
/// needed to subject a new backend to the full suite.
const KINDS: &[ProviderKind] = &[ProviderKind::Anthropic];

fn build_provider(kind: ProviderKind, base_url: &str) -> Arc<dyn LlmProvider> {
    match kind {
        ProviderKind::Anthropic => Arc::new(
            AnthropicProvider::with_api_key("test-key".into())
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024)),
        ),
    }
}

/// The mock's base URL for a given wire. Both wires are served by the same
/// process on the same port; the provider appends its own path.
fn base_url_for(kind: ProviderKind, server: &MockLlmServer) -> String {
    match kind {
        ProviderKind::Anthropic => server.url(),
    }
}

async fn spawn_mock() -> MockLlmServer {
    MockLlmServer::builder().build().await
}

#[tokio::test]
async fn conformance_plain_text_turn() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_response("Hello from the mock");
        let provider = build_provider(kind, &base_url_for(kind, &server));

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let output = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: run failed: {e}"));

        match output.result {
            AgentResult::Completed(CompletedOutput { text }) => {
                assert_eq!(text, "Hello from the mock", "{kind:?}");
            }
            other => panic!(
                "{kind:?}: expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
        assert!(
            !sink.message_complete_ids().is_empty(),
            "{kind:?}: expected a MessageComplete event",
        );
    }
}

#[tokio::test]
async fn conformance_tool_call_then_text() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));
        server.queue_response("tool said 42");
        let provider = build_provider(kind, &base_url_for(kind, &server));

        let mut agent = Agent::builder(provider, MockToolbox::echo("echo"))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let output = agent
            .run(
                AgentInput::user_message("msg-1", "use the tool"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: run failed: {e}"));

        match output.result {
            AgentResult::Completed(CompletedOutput { text }) => {
                assert_eq!(text, "tool said 42", "{kind:?}");
            }
            other => panic!(
                "{kind:?}: expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }

        // Two assistant turns: the tool call, then the final text. This is the
        // portable proof the tool-result round trip reached the model — the
        // second turn only happens if the loop fed the result back.
        assert_eq!(
            sink.message_complete_ids().len(),
            2,
            "{kind:?}: expected 2 assistant messages",
        );
    }
}

#[tokio::test]
async fn conformance_rate_limit_is_classified() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        // 429 must surface as LlmError::RateLimit, not fall through to Network —
        // misclassification silently burns the retry budget.
        server.queue_error(429, "slow down");
        let provider = build_provider(kind, &base_url_for(kind, &server));

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .with_config(horsie_agentcore::AgentConfig {
                max_iterations: 1,
                ..Default::default()
            })
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let err = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .expect_err("expected the 429 to surface as an error");

        assert!(
            matches!(
                err,
                AgentError::Provider(LlmError::RateLimit { .. } | LlmError::Overloaded)
            ),
            "{kind:?}: expected RateLimit/Overloaded, got {err:?}",
        );
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p integration-tests --test provider_conformance`
Expected: FAIL to compile — `no method named queue_error found for struct MockLlmServer`. `MockLlmServer` has `queue_response` and `queue_tool_call` but no error helper (`providers/mock-llm/src/server.rs:253-269`).

- [ ] **Step 4: Add `queue_error` to the mock server**

In `providers/mock-llm/src/server.rs`, add to `impl MockLlmServer` immediately after `queue_tool_call`:

```rust
    /// Queue an error response. In streaming mode this becomes an SSE error
    /// event whose type is derived from `status` (429 → `rate_limit_error`,
    /// 529 → `overloaded_error`, else `invalid_request_error`).
    pub fn queue_error(&self, status: u16, message: impl Into<String>) {
        self.state
            .queue
            .lock()
            .push(QueueEntry::immediate(MockResponse::Error {
                status,
                message: message.into(),
            }));
    }
```

- [ ] **Step 5: Run the suite**

Run: `cargo test -p integration-tests --test provider_conformance`
Expected: PASS — 3 tests.

If `conformance_rate_limit_is_classified` hangs rather than failing, the Anthropic provider is retrying with backoff (`MAX_STREAM_RETRIES = 6`, `BACKOFF_BASE_SECS = 5` at `providers/anthropic/src/lib.rs:24-25`). Add `.with_retry_delay_secs(0)` to the `AnthropicProvider` chain in `build_provider` — the builder exists for exactly this.

- [ ] **Step 6: Full gate**

Run: `make check`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add tests/Cargo.toml tests/tests/provider_conformance.rs providers/mock-llm/src/server.rs
git commit -m "tests: add provider conformance suite (anthropic)"
```

- [ ] **Step 8: Open PR 1**

```bash
git push -u origin feat/openai-provider
gh pr create --repo blossomstack/horsie --base main \
  --title "Promote the agent test kit and add a provider conformance suite" \
  --body "$(cat <<'EOF'
First of four PRs for #16.

Moves `MockProvider`, `MockToolbox` and `CollectingEventSink` out of
`#[cfg(test)] mod tests` in `agent.rs` into `agentcore::testkit`, behind a
`test-util` feature (matching the precedent in `server/Cargo.toml`). The
`cfg(any(test, feature = "test-util"))` guard keeps them available to
agentcore's own tests unfeatured.

Adds `tests/tests/provider_conformance.rs`: agent-loop assertions that are
behavioral rather than wire-level, so they port to a second protocol unchanged.
Anthropic-only for now; PR 2 parameterizes it.

Because `integration-tests` is a workspace member and CI already runs
`--workspace --all-features`, this suite runs on every PR with no `ci.yml`
change and no secrets.

Refs #16
EOF
)"
```

---

# Phase 2 — The OpenAI wire (PR 2)

## Task 3: Serve `/v1/chat/completions` from the mock

The same `MockResponse` queue, rendered in OpenAI SSE format. One mock process, two wires, so a conformance test can point either provider at the same server and queue.

**Files:**
- Create: `providers/mock-llm/src/openai.rs`
- Modify: `providers/mock-llm/src/server.rs`
- Modify: `providers/mock-llm/src/lib.rs`

**Interfaces:**
- Consumes: `MockResponse`, `MockState`, `QueueEntry`, `ResponseKind` from `server.rs`.
- Produces: `pub(crate) async fn handle_chat_completions(State<Arc<MockState>>, HeaderMap, Json<ChatRequest>) -> ResponseKind`, mounted at `/v1/chat/completions`.

- [ ] **Step 1: Make the shared internals visible to the new module**

In `providers/mock-llm/src/server.rs`, change these four items from private to `pub(crate)`. They are currently bare `struct` / `enum` / `fn`:

```rust
pub(crate) struct MockState { /* ... unchanged fields ... */ }
pub(crate) struct QueueEntry { /* ... unchanged fields ... */ }
pub(crate) enum ResponseKind { /* ... unchanged variants ... */ }
```

and the inherent method used for dequeuing:

```rust
impl MockState {
    pub(crate) fn dequeue_entry(&self) -> Option<QueueEntry> { /* unchanged */ }
}
```

Also make `QueueEntry`'s fields `pub(crate)` (`response`, `reached`, `gate`) and `MockState`'s fields `pub(crate)` (`queue`, `scenarios`, `session_bindings`, `session_states`), and mark `sse_from_pairs` as `pub(crate) fn sse_from_pairs(...)`.

- [ ] **Step 2: Write the failing wire test**

Create `providers/mock-llm/src/openai.rs` with only the test module first, so the failure is real:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use crate::MockLlmServer;

    #[tokio::test]
    async fn chat_completions_streams_queued_text() {
        let server = MockLlmServer::builder().build().await;
        server.queue_response("hi there");

        let body = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.url()))
            .json(&serde_json::json!({
                "model": "mock-model",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(body.contains("chat.completion.chunk"), "body was: {body}");
        assert!(body.contains("hi there"), "body was: {body}");
        assert!(body.contains("[DONE]"), "body was: {body}");
    }

    #[tokio::test]
    async fn chat_completions_streams_queued_tool_call() {
        let server = MockLlmServer::builder().build().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));

        let body = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.url()))
            .json(&serde_json::json!({
                "model": "mock-model",
                "messages": [{"role": "user", "content": "go"}],
                "stream": true
            }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(body.contains("tool_calls"), "body was: {body}");
        assert!(body.contains("echo"), "body was: {body}");
        assert!(body.contains("tool_calls\"") || body.contains("finish_reason"), "body was: {body}");
    }

    #[tokio::test]
    async fn chat_completions_error_uses_http_status() {
        let server = MockLlmServer::builder().build().await;
        server.queue_error(429, "slow down");

        let resp = reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.url()))
            .json(&serde_json::json!({
                "model": "mock-model",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            }))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status().as_u16(), 429);
    }
}
```

Add `reqwest` to `providers/mock-llm/Cargo.toml` dev-dependencies:

```toml
[dev-dependencies]
reqwest = { workspace = true }
tokio   = { workspace = true, features = ["rt-multi-thread", "macros"] }
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p horsie-mock-llm`
Expected: FAIL — the route is not mounted, so the POST returns 404 and the `contains` assertions fail.

- [ ] **Step 4: Implement the handler**

Prepend to `providers/mock-llm/src/openai.rs`, above the test module:

```rust
//! The OpenAI-compatible wire (`/v1/chat/completions`), served from the same
//! `MockResponse` queue as the Anthropic route. One mock process speaks both
//! protocols so a conformance test can point either provider at one server.
//!
//! `MockResponse::Thinking` has no OpenAI equivalent and renders as an empty
//! turn — OpenAI-shaped backends have no thinking blocks.

use crate::server::{MockState, ResponseKind, sse_from_pairs};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(crate) struct ChatRequest {
    #[serde(default)]
    pub(crate) stream: Option<bool>,
}

/// SSE frames for a plain text completion.
fn text_chunks(id: &str, text: &str) -> Vec<(String, String)> {
    vec![
        (
            "message".into(),
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "delta": {"role": "assistant", "content": text},
                    "finish_reason": null
                }]
            })
            .to_string(),
        ),
        (
            "message".into(),
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
            })
            .to_string(),
        ),
        ("message".into(), "[DONE]".into()),
    ]
}

/// SSE frames for a tool call. The arguments arrive as a single delta —
/// real backends fragment them, and the provider must accumulate either way.
fn tool_call_chunks(id: &str, call_id: &str, name: &str, input: &serde_json::Value) -> Vec<(String, String)> {
    let args = serde_json::to_string(input).unwrap_or_default();
    vec![
        (
            "message".into(),
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "tool_calls": [{
                            "index": 0,
                            "id": call_id,
                            "type": "function",
                            "function": {"name": name, "arguments": args}
                        }]
                    },
                    "finish_reason": null
                }]
            })
            .to_string(),
        ),
        (
            "message".into(),
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 20, "completion_tokens": 10, "total_tokens": 30}
            })
            .to_string(),
        ),
        ("message".into(), "[DONE]".into()),
    ]
}

/// Split a text into per-chunk content deltas, then a terminal stop frame.
fn text_stream_chunks(id: &str, chunks: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = chunks
        .iter()
        .map(|c| {
            (
                "message".into(),
                serde_json::json!({
                    "id": id,
                    "object": "chat.completion.chunk",
                    "created": 0,
                    "model": "mock-model",
                    "choices": [{
                        "index": 0,
                        "delta": {"role": "assistant", "content": c},
                        "finish_reason": null
                    }]
                })
                .to_string(),
            )
        })
        .collect();
    out.push((
        "message".into(),
        serde_json::json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })
        .to_string(),
    ));
    out.push(("message".into(), "[DONE]".into()));
    out
}

pub(crate) async fn handle_chat_completions(
    State(state): State<Arc<MockState>>,
    _headers: HeaderMap,
    Json(_req): Json<ChatRequest>,
) -> ResponseKind {
    let entry = state.dequeue_entry();

    if let Some(e) = &entry {
        if let Some(r) = &e.reached {
            r.notify_one();
        }
        if let Some(g) = &e.gate {
            g.notified().await;
        }
    }

    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let call_id = format!("call_{}", uuid::Uuid::new_v4());

    match entry.map(|e| e.response) {
        Some(crate::server::MockResponse::Text { content }) => {
            sse_from_pairs(text_chunks(&id, &content))
        }
        Some(crate::server::MockResponse::TextStream { chunks }) => {
            sse_from_pairs(text_stream_chunks(&id, &chunks))
        }
        Some(crate::server::MockResponse::ToolCall { name, input }) => {
            sse_from_pairs(tool_call_chunks(&id, &call_id, &name, &input))
        }
        Some(crate::server::MockResponse::ToolCallStream { name, id: tid, input }) => {
            sse_from_pairs(tool_call_chunks(&id, &tid, &name, &input))
        }
        // No OpenAI equivalent — render as an empty assistant turn.
        Some(crate::server::MockResponse::Thinking { .. }) => {
            sse_from_pairs(text_chunks(&id, ""))
        }
        // Unlike the Anthropic route, errors are real HTTP statuses. That is
        // how OpenAI-compatible backends signal 429/5xx, and it is what the
        // provider's error classification must handle.
        Some(crate::server::MockResponse::Error { status, message }) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            ResponseKind::HttpError(
                code,
                axum::Json(serde_json::json!({
                    "error": {
                        "message": message,
                        "type": match status {
                            429 => "rate_limit_exceeded",
                            500..=599 => "server_error",
                            _ => "invalid_request_error",
                        },
                        "code": status,
                    }
                })),
            )
        }
        None => sse_from_pairs(text_chunks(&id, "No mock response queued")),
    }
}
```

- [ ] **Step 5: Mount the route and declare the module**

In `providers/mock-llm/src/lib.rs`, add:

```rust
mod openai;
```

In `providers/mock-llm/src/server.rs`, add the route inside `build()`, immediately after the `/v1/messages` line:

```rust
            .route(
                "/v1/chat/completions",
                post(crate::openai::handle_chat_completions),
            )
```

Also make `MockResponse` reachable from the sibling module — it is already `pub`, so no change is needed, but confirm `pub(crate) mod server;` or `pub mod server;` in `lib.rs` exposes it. If `lib.rs` currently re-exports via `pub use server::*;`, leave that and change `use crate::server::{...}` in `openai.rs` accordingly.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p horsie-mock-llm`
Expected: PASS — 3 tests.

- [ ] **Step 7: Commit**

```bash
git add providers/mock-llm/
git commit -m "mock-llm: serve the OpenAI chat-completions wire from the same queue"
```

---

## Task 4: The `providers/openai` crate — wire types and request mapping

**Files:**
- Create: `providers/openai/Cargo.toml`
- Create: `providers/openai/src/wire.rs`
- Create: `providers/openai/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Consumes: `horsie_agentcore::{LlmProvider, CompletionRequest, CompletionResponse, StopReason, ToolChoice, Secret, LlmError, EventSink}`.
- Produces:
  - `OpenAiProvider::new() -> Result<Self, LlmError>`
  - `OpenAiProvider::with_api_key(Secret) -> Result<Self, LlmError>`
  - `.with_model(&str) -> Self`, `.with_base_url(&str) -> Self`, `.with_max_tokens(Option<u32>) -> Self`
  - `pub const DEFAULT_MODEL: &str`, `pub const DEFAULT_MAX_TOKENS: u32`
  - `wire::{ChatMessage, ChatRequest, to_wire_messages}` — used by Task 5.

- [ ] **Step 1: Create the crate manifest**

`providers/openai/Cargo.toml`:

```toml
[package]
name = "horsie-openai"
license = "MIT OR Apache-2.0"
repository = "https://github.com/blossomstack/horsie"
description = "OpenAI-compatible chat-completions provider for horsie"
version = "0.1.4"
edition = "2024"

[dependencies]
horsie-agentcore = { version = "0.1.4", path = "../../agentcore" }
horsie-models = { version = "0.1.4", path = "../../models" }
async-trait          = { workspace = true }
tokio                = { workspace = true, features = ["time"] }
tokio-stream         = { workspace = true }
futures-util         = { workspace = true }
serde                = { workspace = true }
serde_json           = { workspace = true }
tracing              = { workspace = true }
reqwest              = { workspace = true, features = ["json", "stream"] }
reqwest-eventsource  = "0.6.0"

[dev-dependencies]
horsie-mock-llm = { path = "../mock-llm" }
tokio = { workspace = true, features = ["rt-multi-thread", "macros", "time"] }

[lints]
workspace = true
```

Add `"providers/openai",` to `members` in the root `Cargo.toml`, immediately after `"providers/anthropic",`.

Add to the root `Cargo.toml` `[workspace.dependencies]` if not already present:

```toml
reqwest-eventsource = "0.6.0"
```

(`async-llm` already depends on it, so the version is in `Cargo.lock`; declaring it at the workspace root keeps the tree deduplicated.)

- [ ] **Step 2: Write the failing mapping test**

Create `providers/openai/src/wire.rs`:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::agent::{
        ContentPart, Message, Role, TextPart, ThinkingPart, ToolCallPart, ToolResultPart,
    };

    #[test]
    fn tool_results_become_their_own_tool_role_messages() {
        // Anthropic collapses Role::Tool into `user` and carries results as
        // content blocks. OpenAI needs one `role: "tool"` message per result,
        // keyed by tool_call_id. This is the structural remap.
        let history = vec![Message {
            id: "m1".into(),
            role: Role::Tool,
            parts: vec![
                ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "call_a".into(),
                    output: "result a".into(),
                    is_error: None,
                }),
                ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "call_b".into(),
                    output: "result b".into(),
                    is_error: None,
                }),
            ],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 2, "one tool message per result");
        assert_eq!(wire[0].role, "tool");
        assert_eq!(wire[0].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(wire[0].content.as_deref(), Some("result a"));
        assert_eq!(wire[1].tool_call_id.as_deref(), Some("call_b"));
    }

    #[test]
    fn assistant_tool_calls_move_into_the_tool_calls_field() {
        let history = vec![Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "call_a".into(),
                name: "echo".into(),
                input: serde_json::json!({"v": 1}),
            })],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "assistant");
        let calls = wire[0].tool_calls.as_ref().expect("tool_calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.name, "echo");
        // Arguments are a JSON *string*, not an object — a real OpenAI quirk.
        assert_eq!(calls[0].function.arguments, r#"{"v":1}"#);
    }

    #[test]
    fn thinking_parts_are_dropped() {
        // No OpenAI equivalent. Replaying them would be sending a field the
        // backend never produced.
        let history = vec![Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![
                ContentPart::Thinking(ThinkingPart {
                    text: "hmm".into(),
                    signature: Some("sig".into()),
                }),
                ContentPart::Text(TextPart {
                    text: "answer".into(),
                }),
            ],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].content.as_deref(), Some("answer"));
    }

    #[test]
    fn user_text_maps_to_a_user_message() {
        let history = vec![Message {
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: "hi".into() })],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
        assert_eq!(wire[0].content.as_deref(), Some("hi"));
    }
}
```

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p horsie-openai`
Expected: FAIL to compile — `to_wire_messages`, `ChatMessage` etc. do not exist.

Before running, confirm `ToolResultPart`'s exact field names:

Run: `grep -n 'struct ToolResultPart' -A 8 models/src/*.rs`

If the fields differ from `tool_call_id` / `output` / `is_error`, correct the test to match the real struct — do not change the struct.

- [ ] **Step 4: Implement the wire types**

Prepend to `providers/openai/src/wire.rs`:

```rust
//! Wire types for the OpenAI-compatible `/v1/chat/completions` endpoint.
//!
//! Deliberately lenient: every response field is `#[serde(default)]` and
//! unknown fields are ignored, because "OpenAI-compatible" is a family of
//! near-misses — Ollama, vLLM and llama.cpp each omit or add fields relative
//! to OpenAI proper. Strict typing here would fail on exactly the backends
//! this crate exists to support.

use horsie_models::agent::{ContentPart, Message, Role};
use serde::{Deserialize, Serialize};

// ── request ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCall {
    pub name: String,
    /// A JSON *string*, not an object — the OpenAI schema requires this.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    fn new(role: &str, content: Option<String>) -> Self {
        Self {
            role: role.to_string(),
            content,
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

// ── response (streaming chunks) ──────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeltaToolCall {
    /// Absent on continuation frames — the index is what ties fragments
    /// together, which is why accumulation is keyed on it.
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<DeltaFunction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// Many compatible backends never send usage on stream frames.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

// ── mapping ──────────────────────────────────────────────────────────────────

/// Map horsie's provider-neutral history onto OpenAI's message list.
///
/// Three structural differences from the Anthropic mapping:
/// 1. Each `ToolResult` part becomes its own `role: "tool"` message, keyed by
///    `tool_call_id`, rather than a content block on a `user` turn.
/// 2. Assistant tool calls move into the `tool_calls` field, with arguments
///    serialized to a JSON string.
/// 3. Thinking parts are dropped — there is no equivalent.
pub fn to_wire_messages(history: &[Message]) -> Vec<ChatMessage> {
    let mut out = Vec::new();

    for msg in history {
        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();

        for part in &msg.parts {
            match part {
                ContentPart::Text(t) => text.push_str(&t.text),
                ContentPart::ToolCall(tc) => calls.push(ToolCall {
                    id: tc.id.clone(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: tc.name.clone(),
                        arguments: serde_json::to_string(&tc.input).unwrap_or_default(),
                    },
                }),
                ContentPart::ToolResult(tr) => {
                    let mut m = ChatMessage::new("tool", Some(tr.output.clone()));
                    m.tool_call_id = Some(tr.tool_call_id.clone());
                    out.push(m);
                }
                // No OpenAI equivalent.
                ContentPart::Thinking(_) => {}
            }
        }

        let role = match msg.role {
            Role::Assistant => "assistant",
            Role::User | Role::Tool => "user",
        };

        // A Tool-role message contributed only tool messages above; emitting an
        // extra empty `user` turn would be a stray message the backend charges for.
        if msg.role == Role::Tool && text.is_empty() && calls.is_empty() {
            continue;
        }
        if text.is_empty() && calls.is_empty() {
            continue;
        }

        let mut m = ChatMessage::new(role, (!text.is_empty()).then_some(text));
        if !calls.is_empty() {
            m.tool_calls = Some(calls);
        }
        out.push(m);
    }

    out
}
```

- [ ] **Step 5: Run the mapping tests**

Run: `cargo test -p horsie-openai`
Expected: PASS — 4 tests.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml providers/openai/
git commit -m "openai: add the provider crate with wire types and message mapping"
```

---

## Task 5: OpenAI streaming, response assembly, and error classification

**Files:**
- Create: `providers/openai/src/lib.rs`

**Interfaces:**
- Consumes: `wire::{ChatRequest, ChatChunk, ToolDef, FunctionDef, to_wire_messages}` from Task 4.
- Produces: `OpenAiProvider` implementing `LlmProvider`, exported as `horsie_openai::OpenAiProvider`.

- [ ] **Step 1: Write the failing end-to-end test**

Create `providers/openai/src/lib.rs` with the test module first:

```rust
#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_agentcore::{EventSink, EventSinkError};
    use horsie_mock_llm::MockLlmServer;
    use horsie_models::agent::{Message, Role, TextPart};
    use horsie_models::events::AgentEvent;
    use std::sync::Mutex;

    struct NullSink(Mutex<Vec<AgentEvent>>);

    #[async_trait]
    impl EventSink for NullSink {
        async fn emit(&self, e: AgentEvent) -> Result<(), EventSinkError> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(e);
            Ok(())
        }
    }

    fn user(text: &str) -> Message {
        Message {
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
        }
    }

    #[tokio::test]
    async fn streams_text_and_reports_end_turn() {
        let server = MockLlmServer::builder().build().await;
        server.queue_response("hello from openai");

        let provider = OpenAiProvider::with_api_key("k".into())
            .unwrap()
            .with_model("mock-model")
            .with_base_url(&server.url());

        let history = vec![user("hi")];
        let sink = NullSink(Mutex::new(Vec::new()));
        let resp = provider
            .complete(
                CompletionRequest {
                    messages: &history,
                    system: None,
                    tools: vec![],
                    tool_choice: ToolChoice::Auto,
                    max_tokens: Some(64),
                },
                "msg-1",
                &sink,
            )
            .await
            .unwrap();

        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        match &resp.parts[0] {
            ContentPart::Text(t) => assert_eq!(t.text, "hello from openai"),
            other => panic!("expected text, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[tokio::test]
    async fn streams_a_tool_call_and_reports_tool_use() {
        let server = MockLlmServer::builder().build().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));

        let provider = OpenAiProvider::with_api_key("k".into())
            .unwrap()
            .with_model("mock-model")
            .with_base_url(&server.url());

        let history = vec![user("go")];
        let sink = NullSink(Mutex::new(Vec::new()));
        let resp = provider
            .complete(
                CompletionRequest {
                    messages: &history,
                    system: None,
                    tools: vec![],
                    tool_choice: ToolChoice::Auto,
                    max_tokens: Some(64),
                },
                "msg-1",
                &sink,
            )
            .await
            .unwrap();

        assert_eq!(resp.stop_reason, StopReason::ToolUse);
        match &resp.parts[0] {
            ContentPart::ToolCall(tc) => {
                assert_eq!(tc.name, "echo");
                assert_eq!(tc.input, serde_json::json!({ "value": 42 }));
            }
            other => panic!("expected tool call, got {:?}", std::mem::discriminant(other)),
        }
    }

    #[tokio::test]
    async fn classifies_429_as_rate_limit() {
        let server = MockLlmServer::builder().build().await;
        server.queue_error(429, "slow down");

        let provider = OpenAiProvider::with_api_key("k".into())
            .unwrap()
            .with_model("mock-model")
            .with_base_url(&server.url())
            .with_retry_delay_secs(0);

        let history = vec![user("hi")];
        let sink = NullSink(Mutex::new(Vec::new()));
        let err = provider
            .complete(
                CompletionRequest {
                    messages: &history,
                    system: None,
                    tools: vec![],
                    tool_choice: ToolChoice::Auto,
                    max_tokens: Some(64),
                },
                "msg-1",
                &sink,
            )
            .await
            .expect_err("expected an error");

        assert!(
            matches!(err, LlmError::RateLimit { .. }),
            "expected RateLimit, got {err:?}",
        );
    }
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-openai`
Expected: FAIL to compile — `OpenAiProvider` does not exist.

- [ ] **Step 3: Implement the provider**

Prepend to `providers/openai/src/lib.rs`:

```rust
//! An `LlmProvider` for OpenAI-compatible `/v1/chat/completions` backends.
//!
//! One implementation targets Ollama, vLLM, llama.cpp, OpenRouter and DeepSeek.
//! It never emits `cache_control`, never replays thinking blocks, and maps tool
//! results onto their own `role: "tool"` messages — see `wire.rs`.

pub mod wire;

use async_trait::async_trait;
use futures_util::StreamExt;
use horsie_agentcore::{
    AgentEvent, CompletionRequest, CompletionResponse, ContentBlockStopEvent, ContentPart,
    EventSink, LlmError, LlmProvider, Secret, StopReason, TextBlockStartEvent, TextChunkEvent,
    TextPart, ToolCallInputDeltaEvent, ToolCallPart, ToolCallStartEvent, ToolChoice, Usage,
};
use reqwest_eventsource::{Event, EventSource};
use std::{collections::BTreeMap, env, time::Duration};
use wire::{ChatChunk, ChatRequest, FunctionDef, ToolDef, to_wire_messages};

pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const MAX_STREAM_RETRIES: u32 = 6;
const BACKOFF_BASE_SECS: u64 = 5;

pub fn env_base_url() -> Option<String> {
    env::var("OPENAI_BASE_URL").ok().filter(|s| !s.is_empty())
}

fn io_err(msg: impl std::fmt::Display) -> LlmError {
    LlmError::Network(Box::new(std::io::Error::other(msg.to_string())))
}

/// Map an HTTP status onto a classified error. Unlike the Anthropic provider,
/// which greps error *bodies*, OpenAI-compatible backends signal with real
/// status codes — so classification is exact rather than string-matched.
fn classify_status(status: u16, body: &str) -> LlmError {
    match status {
        429 => LlmError::RateLimit { retry_after: None },
        503 | 529 => LlmError::Overloaded,
        _ => LlmError::ApiError {
            status,
            message: body.to_string(),
        },
    }
}

fn is_retryable(e: &LlmError) -> bool {
    matches!(e, LlmError::RateLimit { .. } | LlmError::Overloaded)
}

pub struct OpenAiProvider {
    http: reqwest::Client,
    model: String,
    api_key: Option<Secret>,
    base_url: String,
    max_tokens: Option<u32>,
    retry_base_secs: u64,
}

impl OpenAiProvider {
    fn build(api_key: Option<Secret>) -> Result<Self, LlmError> {
        Ok(Self {
            http: reqwest::Client::builder()
                .build()
                .map_err(|e| LlmError::Network(Box::new(e)))?,
            model: DEFAULT_MODEL.to_string(),
            api_key,
            base_url: env_base_url().unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_tokens: None,
            retry_base_secs: BACKOFF_BASE_SECS,
        })
    }

    pub fn new() -> Result<Self, LlmError> {
        let key = env::var("OPENAI_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .map(Secret::from);
        Self::build(key)
    }

    pub fn with_api_key(key: Secret) -> Result<Self, LlmError> {
        Self::build(Some(key))
    }

    #[must_use]
    pub fn with_model(mut self, m: impl Into<String>) -> Self {
        self.model = m.into();
        self
    }

    #[must_use]
    pub fn with_base_url(mut self, u: impl Into<String>) -> Self {
        self.base_url = u.into().trim_end_matches('/').to_string();
        self
    }

    #[must_use]
    pub fn with_max_tokens(mut self, t: Option<u32>) -> Self {
        self.max_tokens = t;
        self
    }

    #[must_use]
    pub fn with_retry_delay_secs(mut self, secs: u64) -> Self {
        self.retry_base_secs = secs;
        self
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/chat/completions", self.base_url)
    }

    fn build_body(&self, request: &CompletionRequest<'_>) -> ChatRequest {
        let mut messages = Vec::new();
        if let Some(sys) = &request.system {
            messages.push(wire::ChatMessage {
                role: "system".to_string(),
                content: Some(sys.clone()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        messages.extend(to_wire_messages(request.messages));

        let tools: Vec<ToolDef> = request
            .tools
            .iter()
            .map(|t| ToolDef {
                kind: "function".to_string(),
                function: FunctionDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect();

        let tool_choice = if tools.is_empty() {
            None
        } else {
            match &request.tool_choice {
                ToolChoice::Auto => None,
                ToolChoice::Any => Some(serde_json::json!("required")),
                ToolChoice::Required(name) => Some(serde_json::json!({
                    "type": "function",
                    "function": { "name": name }
                })),
            }
        };

        ChatRequest {
            model: self.model.clone(),
            messages,
            stream: true,
            max_tokens: self.max_tokens.or(request.max_tokens).or(Some(DEFAULT_MAX_TOKENS)),
            tools,
            tool_choice,
        }
    }
}

/// Accumulator for one streamed tool call. Arguments arrive fragmented and are
/// only parseable once the stream ends.
#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    args: String,
    started: bool,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    fn model_id(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        let body = self.build_body(&request);
        let mut last_error: Option<LlmError> = None;

        'retry: for attempt in 0..=MAX_STREAM_RETRIES {
            if attempt > 0 {
                let delay = self.retry_base_secs * 2u64.pow(attempt - 1);
                tracing::warn!(attempt, delay_secs = delay, "OpenAI retry");
                tokio::time::sleep(Duration::from_secs(delay)).await;
            }

            let mut req = self.http.post(self.endpoint()).json(&body);
            if let Some(k) = &self.api_key {
                req = req.bearer_auth(k.expose());
            }

            let mut text = String::new();
            let mut tools: BTreeMap<usize, ToolAcc> = BTreeMap::new();
            let mut stop_reason = StopReason::EndTurn;
            let mut usage = Usage {
                input_tokens: 0,
                output_tokens: 0,
            };
            let mut text_started = false;
            let mut emitted_anything = false;

            let mut es = EventSource::new(req).map_err(io_err)?;

            while let Some(ev) = es.next().await {
                match ev {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(m)) => {
                        if m.data.trim() == "[DONE]" {
                            break;
                        }
                        // Lenient by design: a frame we cannot parse is skipped,
                        // not fatal. Compatible backends emit keepalives and
                        // vendor-specific frames that are not chat chunks.
                        let Ok(chunk) = serde_json::from_str::<ChatChunk>(&m.data) else {
                            continue;
                        };

                        if let Some(u) = &chunk.usage {
                            usage.input_tokens = u.prompt_tokens;
                            usage.output_tokens = u.completion_tokens;
                        }

                        for choice in &chunk.choices {
                            if let Some(c) = &choice.delta.content
                                && !c.is_empty()
                            {
                                if !text_started {
                                    text_started = true;
                                    events
                                        .emit(AgentEvent::TextBlockStart(TextBlockStartEvent {
                                            message_id: message_id.to_string(),
                                            index: 0,
                                        }))
                                        .await?;
                                }
                                text.push_str(c);
                                emitted_anything = true;
                                events
                                    .emit(AgentEvent::TextChunk(TextChunkEvent {
                                        message_id: message_id.to_string(),
                                        index: 0,
                                        text: c.clone(),
                                    }))
                                    .await?;
                            }

                            for tc in choice.delta.tool_calls.iter().flatten() {
                                let acc = tools.entry(tc.index).or_default();
                                if let Some(id) = &tc.id {
                                    acc.id = id.clone();
                                }
                                if let Some(f) = &tc.function {
                                    if let Some(n) = &f.name {
                                        acc.name = n.clone();
                                    }
                                    if let Some(a) = &f.arguments {
                                        acc.args.push_str(a);
                                    }
                                }
                                if !acc.started && !acc.id.is_empty() && !acc.name.is_empty() {
                                    acc.started = true;
                                    emitted_anything = true;
                                    events
                                        .emit(AgentEvent::ToolCallStart(ToolCallStartEvent {
                                            message_id: message_id.to_string(),
                                            index: u32::try_from(tc.index).unwrap_or(0),
                                            tool_call_id: acc.id.clone(),
                                            name: acc.name.clone(),
                                        }))
                                        .await?;
                                }
                                if let Some(f) = &tc.function
                                    && let Some(a) = &f.arguments
                                    && !a.is_empty()
                                {
                                    events
                                        .emit(AgentEvent::ToolCallInputDelta(
                                            ToolCallInputDeltaEvent {
                                                message_id: message_id.to_string(),
                                                index: u32::try_from(tc.index).unwrap_or(0),
                                                partial_json: a.clone(),
                                            },
                                        ))
                                        .await?;
                                }
                            }

                            if let Some(fr) = &choice.finish_reason {
                                stop_reason = match fr.as_str() {
                                    "tool_calls" | "function_call" => StopReason::ToolUse,
                                    "length" => StopReason::MaxTokens,
                                    _ => StopReason::EndTurn,
                                };
                            }
                        }
                    }
                    Err(reqwest_eventsource::Error::StreamEnded) => break,
                    Err(reqwest_eventsource::Error::InvalidStatusCode(status, resp)) => {
                        let code = status.as_u16();
                        let body_text = resp.text().await.unwrap_or_default();
                        let err = classify_status(code, &body_text);
                        // Only retry when nothing has been emitted — re-running
                        // a partially streamed turn would duplicate content.
                        if is_retryable(&err) && !emitted_anything {
                            last_error = Some(err);
                            es.close();
                            continue 'retry;
                        }
                        es.close();
                        return Err(err);
                    }
                    Err(e) => {
                        let err = io_err(e);
                        es.close();
                        return Err(err);
                    }
                }
            }

            es.close();

            let mut parts: Vec<ContentPart> = Vec::new();
            if !text.is_empty() {
                events
                    .emit(AgentEvent::ContentBlockStop(ContentBlockStopEvent {
                        message_id: message_id.to_string(),
                        index: 0,
                    }))
                    .await?;
                parts.push(ContentPart::Text(TextPart { text: text.clone() }));
            }
            for (_, acc) in tools {
                let input = if acc.args.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&acc.args).unwrap_or(serde_json::Value::Null)
                };
                parts.push(ContentPart::ToolCall(ToolCallPart {
                    id: acc.id,
                    name: acc.name,
                    input,
                }));
            }

            // Some backends report `stop` even when they emitted tool calls.
            if parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolCall(_)))
                && stop_reason == StopReason::EndTurn
            {
                stop_reason = StopReason::ToolUse;
            }

            return Ok(CompletionResponse {
                parts,
                stop_reason,
                usage,
            });
        }

        Err(last_error.unwrap_or_else(|| io_err("stream retries exhausted")))
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-openai`
Expected: PASS — 7 tests (4 mapping + 3 streaming).

If `Secret::expose` is not the accessor name, check `agentcore/src/secret.rs` and use the real one. If `AgentEvent::TextChunk` / `ToolCallInputDelta` variant or field names differ, check `models/src/events.rs` and correct the call sites — the events crate is the source of truth.

- [ ] **Step 5: Lint**

Run: `cargo clippy -p horsie-openai --all-targets --all-features -- -D warnings`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add providers/openai/
git commit -m "openai: implement streaming completion with status-based error classification"
```

---

## Task 6: Run the conformance suite on both kinds

**Files:**
- Modify: `tests/tests/provider_conformance.rs`
- Modify: `tests/Cargo.toml`

**Interfaces:**
- Consumes: `horsie_openai::OpenAiProvider` from Task 5.
- Produces: `KINDS` containing both variants — every existing conformance test now runs twice.

- [ ] **Step 1: Add the dependency**

In `tests/Cargo.toml`, add after the `horsie-anthropic` line:

```toml
horsie-openai = { path = "../providers/openai" }
```

- [ ] **Step 2: Add the variant — this is the failing step**

In `tests/tests/provider_conformance.rs`, make three edits:

```rust
use horsie_openai::OpenAiProvider;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Anthropic,
    Openai,
}

const KINDS: &[ProviderKind] = &[ProviderKind::Anthropic, ProviderKind::Openai];

fn build_provider(kind: ProviderKind, base_url: &str) -> Arc<dyn LlmProvider> {
    match kind {
        ProviderKind::Anthropic => Arc::new(
            AnthropicProvider::with_api_key("test-key".into())
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024))
                .with_retry_delay_secs(0),
        ),
        ProviderKind::Openai => Arc::new(
            OpenAiProvider::with_api_key("test-key".into())
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024))
                .with_retry_delay_secs(0),
        ),
    }
}
```

`base_url_for` needs no change — both providers take the same server root and append their own path (`/v1/messages` vs `/v1/chat/completions`). Delete `base_url_for` and call `server.url()` directly if the compiler flags it as trivially wrapping; otherwise leave it, since it documents the intent.

- [ ] **Step 3: Run — expect real failures**

Run: `cargo test -p integration-tests --test provider_conformance`
Expected: the three tests now run twice each. This is where genuine wire bugs surface. Debug against the panic messages, which all carry `{kind:?}`.

If `conformance_tool_call_then_text` fails for `Openai` with 1 assistant message instead of 2, the tool-result round trip is broken — check `to_wire_messages` is emitting the `role: "tool"` message and that `mock-llm` dequeued the second queued response.

- [ ] **Step 4: Add a multi-turn history test**

Append to `tests/tests/provider_conformance.rs`:

```rust
#[tokio::test]
async fn conformance_multi_turn_history_is_replayed() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_response("first");
        server.queue_response("second");
        let provider = build_provider(kind, &base_url_for(kind, &server));

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        agent
            .run(
                AgentInput::user_message("msg-1", "one"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: first run failed: {e}"));

        let output = agent
            .run(
                AgentInput::user_message("msg-2", "two"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|e| panic!("{kind:?}: second run failed: {e}"));

        match output.result {
            AgentResult::Completed(CompletedOutput { text }) => {
                assert_eq!(text, "second", "{kind:?}");
            }
            other => panic!(
                "{kind:?}: expected Completed, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }
}
```

- [ ] **Step 5: Run the full suite**

Run: `cargo test -p integration-tests --test provider_conformance`
Expected: PASS — 4 tests × 2 kinds.

- [ ] **Step 6: Full gate**

Run: `make check`
Expected: PASS.

- [ ] **Step 7: Commit and open PR 2**

```bash
git add tests/
git commit -m "tests: run the conformance suite against both wire protocols"
git push
gh pr create --repo blossomstack/horsie --base main \
  --title "Add an OpenAI-compatible provider and prove it via the conformance suite" \
  --body "$(cat <<'EOF'
Second of four PRs for #16.

Adds `providers/openai`: a hand-rolled `/v1/chat/completions` client
(`reqwest` + `reqwest-eventsource`) with deliberately lenient deserialization,
so one implementation covers Ollama, vLLM, llama.cpp, OpenRouter and DeepSeek.
Unparseable stream frames are skipped rather than fatal.

Adds `/v1/chat/completions` to `mock-llm`, serving the *same* `MockResponse`
queue as the Anthropic route. One mock process, two wires.

The conformance suite now runs every assertion against both kinds. Structural
differences handled inside the new crate: tool results become their own
`role: "tool"` messages, thinking parts are dropped, errors classify off HTTP
status rather than string-matched bodies.

Refs #16
EOF
)"
```

---

# Phase 3 — Capability flag and two bug fixes (PR 3)

## Task 7: `ProviderCapabilities`, consumed where `tool_choice` is built

The issue points at `workflow/src/agent_actor.rs:240`, but the actual construction site is `agentcore/src/agent.rs:266` — fixing it there covers the workflow *and* interactive paths in one place, and the existing nudge-and-retry safety net (`agent.rs:344`) is already the correct fallback for a model that answers with text instead of the forced call.

**Files:**
- Modify: `agentcore/src/provider.rs`
- Modify: `agentcore/src/lib.rs`
- Modify: `agentcore/src/agent.rs:266`
- Modify: `providers/openai/src/lib.rs`

**Interfaces:**
- Produces: `horsie_agentcore::ProviderCapabilities { supports_tool_choice: bool }`, `LlmProvider::capabilities(&self) -> ProviderCapabilities` with a default impl returning fully-capable.

- [ ] **Step 1: Write the failing test**

Append to `agentcore/src/agent.rs`'s `mod tests`:

```rust
    /// A provider that reports it cannot honor `tool_choice`, and records what
    /// the loop asked for anyway.
    struct NoToolChoiceProvider {
        seen: Mutex<Option<ToolChoice>>,
    }

    #[async_trait]
    impl crate::provider::LlmProvider for NoToolChoiceProvider {
        fn model_id(&self) -> &str {
            "no-tool-choice"
        }

        fn capabilities(&self) -> crate::provider::ProviderCapabilities {
            crate::provider::ProviderCapabilities {
                supports_tool_choice: false,
            }
        }

        async fn complete(
            &self,
            request: crate::provider::CompletionRequest<'_>,
            _message_id: &str,
            _events: &dyn EventSink,
        ) -> Result<CompletionResponse, crate::error::LlmError> {
            *self.seen.lock().unwrap() = Some(request.tool_choice.clone());
            Ok(CompletionResponse {
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "call-1".into(),
                    name: "done".into(),
                    input: json!({}),
                })],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            })
        }
    }

    #[tokio::test]
    async fn test_forced_handoff_falls_back_to_auto_when_unsupported() {
        // A forced handoff would normally send tool_choice=Any. A backend that
        // cannot honor it (Ollama, llama.cpp) must get Auto instead — the
        // nudge-and-retry net already covers a text-only reply.
        let provider = Arc::new(NoToolChoiceProvider {
            seen: Mutex::new(None),
        });
        let mut agent = Agent::builder(provider.clone(), MockToolbox::echo("done"))
            .with_handoff_tool("done")
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        agent
            .run(
                AgentInput::user_message("msg-1", "go"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        let seen = provider.seen.lock().unwrap().clone();
        assert!(
            matches!(seen, Some(ToolChoice::Auto)),
            "expected Auto, got {seen:?}",
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-agentcore test_forced_handoff_falls_back_to_auto_when_unsupported`
Expected: FAIL to compile — `ProviderCapabilities` does not exist and `LlmProvider` has no `capabilities` method.

- [ ] **Step 3: Add the capability type**

In `agentcore/src/provider.rs`, add above the `LlmProvider` trait:

```rust
/// What a backend can do, so the agent loop can degrade rather than send a
/// request the backend will reject.
///
/// Only fields with a live consumer belong here. Anthropic-specific behavior
/// that is already encapsulated in its own provider crate (prompt caching,
/// thinking replay, error-body classification) needs no flag — the trait
/// boundary is doing that job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderCapabilities {
    /// Whether the backend honors `tool_choice`. OpenAI proper does; Ollama and
    /// llama.cpp may ignore or reject it. When false, a forced handoff degrades
    /// to `Auto` and relies on the loop's nudge-and-retry.
    pub supports_tool_choice: bool,
}

impl Default for ProviderCapabilities {
    fn default() -> Self {
        Self {
            supports_tool_choice: true,
        }
    }
}
```

And add to the `LlmProvider` trait, after `model_id`:

```rust
    /// Defaults to fully capable — an existing provider needs no change.
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
```

Export it from `agentcore/src/lib.rs` by extending the `provider` re-export line:

```rust
pub use provider::{
    CompletionRequest, CompletionResponse, LlmProvider, ProviderCapabilities, StopReason,
    ToolChoice,
};
```

- [ ] **Step 4: Consume it at the construction site**

In `agentcore/src/agent.rs`, replace the `tool_choice` binding at line 266 (keeping the existing comment above it, and extending it):

```rust
        // With a *forced* handoff tool, require a tool call every turn (`Any`) so
        // the model can never end with bare text — it must keep working or call the
        // handoff tool to finish. Without one (or with an optional handoff tool —
        // see `with_handoff_tool_optional`), the model may end its turn with text
        // (`Auto`) exactly as freely as it may call any other tool.
        //
        // A backend that cannot honor `tool_choice` at all gets `Auto` regardless:
        // sending a directive it rejects fails the call outright, whereas `Auto`
        // plus the nudge-and-retry below degrades gracefully.
        let tool_choice = if self.handoff_tool.is_some()
            && self.force_handoff_choice
            && self.provider.capabilities().supports_tool_choice
        {
            ToolChoice::Any
        } else {
            ToolChoice::Auto
        };
```

- [ ] **Step 5: Run the test**

Run: `cargo test -p horsie-agentcore`
Expected: PASS, including the new test and the existing `test_handoff_agent_forces_tool_choice_any` (which uses a default-capability provider and must still see `Any`).

- [ ] **Step 6: Declare the OpenAI provider's capability**

The OpenAI wire *does* support `tool_choice`, so the default is already correct. Add an explicit impl anyway to document the decision — in `providers/openai/src/lib.rs`, inside `impl LlmProvider for OpenAiProvider`, after `model_id`:

```rust
    fn capabilities(&self) -> horsie_agentcore::ProviderCapabilities {
        // The OpenAI schema defines `tool_choice`, and OpenRouter/DeepSeek/vLLM
        // honor it. Ollama and llama.cpp are inconsistent; when that bites, this
        // becomes configurable rather than hardcoded.
        horsie_agentcore::ProviderCapabilities {
            supports_tool_choice: true,
        }
    }
```

- [ ] **Step 7: Commit**

```bash
git add agentcore/ providers/openai/
git commit -m "agentcore: degrade forced tool_choice on backends that cannot honor it"
```

---

## Task 8: Read `stop_reason` — stop treating truncation as success

**Files:**
- Modify: `agentcore/src/error.rs`
- Modify: `agentcore/src/agent.rs:335`

**Interfaces:**
- Produces: `AgentError::Truncated { max_tokens: Option<u32> }`.

- [ ] **Step 1: Write the failing test**

Append to `agentcore/src/agent.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn test_max_tokens_truncation_is_an_error_not_a_completion() {
        // stop_reason was computed but never read in production: a response cut
        // off by max_tokens looked exactly like a normal end of turn, so the
        // caller received a silently truncated answer as a success.
        let provider = MockProvider::new(vec![CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: "half an ans".into(),
            })],
            stop_reason: StopReason::MaxTokens,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }]);
        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();

        let err = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .expect_err("truncation must not be reported as a completed turn");

        assert!(
            matches!(err, AgentError::Truncated { .. }),
            "expected Truncated, got {err:?}",
        );
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p horsie-agentcore test_max_tokens_truncation_is_an_error_not_a_completion`
Expected: FAIL — `AgentError::Truncated` does not exist.

- [ ] **Step 3: Add the error variant**

In `agentcore/src/error.rs`, add to `enum AgentError`, before `Cancelled`:

```rust
    /// The backend stopped because it hit the output-token ceiling. The partial
    /// text is not a valid answer, so the run fails rather than returning it as
    /// a completed turn.
    #[error("response truncated at the max_tokens limit ({max_tokens:?})")]
    Truncated { max_tokens: Option<u32> },
```

- [ ] **Step 4: Read `stop_reason` in the loop**

In `agentcore/src/agent.rs`, immediately after `self.history.push(assistant_msg);` (line 331) and before `let tool_calls = extract_tool_calls(&response.parts);` (line 333), insert:

```rust
            // A truncated turn is not a finished turn. Tool calls are exempt:
            // a backend may report `length` alongside a complete tool call, and
            // the loop can still execute it and continue.
            if response.stop_reason == StopReason::MaxTokens
                && extract_tool_calls(&response.parts).is_empty()
            {
                return Err(AgentError::Truncated {
                    max_tokens: self.config.max_tokens,
                });
            }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p horsie-agentcore`
Expected: PASS. If an existing test used a `MockProvider` response with `StopReason::MaxTokens` expecting success, that test was asserting the bug — update it to expect `Truncated` and note why in a comment.

- [ ] **Step 6: Add conformance coverage**

Append to `tests/tests/provider_conformance.rs`:

```rust
#[tokio::test]
async fn conformance_max_tokens_truncation_surfaces() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_truncated("cut off here");
        let provider = build_provider(kind, &base_url_for(kind, &server));

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .build()
            .unwrap();
        let sink = CollectingEventSink::new();
        let err = agent
            .run(
                AgentInput::user_message("msg-1", "hi"),
                &sink,
                CancellationToken::new(),
            )
            .await
            .expect_err("truncation must surface");

        assert!(
            matches!(err, AgentError::Truncated { .. }),
            "{kind:?}: expected Truncated, got {err:?}",
        );
    }
}
```

This needs a new `MockResponse` variant. In `providers/mock-llm/src/server.rs`, add to `enum MockResponse`:

```rust
    /// A response cut off by the output-token ceiling: `stop_reason: max_tokens`
    /// on the Anthropic wire, `finish_reason: length` on the OpenAI wire.
    Truncated {
        content: String,
    },
```

Add the queue helper to `impl MockLlmServer`:

```rust
    pub fn queue_truncated(&self, content: impl Into<String>) {
        self.state
            .queue
            .lock()
            .push(QueueEntry::immediate(MockResponse::Truncated {
                content: content.into(),
            }));
    }
```

In the Anthropic handler, add an arm rendering `text_sse` with `stop_reason` `"max_tokens"` — copy `text_sse` to a `truncated_sse` that differs only in the `message_delta` frame. In `openai.rs`, add an arm using a copy of `text_chunks` whose terminal frame carries `"finish_reason": "length"`.

Both handlers currently have `unreachable!()` arms for variants they do not expect — add `Truncated` explicitly to every `match` on `MockResponse` in both files, or the build fails on non-exhaustive matches. That compile error is the guardrail; follow it.

- [ ] **Step 7: Run everything**

Run: `make check`
Expected: PASS.

- [ ] **Step 8: Commit and open PR 3**

```bash
git add agentcore/ providers/ tests/
git commit -m "agentcore: treat max_tokens truncation as an error"
git push
gh pr create --repo blossomstack/horsie --base main \
  --title "Add a provider capability flag and fix two provider-boundary bugs" \
  --body "$(cat <<'EOF'
Third of four PRs for #16.

Adds `ProviderCapabilities { supports_tool_choice }` with a fully-capable
default impl, so existing providers need no change. Consumed at
`agentcore/src/agent.rs:266` — the site where `tool_choice` is actually built.
The issue pointed at `workflow/src/agent_actor.rs:240`, but fixing it in
agentcore covers the workflow *and* interactive paths at once, and the existing
nudge-and-retry net is already the right fallback for a text-only reply.

Deliberately one field. Four of the five breakages the issue lists
(`cache_control`, role collapsing, error-body matching, thinking replay) are
already encapsulated in their provider crates and need no flag.

Also fixes `stop_reason` never being read in production: every consumer in
`agent.rs` was `#[cfg(test)]`, so a `max_tokens` truncation was silently
returned as a normal completed turn. It is now `AgentError::Truncated`, with
conformance coverage on both wires.

Refs #16
EOF
)"
```

---

# Phase 4 — Configuration and docs (PR 4)

## Task 9: Make `kind` a closed set on the settings path

Per the spec's §6: an enum, not a tagged union, because both provider kinds have identical config.

**Files:**
- Modify: `models/fluorite/settings.fl:112`
- Modify: `server/src/config/store.rs:356`, `:613`, `:631`
- Modify: `clients/ts/` and `clients/web/src/generated/` (regenerated, not hand-edited)

**Interfaces:**
- Produces: `ProviderKind` in the fluorite schema, serialized as `"anthropic"` / `"openai"`.

- [ ] **Step 1: Read how fluorite enums are declared**

Run: `grep -n 'enum ' models/fluorite/*.fl | head -20`

Use whatever declaration form the existing schemas use. Do not invent syntax — if no plain enum exists in any `.fl` file, keep `kind: String` in the schema and enforce the closed set in `store.rs` only, then say so in the PR body.

- [ ] **Step 2: Write the failing guard test**

Find the existing store tests:

Run: `grep -n 'mod tests' -A 5 server/src/config/store.rs`

Add a test asserting an `openai` provider round-trips through `update` and `view`, and that a bogus kind is still rejected:

```rust
    #[tokio::test]
    async fn openai_provider_kind_is_accepted() {
        let store = test_store().await;
        let view = store
            .update(SettingsUpdate {
                providers: Some(vec![ProviderInput {
                    name: "local".into(),
                    kind: "openai".into(),
                    base_url: Some("http://127.0.0.1:11434".into()),
                    api_key: Some("k".into()),
                }]),
                ..Default::default()
            })
            .await
            .expect("openai must be an accepted provider kind");

        assert_eq!(view.providers.len(), 1);
        assert_eq!(view.providers[0].kind, "openai");
    }

    #[tokio::test]
    async fn unknown_provider_kind_is_still_rejected() {
        let store = test_store().await;
        let err = store
            .update(SettingsUpdate {
                providers: Some(vec![ProviderInput {
                    name: "bogus".into(),
                    kind: "cohere".into(),
                    base_url: None,
                    api_key: None,
                }]),
                ..Default::default()
            })
            .await
            .expect_err("unknown kinds must be rejected");

        assert!(err.contains("cohere"), "error was: {err}");
    }
```

Adapt `test_store()` and `SettingsUpdate` construction to whatever the existing tests in that file already do — mirror them exactly rather than inventing a helper.

- [ ] **Step 3: Run to verify it fails**

Run: `cargo test -p horsie-server openai_provider_kind_is_accepted`
Expected: FAIL — `unsupported provider kind 'openai' (only 'anthropic')`.

- [ ] **Step 4: Widen the write-path guard**

In `server/src/config/store.rs`, replace the check at line 356:

```rust
                if !matches!(p.kind.as_str(), "anthropic" | "openai") {
                    return Err(format!(
                        "unsupported provider kind '{}' (expected 'anthropic' or 'openai')",
                        p.kind
                    ));
                }
```

- [ ] **Step 5: Widen the load-path guard and dispatch construction**

Replace the `build_registry` kind check (line 613) and the construction call with a dispatch:

```rust
        let max_tokens = m.max_tokens.and_then(|v| u32::try_from(v).ok());
        let built = match p.kind.as_str() {
            "anthropic" => build_anthropic(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
            )?,
            "openai" => build_openai(
                p.base_url.as_deref(),
                p.api_key.as_deref(),
                &m.model_id,
                max_tokens,
            )?,
            other => {
                return Err(format!(
                    "provider '{}' has unsupported kind '{other}'",
                    p.name
                ));
            }
        };
        reg.insert(m.alias.clone(), built);
```

Add the builder next to `build_anthropic`:

```rust
fn build_openai(
    base_url: Option<&str>,
    api_key: Option<&str>,
    model_id: &str,
    max_tokens: Option<u32>,
) -> Result<Arc<dyn LlmProvider>, String> {
    let key: Option<Secret> = match api_key {
        Some(k) if !k.is_empty() => Some(Secret::from(k)),
        Some(_) => return Err("inline api_key is empty".into()),
        None => None,
    };
    let mut p = match key {
        Some(k) => OpenAiProvider::with_api_key(k).map_err(|e| e.to_string())?,
        None => OpenAiProvider::new().map_err(|e| e.to_string())?,
    };
    p = p.with_model(model_id).with_max_tokens(max_tokens);
    if let Some(u) = base_url {
        p = p.with_base_url(u);
    }
    Ok(Arc::new(p))
}
```

Add `horsie-openai = { path = "../providers/openai" }` to `server/Cargo.toml` and the matching `use horsie_openai::OpenAiProvider;` import.

- [ ] **Step 6: Run**

Run: `cargo test -p horsie-server`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add models/fluorite/ server/
git commit -m "server: accept and construct openai providers from settings"
```

---

## Task 10: CLI config gains an `Openai` variant

**Files:**
- Modify: `cli/src/config.rs:73`

- [ ] **Step 1: Add the variant**

In `cli/src/config.rs`, add to `enum ProviderConfig` after the `Anthropic` variant:

```rust
    /// An OpenAI-compatible `/v1/chat/completions` provider — OpenAI itself,
    /// or Ollama, vLLM, llama.cpp, OpenRouter, DeepSeek via `base_url`. Key
    /// resolution matches `Anthropic`: inline `api_key`, else the env var named
    /// by `api_key_env`, else unauthenticated (for a local server).
    Openai {
        #[serde(default)]
        api_key: Option<Secret>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
    },
```

- [ ] **Step 2: Follow the compile errors**

Run: `cargo build -p cli`
Expected: FAIL — every `match` on `ProviderConfig` is now non-exhaustive. The compiler lists them; add an `Openai` arm to each, constructing an `OpenAiProvider` exactly as the `Anthropic` arm constructs an `AnthropicProvider`.

Add `horsie-openai = { path = "../providers/openai" }` to `cli/Cargo.toml`.

- [ ] **Step 3: Verify**

Run: `cargo test -p cli && cargo clippy -p cli --all-targets --all-features -- -D warnings`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add cli/
git commit -m "cli: add an openai provider variant to config"
```

---

## Task 11: Settings UI kind selector

**Files:**
- Modify: `clients/web/src/pages/SettingsPage.tsx:214`

- [ ] **Step 1: Regenerate the TS types**

Run: `cd clients/web && bun install && bun run generate-types`
Expected: success. If `ProviderKind` landed in the schema in Task 9, it appears in `src/generated`.

- [ ] **Step 2: Add `kind` to the provider form state**

Find the provider row state (search for `apiKeyInput` in the file — it is the provider row shape). Add a `kind` field defaulting to `"anthropic"`, and include it wherever a blank provider row is created.

- [ ] **Step 3: Replace the hardcoded kind at submit**

At line 214, replace:

```tsx
      kind: "anthropic",
```

with:

```tsx
      kind: p.kind,
```

- [ ] **Step 4: Render the selector**

In the provider row JSX, next to the name and base-URL inputs, add a select. Match the surrounding inputs' Tailwind classes exactly — copy them from the adjacent `<input>`:

```tsx
<select
  value={p.kind}
  onChange={(e) => updateProvider(i, { kind: e.target.value })}
  className="<copy the sibling input's className verbatim>"
>
  <option value="anthropic">Anthropic</option>
  <option value="openai">OpenAI-compatible</option>
</select>
```

Use whatever the file's existing per-row update helper is called — do not invent `updateProvider` if the file names it something else.

- [ ] **Step 5: Typecheck and build**

Run: `make web-build`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add clients/web/
git commit -m "web: add a provider kind selector to Settings"
```

---

## Task 12: Document the second backend

**Files:**
- Create or modify: `docs/guide/providers.md` (check `docs/guide/` for the existing structure first)

- [ ] **Step 1: Check where provider config is already documented**

Run: `ls docs/guide/ && grep -rln 'anthropic' docs/`

Write into whichever existing guide covers provider/model configuration. Create `docs/guide/providers.md` only if none exists.

- [ ] **Step 2: Write the section**

Cover, with real copy-pasteable config:

- The two kinds, `anthropic` and `openai`, and what each speaks.
- An Ollama example: `{"type": "openai", "base_url": "http://127.0.0.1:11434"}` with no API key.
- An OpenRouter/DeepSeek example using `api_key_env`.
- Known limits, stated plainly: streaming is required; thinking blocks are Anthropic-only and are dropped on the OpenAI wire; prompt caching is Anthropic-only.
- How to run the conformance suite: `cargo test -p integration-tests --test provider_conformance`, and that it covers both wires against `mock-llm` with no network access or key.

- [ ] **Step 3: Full gate**

Run: `make check && make web-build`
Expected: PASS.

- [ ] **Step 4: Commit and open PR 4**

```bash
git add docs/
git commit -m "docs: document the openai-compatible provider"
git push
gh pr create --repo blossomstack/horsie --base main \
  --title "Make provider kind configurable end to end" \
  --body "$(cat <<'EOF'
Last of four PRs for #16.

`kind` becomes a closed set rather than a free-form string: both `store.rs`
guards accept `anthropic` and `openai`, construction dispatches on it, the CLI
config enum gains an `Openai` variant, and the Settings UI gets a selector
instead of a hardcoded `kind: "anthropic"`.

Deviation from the issue's wording, deliberate and covered in the spec: the
settings path uses an enum, not a tagged union. `VendorConfigInput` earns a
union because velos carries ~10 fields no other vendor has; the two provider
kinds have *identical* config (`name`, `base_url`, `api_key`), so a union there
would be two variants with the same payload plus a DB migration, buying
nothing. Widen it when a backend with genuinely different config lands.

Closes #16
EOF
)"
```

---

## Post-merge: manual live verification

Not a CI job — per the design, live-backend testing is manual and happens once the change is ready.

- [ ] Run `ollama serve` and `ollama pull qwen2.5:7b` (any tool-calling model).
- [ ] Configure a provider `{"type": "openai", "base_url": "http://127.0.0.1:11434"}` and a model alias pointing at the pulled model.
- [ ] Drive one interactive session turn that calls a tool, through the web UI.
- [ ] Run one workflow (`examples/dev-workflow.json`) — this is the path that exercises the forced-`tool_choice` degradation.
- [ ] Record what broke. Ollama's `tool_choice` handling is the most likely failure, and is exactly what `supports_tool_choice` exists to turn off.

---

## Self-Review Notes

Spec coverage check, section by section:

| Spec section | Task |
|---|---|
| §1 Shared test kit | Task 1 |
| §2 Two-wire mock + conformance suite | Tasks 2, 3, 6 |
| §3 OpenAI provider | Tasks 4, 5 |
| §4 Capability surface (one field) | Task 7 |
| §5 Forced `tool_choice` fix | Task 7 |
| §5 `stop_reason` fix | Task 8 |
| §6 Config — settings path | Task 9 |
| §6 Config — CLI path | Task 10 |
| §6 Settings UI selector | Task 11 |
| Docs + known limits | Task 12 |
| CI, both kinds, no secrets | Task 2 Step 5 (no `ci.yml` change needed) |

Naming consistency verified across tasks: `ProviderKind` (test enum, Tasks 2/6), `ProviderCapabilities.supports_tool_choice` (Task 7, consumed Task 7 Step 4, declared Task 7 Step 6), `AgentError::Truncated { max_tokens }` (Task 8, asserted Tasks 8 Step 1 and Step 6), `to_wire_messages` (Task 4, consumed Task 5), `queue_error` (Task 2 Step 4, used Tasks 3/5/6), `queue_truncated` (Task 8 Step 6).

Two places the plan deliberately tells the implementer to verify against reality rather than trusting the plan: fluorite enum syntax (Task 9 Step 1) and `ToolResultPart` field names (Task 4 Step 3). Both are cases where guessing would produce a confident wrong answer.
