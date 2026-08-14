//! Shared test doubles for the agent loop.
//!
//! Gated behind `cfg(any(test, feature = "test-util"))`: available to
//! agentcore's own unit tests unconditionally, and to other crates (the
//! provider conformance suite, workflow, server) when they enable
//! `horsie-agentcore/test-util`.

pub mod script;
pub use script::{Script, ScriptExhausted};

use crate::{
    error::{LlmError, ToolCallError},
    events::{EventSink, EventSinkError},
    provider::{CompletionRequest, CompletionResponse, LlmProvider, StopReason},
    tool::{ToolOutcome, ToolSpec, Toolbox},
};
use async_trait::async_trait;
use horsie_models::agent::{ContentPart, Message, Role, TextPart, ToolCallPart, Usage};
use horsie_models::events::AgentEvent;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, PoisonError};

/// A summary of one `CompletionRequest`, captured because the request itself is
/// borrowed and cannot be stored. Enough to assert *what the model was asked* —
/// which is the only way to catch a retry that rebuilds history from the wrong
/// place (#61 item 21).
#[derive(Debug, Clone, PartialEq)]
pub struct RequestSummary {
    /// What the caller named this conversation. Recorded so a test can pin the
    /// plumbing from `Agent::builder` through to the wire.
    pub conversation_id: String,
    pub message_count: usize,
    pub roles: Vec<Role>,
    pub tool_call_ids: Vec<String>,
    pub tool_result_ids: Vec<String>,
    /// Every text part in the prompt, in order. What the model was *told*, as
    /// opposed to how the prompt was shaped — the only way to assert that
    /// content assembled outside the transcript (a translated hook record, say)
    /// actually arrived.
    pub texts: Vec<String>,
}

impl RequestSummary {
    fn of(conversation_id: &str, messages: &[Message]) -> Self {
        let mut tool_call_ids = Vec::new();
        let mut tool_result_ids = Vec::new();
        let mut texts = Vec::new();
        for message in messages {
            for part in &message.parts {
                match part {
                    ContentPart::ToolCall(c) => tool_call_ids.push(c.id.clone()),
                    ContentPart::ToolResult(r) => tool_result_ids.push(r.tool_call_id.clone()),
                    ContentPart::Text(t) => texts.push(t.text.clone()),
                    ContentPart::Thinking(_) | ContentPart::SubAgentResult(_) => {}
                }
            }
        }
        Self {
            conversation_id: conversation_id.to_string(),
            message_count: messages.len(),
            roles: messages.iter().map(|m| m.role.clone()).collect(),
            tool_call_ids,
            tool_result_ids,
            texts,
        }
    }
}

/// An `LlmProvider` that replays a [`Script`] of programmed outcomes and records
/// what it was asked.
pub struct MockProvider {
    script: Script<Result<CompletionResponse, LlmError>>,
    requests: Mutex<Vec<RequestSummary>>,
}

impl MockProvider {
    /// Replay `script`. When it runs out, every further call returns
    /// `LlmError::ApiError { status: 500 }` naming the exhausted script — a loud,
    /// attributable failure rather than a silent repeat.
    pub fn scripted(script: Script<Result<CompletionResponse, LlmError>>) -> Arc<Self> {
        Arc::new(Self {
            script,
            requests: Mutex::new(Vec::new()),
        })
    }

    /// Replay `responses` in order, erroring once they run out.
    ///
    /// Strict on purpose: the previous implementation cycled, so a test that
    /// over-ran its script silently received a repeated response instead of
    /// failing — the mechanism that hides iteration-count bugs (#61 R6).
    pub fn new(responses: Vec<CompletionResponse>) -> Arc<Self> {
        Self::scripted(Script::of(responses.into_iter().map(Ok)).labelled("MockProvider::new"))
    }

    /// Return `response` to every call — a steady state, said out loud.
    ///
    /// For loop-control tests (max iterations, stuck detection, handoff retries)
    /// that need the model to keep answering the same way.
    pub fn always(response: CompletionResponse) -> Arc<Self> {
        Self::scripted(Script::of([]).then_repeating_with(move || Ok(response.clone())))
    }

    /// Fail every call with an error carrying `err`'s status and message.
    pub fn failing(err: LlmError) -> Arc<Self> {
        let message = err.to_string();
        let status = match err {
            LlmError::ApiError { status, .. } => status,
            LlmError::RateLimit { .. } => 429,
            LlmError::Overloaded => 529,
            LlmError::Network(_) | LlmError::EventSink(_) => 502,
        };
        Self::scripted(Script::of([]).then_repeating_with(move || {
            Err(LlmError::ApiError {
                status,
                message: message.clone(),
            })
        }))
    }

    /// A provider that answers `text` on every call.
    pub fn text(text: &str) -> Arc<Self> {
        let response = CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: text.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(10, 5),
        };
        Self::scripted(Script::of([]).then_repeating_with(move || Ok(response.clone())))
    }

    /// One tool call, then `reply` on every later call.
    pub fn tool_then_text(tool_id: &str, tool_name: &str, input: Value, reply: &str) -> Arc<Self> {
        let first = CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: tool_id.to_string(),
                name: tool_name.to_string(),
                input,
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(20, 10),
        };
        let steady = CompletionResponse {
            parts: vec![ContentPart::Text(TextPart {
                text: reply.to_string(),
            })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(30, 8),
        };
        Self::scripted(Script::of([Ok(first)]).then_repeating_with(move || Ok(steady.clone())))
    }

    /// How many completions have been requested.
    pub fn calls(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// A summary of every request, in order.
    pub fn requests(&self) -> Vec<RequestSummary> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn model_id(&self) -> &str {
        "mock-model"
    }

    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        _message_id: &str,
        _events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(RequestSummary::of(
                request.conversation_id,
                request.messages,
            ));
        match self.script.next_step() {
            Ok(outcome) => outcome,
            Err(exhausted) => Err(LlmError::ApiError {
                status: 500,
                message: exhausted.to_string(),
            }),
        }
    }
}

/// Executes a tool call by name. Returning `Err` exercises the loop's
/// tool-failure path; returning [`ToolOutcome::StopRun`] exercises a tool that
/// ends the run.
pub type ToolHandler = Arc<dyn Fn(&str, Value) -> Result<ToolOutcome, ToolCallError> + Send + Sync>;

/// A `Toolbox` backed by an arbitrary handler closure.
pub struct MockToolbox {
    specs: Vec<ToolSpec>,
    handler: ToolHandler,
}

impl MockToolbox {
    /// A toolbox advertising `specs`, dispatching every call to `handler`.
    pub fn new(specs: Vec<ToolSpec>, handler: ToolHandler) -> Arc<Self> {
        Arc::new(Self { specs, handler })
    }

    /// One tool named `name` that returns its input unchanged.
    pub fn echo(name: &str) -> Arc<Self> {
        let spec = ToolSpec {
            name: name.to_string(),
            description: "echo tool".to_string(),
            input_schema: json!({ "type": "object" }),
        };
        Arc::new(Self {
            specs: vec![spec],
            handler: Arc::new(|_, input| Ok(ToolOutcome::Result(input))),
        })
    }

    /// Two tools: `work`, which echoes its input, and `stopper`, which ends the
    /// run. The pair every terminal-tool test needs.
    pub fn with_stopper(stopper: &str) -> Arc<Self> {
        let stopper = stopper.to_string();
        let spec = |name: &str| ToolSpec {
            name: name.to_string(),
            description: "mock tool".to_string(),
            input_schema: json!({ "type": "object" }),
        };
        let stops = stopper.clone();
        Arc::new(Self {
            specs: vec![spec("work"), spec(&stopper)],
            handler: Arc::new(move |name, input| {
                if name == stops {
                    Ok(ToolOutcome::StopRun)
                } else {
                    Ok(ToolOutcome::Result(input))
                }
            }),
        })
    }
}

#[async_trait]
impl Toolbox for MockToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        _tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        (self.handler)(name, input)
    }
}

/// An `EventSink` that fails after `allow` successful emits.
///
/// Models a journal that cannot write. The agent loop treats a sink failure as
/// fatal — proceeding would build on a history that was never recorded — so this
/// is the double for "the disk went away mid-turn" (#61 item 22).
pub struct FailingEventSink {
    allow: usize,
    seen: Mutex<usize>,
    reason: String,
}

impl FailingEventSink {
    /// Fail every emit.
    pub fn always(reason: impl Into<String>) -> Self {
        Self::after(0, reason)
    }

    /// Succeed `allow` times, then fail every emit after.
    pub fn after(allow: usize, reason: impl Into<String>) -> Self {
        Self {
            allow,
            seen: Mutex::new(0),
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl EventSink for FailingEventSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        *seen += 1;
        if *seen > self.allow {
            return Err(EventSinkError(self.reason.clone()));
        }
        Ok(())
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
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn message_complete_ids(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter_map(|e| {
                if let AgentEvent::MessageComplete(mc) = e {
                    Some(mc.message_id)
                } else {
                    None
                }
            })
            .collect()
    }
}

#[async_trait]
impl EventSink for CollectingEventSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        self.events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::provider::ToolChoice;
    use horsie_models::agent::{ToolCallPart as TcPart, ToolResultPart};

    fn user_msg(id: &str, text: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: id.to_string(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart {
                text: text.to_string(),
            })],
        }
    }

    async fn call(
        provider: &MockProvider,
        messages: &[Message],
    ) -> Result<CompletionResponse, LlmError> {
        let sink = CollectingEventSink::new();
        provider
            .complete(
                CompletionRequest {
                    messages,
                    system: None,
                    tools: vec![],
                    tool_choice: ToolChoice::Auto,
                    max_tokens: None,
                    thinking_effort: None,
                    conversation_id: "test-conversation",
                },
                "msg-1",
                &sink as &dyn EventSink,
            )
            .await
    }

    #[tokio::test]
    async fn scripted_provider_yields_then_errors_on_exhaustion() {
        let p = MockProvider::scripted(Script::of([
            Ok(CompletionResponse {
                parts: vec![ContentPart::Text(TextPart { text: "one".into() })],
                stop_reason: StopReason::EndTurn,
                usage: Usage::without_cache(1, 1),
            }),
            Err(LlmError::Overloaded),
        ]));
        let msgs = vec![user_msg("m1", "hi")];

        assert!(call(&p, &msgs).await.is_ok());
        assert!(matches!(call(&p, &msgs).await, Err(LlmError::Overloaded)));
        // Third call: the script is spent. A cycling double would silently repeat.
        assert!(matches!(
            call(&p, &msgs).await,
            Err(LlmError::ApiError { status: 500, .. })
        ));
    }

    #[tokio::test]
    async fn failing_provider_always_errors() {
        let p = MockProvider::failing(LlmError::ApiError {
            status: 400,
            message: "context length exceeded".into(),
        });
        let msgs = vec![user_msg("m1", "hi")];
        for _ in 0..3 {
            assert!(matches!(
                call(&p, &msgs).await,
                Err(LlmError::ApiError { status: 400, .. })
            ));
        }
    }

    #[tokio::test]
    async fn records_what_each_call_was_asked() {
        let p = MockProvider::text("ok");
        let first = vec![user_msg("m1", "hi")];
        let second = vec![
            user_msg("m1", "hi"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "m2".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(TcPart {
                    id: "call-1".into(),
                    name: "echo".into(),
                    input: json!({}),
                })],
            },
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "m3".into(),
                role: Role::Tool,
                parts: vec![ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "call-1".into(),
                    output: "done".into(),
                    is_error: false,
                })],
            },
        ];

        let _ = call(&p, &first).await;
        let _ = call(&p, &second).await;

        let seen = p.requests();
        assert_eq!(p.calls(), 2);
        assert_eq!(seen[0].message_count, 1);
        assert_eq!(seen[0].roles, vec![Role::User]);
        assert_eq!(seen[1].message_count, 3);
        assert_eq!(seen[1].tool_call_ids, vec!["call-1".to_string()]);
        assert_eq!(seen[1].tool_result_ids, vec!["call-1".to_string()]);
    }

    #[tokio::test]
    async fn existing_constructors_still_repeat() {
        // `text()` is used by tests that call it more than once; it must not become
        // strict, or migrating the suite becomes a rewrite.
        let p = MockProvider::text("hello");
        let msgs = vec![user_msg("m1", "hi")];
        assert!(call(&p, &msgs).await.is_ok());
        assert!(call(&p, &msgs).await.is_ok());
    }
}
