//! End-to-end tests for Agent + AnthropicProvider + MockLlmServer.
//!
//! Each test spins up a real Axum SSE mock, wires AnthropicProvider to it,
//! builds an Agent, calls run(), and asserts on both the final result and the
//! sequence of events emitted to the EventSink.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use async_llm::mock::MockLlmServer;
use async_trait::async_trait;
use horsie_agentcore::{
    Agent, AgentError, AgentEvent, AgentInput, AgentResult, CompletedOutput, ContentPart,
    EventSink, EventSinkError, StoppedOutput, ToolCallError, ToolOutcome, ToolSpec, Toolbox,
};
use horsie_llm_providers::anthropic::AnthropicProvider;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

// ── shared helpers ────────────────────────────────────────────────────────────

struct CollectSink(Mutex<Vec<AgentEvent>>);

impl CollectSink {
    fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }
    fn events(&self) -> Vec<AgentEvent> {
        self.0.lock().unwrap().clone()
    }
}

#[async_trait]
impl EventSink for CollectSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        self.0.lock().unwrap().push(event);
        Ok(())
    }
}

fn provider_at(url: &str) -> AnthropicProvider {
    AnthropicProvider::with_api_key("test-key")
        .unwrap()
        .with_base_url(url)
        .with_retry_delay_secs(0)
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

/// Returns event type names in emission order for readable assertions.
fn event_kinds(events: &[AgentEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            AgentEvent::InputMessage(_) => "InputMessage",
            AgentEvent::MessageStart(_) => "MessageStart",
            AgentEvent::MessageStop(_) => "MessageStop",
            AgentEvent::MessageComplete(_) => "MessageComplete",
            AgentEvent::TextBlockStart(_) => "TextBlockStart",
            AgentEvent::TextChunk(_) => "TextChunk",
            AgentEvent::ThinkingBlockStart(_) => "ThinkingBlockStart",
            AgentEvent::ThinkingChunk(_) => "ThinkingChunk",
            AgentEvent::ThinkingSignatureChunk(_) => "ThinkingSignatureChunk",
            AgentEvent::ToolCallStart(_) => "ToolCallStart",
            AgentEvent::ToolCallInputDelta(_) => "ToolCallInputDelta",
            AgentEvent::ContentBlockStop(_) => "ContentBlockStop",
            AgentEvent::ToolExecuting(_) => "ToolExecuting",
            AgentEvent::ToolComplete(_) => "ToolComplete",
            AgentEvent::RunComplete(_) => "RunComplete",
            AgentEvent::RunAborted(_) => "RunAborted",
            AgentEvent::Compacted(_) => "Compacted",
            AgentEvent::CompactionSkipped(_) => "CompactionSkipped",
        })
        .collect()
}

/// A toolbox that records invocations and returns a fixed JSON string.
struct FixedToolbox {
    specs: Vec<ToolSpec>,
    output: serde_json::Value,
    /// The tool that ends the run instead of returning a value.
    stopper: Option<String>,
    calls: Mutex<Vec<(String, serde_json::Value)>>,
}

impl FixedToolbox {
    fn new(name: &str, output: serde_json::Value) -> Arc<Self> {
        Arc::new(Self {
            specs: vec![ToolSpec {
                name: name.to_string(),
                description: format!("{name} tool"),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            output,
            stopper: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// One tool, which ends the run when called.
    fn stopping(name: &str) -> Arc<Self> {
        Arc::new(Self {
            specs: vec![ToolSpec {
                name: name.to_string(),
                description: format!("{name} tool"),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            output: serde_json::Value::Null,
            stopper: Some(name.to_string()),
            calls: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, serde_json::Value)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Toolbox for FixedToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        _tool_call_id: &str,
    ) -> Result<ToolOutcome, ToolCallError> {
        self.calls.lock().unwrap().push((name.to_string(), input));
        if self.stopper.as_deref() == Some(name) {
            return Ok(ToolOutcome::StopRun);
        }
        Ok(ToolOutcome::result(self.output.clone()))
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

// A plain text turn over this wire is `provider_conformance.rs`'s
// `conformance_plain_text_turn`, which makes the same assertions against every
// provider rather than only this one. What lives here is what that suite
// deliberately does not cover: the agent's event sequence, its handoff and
// retry behaviour, and the ids that tie a tool call to its result.

/// Event sequence for a single-turn text response must be exactly:
/// InputMessage → MessageStart → TextChunk(s) → MessageStop → MessageComplete → RunComplete
#[tokio::test]
async fn a_text_turn_emits_its_events_in_order() {
    let mock = MockLlmServer::builder().response("done").build().await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(
        provider,
        Arc::new(horsie_agentcore::EmptyToolbox),
        "test-conversation",
    )
    .build()
    .unwrap();
    let sink = CollectSink::new();

    agent
        .run(AgentInput::user_message("msg-1", "go"), &sink, cancel())
        .await
        .unwrap();

    let kinds = event_kinds(&sink.events());

    // Structural checks
    assert_eq!(kinds[0], "InputMessage");
    assert_eq!(kinds[1], "MessageStart");
    assert_eq!(*kinds.last().unwrap(), "RunComplete");

    // MessageStop and MessageComplete come after all streaming content
    let stop_pos = kinds.iter().rposition(|&k| k == "MessageStop").unwrap();
    let complete_pos = kinds.iter().rposition(|&k| k == "MessageComplete").unwrap();
    let run_complete_pos = kinds.iter().rposition(|&k| k == "RunComplete").unwrap();
    assert!(stop_pos < complete_pos);
    assert!(complete_pos < run_complete_pos);

    // At least one TextChunk was emitted before MessageStop
    let first_chunk_pos = kinds.iter().position(|&k| k == "TextChunk").unwrap();
    assert!(first_chunk_pos < stop_pos);
}

/// TextChunk events carry the right message_id and the assembled text matches the response.
#[tokio::test]
async fn text_chunks_carry_the_message_id_and_reassemble_the_text() {
    let mock = MockLlmServer::builder()
        .response_stream(["Hello", " ", "world"])
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(
        provider,
        Arc::new(horsie_agentcore::EmptyToolbox),
        "test-conversation",
    )
    .build()
    .unwrap();
    let sink = CollectSink::new();

    agent
        .run(AgentInput::user_message("msg-1", "hi"), &sink, cancel())
        .await
        .unwrap();

    let events = sink.events();

    // Capture the message_id from MessageStart
    let start_id = events
        .iter()
        .find_map(|e| {
            if let AgentEvent::MessageStart(s) = e {
                Some(s.message_id.clone())
            } else {
                None
            }
        })
        .unwrap();

    // All TextChunk events share that message_id and assemble to the full text
    let chunks: Vec<String> = events
        .iter()
        .filter_map(|e| {
            if let AgentEvent::TextChunk(c) = e {
                assert_eq!(c.message_id, start_id, "TextChunk message_id mismatch");
                Some(c.text.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(chunks.join(""), "Hello world");
}

/// Agent performs a tool call cycle: tool call → tool execution → follow-up text.
#[tokio::test]
async fn a_tool_is_dispatched_with_the_arguments_the_model_sent() {
    let mock = MockLlmServer::builder()
        .tool_call("search", serde_json::json!({"q": "rust"}))
        .response("found it")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("search", serde_json::json!("search result"));
    let mut agent = Agent::builder(provider, toolbox.clone(), "test-conversation")
        .build()
        .unwrap();
    let sink = CollectSink::new();

    let output = agent
        .run(
            AgentInput::user_message("msg-1", "search rust"),
            &sink,
            cancel(),
        )
        .await
        .unwrap();

    assert!(
        matches!(output.result, AgentResult::Completed(CompletedOutput { ref text }) if text == "found it")
    );

    // Tool was actually executed
    let calls = toolbox.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "search");
    assert_eq!(calls[0].1["q"], "rust");
}

/// Full event sequence for a tool-call turn:
/// InputMessage
/// MessageStart → ToolCallStart → ToolCallInputDelta(s) → ContentBlockStop → MessageStop → MessageComplete
/// ToolExecuting → ToolComplete
/// MessageStart → TextBlockStart → TextChunk(s) → ContentBlockStop → MessageStop → MessageComplete
/// RunComplete
#[tokio::test]
async fn a_tool_turn_emits_its_events_in_order() {
    let mock = MockLlmServer::builder()
        .tool_call("lookup", serde_json::json!({"id": 1}))
        .response("here is the result")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("lookup", serde_json::json!("data"));
    let mut agent = Agent::builder(provider, toolbox, "test-conversation")
        .build()
        .unwrap();
    let sink = CollectSink::new();

    agent
        .run(
            AgentInput::user_message("msg-1", "lookup 1"),
            &sink,
            cancel(),
        )
        .await
        .unwrap();

    let kinds = event_kinds(&sink.events());

    // Helpers: position of first/last occurrence
    let pos = |k: &str| kinds.iter().position(|&x| x == k).unwrap();
    let rpos = |k: &str| kinds.iter().rposition(|&x| x == k).unwrap();

    // Global boundaries
    assert_eq!(kinds[0], "InputMessage");
    assert_eq!(*kinds.last().unwrap(), "RunComplete");

    // Turn 1 (tool call): ToolCallStart before the block's ContentBlockStop before
    // the first MessageStop (the first content-block stop is the tool block's).
    let first_stop = pos("MessageStop");
    assert!(pos("ToolCallStart") < pos("ContentBlockStop"));
    assert!(pos("ContentBlockStop") < first_stop);

    // Tool execution comes after the first MessageComplete
    let first_complete = pos("MessageComplete");
    assert!(pos("ToolExecuting") > first_complete);
    assert!(pos("ToolComplete") > pos("ToolExecuting"));

    // Turn 2 (text): TextChunk before the final MessageStop/MessageComplete
    let last_stop = rpos("MessageStop");
    let last_complete = rpos("MessageComplete");
    assert!(rpos("TextChunk") < last_stop);
    assert!(last_stop < last_complete);
    assert!(last_complete < pos("RunComplete"));
}

/// Tool call IDs are consistent: ToolCallStart, ToolExecuting, ToolComplete
/// all carry the same tool_call_id.
#[tokio::test]
async fn one_tool_call_id_ties_the_start_the_execution_and_the_result() {
    let mock = MockLlmServer::builder()
        .tool_call("calc", serde_json::json!({"x": 7}))
        .response("result: 7")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("calc", serde_json::json!(7));
    let mut agent = Agent::builder(provider, toolbox, "test-conversation")
        .build()
        .unwrap();
    let sink = CollectSink::new();

    agent
        .run(AgentInput::user_message("msg-1", "calc"), &sink, cancel())
        .await
        .unwrap();

    let events = sink.events();

    let start = events
        .iter()
        .find_map(|e| {
            if let AgentEvent::ToolCallStart(s) = e {
                Some(s.clone())
            } else {
                None
            }
        })
        .expect("ToolCallStart");

    let executing = events
        .iter()
        .find_map(|e| {
            if let AgentEvent::ToolExecuting(x) = e {
                Some(x.clone())
            } else {
                None
            }
        })
        .expect("ToolExecuting");

    let complete = events
        .iter()
        .find_map(|e| {
            if let AgentEvent::ToolComplete(c) = e {
                Some(c.clone())
            } else {
                None
            }
        })
        .expect("ToolComplete");

    assert_eq!(executing.tool_call_id, start.tool_call_id);
    assert_eq!(complete.tool_call_id, start.tool_call_id);
    assert_eq!(complete.output, "7");
    assert!(!complete.is_error);
}

/// MessageComplete carries the full assembled message with the correct role and content.
#[tokio::test]
async fn message_complete_carries_the_whole_assistant_message() {
    let mock = MockLlmServer::builder()
        .response("the answer")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(
        provider,
        Arc::new(horsie_agentcore::EmptyToolbox),
        "test-conversation",
    )
    .build()
    .unwrap();
    let sink = CollectSink::new();

    agent
        .run(AgentInput::user_message("msg-1", "q"), &sink, cancel())
        .await
        .unwrap();

    let mc = sink
        .events()
        .into_iter()
        .find_map(|e| {
            if let AgentEvent::MessageComplete(m) = e {
                Some(m)
            } else {
                None
            }
        })
        .expect("MessageComplete");

    assert_eq!(mc.message.role, horsie_agentcore::Role::Assistant);
    let text: String = mc
        .message
        .parts
        .iter()
        .filter_map(|p| {
            if let ContentPart::Text(t) = p {
                Some(t.text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, "the answer");
}

/// RunComplete carries accumulated usage and correct iteration count.
#[tokio::test]
async fn run_complete_reports_the_iteration_count_and_the_usage() {
    // Two iterations: first a tool call, then text.
    let mock = MockLlmServer::builder()
        .tool_call("noop", serde_json::json!({}))
        .response("done")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("noop", serde_json::json!(null));
    let mut agent = Agent::builder(provider, toolbox, "test-conversation")
        .build()
        .unwrap();
    let sink = CollectSink::new();

    agent
        .run(AgentInput::user_message("msg-1", "go"), &sink, cancel())
        .await
        .unwrap();

    let rc = sink
        .events()
        .into_iter()
        .find_map(|e| {
            if let AgentEvent::RunComplete(r) = e {
                Some(r)
            } else {
                None
            }
        })
        .expect("RunComplete");

    assert_eq!(rc.iterations, 2);
    assert!(rc.usage.input_tokens > 0);
    assert!(rc.usage.output_tokens > 0);
}

/// A tool answering `StopRun` ends the run, and the loop reports which tool it
/// was and what it was called with.
#[tokio::test]
async fn a_stopping_tool_ends_the_run_naming_the_tool_and_its_input() {
    let mock = MockLlmServer::builder()
        .tool_call("delegate", serde_json::json!({"task": "summarise"}))
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::stopping("delegate");
    let mut agent = Agent::builder(provider, toolbox, "test-conversation")
        .build()
        .unwrap();
    let sink = CollectSink::new();

    let output = agent
        .run(
            AgentInput::user_message("msg-1", "delegate"),
            &sink,
            cancel(),
        )
        .await
        .unwrap();

    match output.result {
        AgentResult::Stopped(StoppedOutput { calls }) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].tool, "delegate");
            assert_eq!(calls[0].input["task"], "summarise");
        }
        other => panic!("expected Stopped, got {:?}", std::mem::discriminant(&other)),
    }

    // The call is dispatched, so it starts — but it records no result, which is
    // what leaves the `tool_use` dangling for an answer to arrive against.
    assert!(
        !sink
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolComplete(_))),
        "a stopping call records no result"
    );
}

/// Exactly one RunComplete event is emitted per run() call.
#[tokio::test]
async fn a_run_emits_run_complete_exactly_once() {
    let mock = MockLlmServer::builder().response("ok").build().await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(
        provider,
        Arc::new(horsie_agentcore::EmptyToolbox),
        "test-conversation",
    )
    .build()
    .unwrap();
    let sink = CollectSink::new();

    agent
        .run(AgentInput::user_message("msg-1", "hi"), &sink, cancel())
        .await
        .unwrap();

    let count = sink
        .events()
        .iter()
        .filter(|e| matches!(e, AgentEvent::RunComplete(_)))
        .count();
    assert_eq!(count, 1);
}

/// Retry on 529 overload: the agent transparently retries and succeeds.
#[tokio::test]
async fn an_overload_is_retried_without_the_caller_seeing_it() {
    let mock = MockLlmServer::builder()
        .error(529, "overloaded_error")
        .response("recovered")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(
        provider,
        Arc::new(horsie_agentcore::EmptyToolbox),
        "test-conversation",
    )
    .build()
    .unwrap();
    let sink = CollectSink::new();

    let output = agent
        .run(AgentInput::user_message("msg-1", "hi"), &sink, cancel())
        .await
        .unwrap();

    assert!(
        matches!(output.result, AgentResult::Completed(CompletedOutput { ref text }) if text == "recovered")
    );
}

/// Cancellation before the first provider call returns AgentError::Cancelled.
#[tokio::test]
async fn a_cancelled_run_ends_as_cancelled() {
    let mock = MockLlmServer::builder().response("never").build().await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("t", serde_json::json!(null));
    let mut agent = Agent::builder(provider, toolbox, "test-conversation")
        .build()
        .unwrap();
    let sink = CollectSink::new();
    let token = CancellationToken::new();
    token.cancel();

    let err = agent
        .run(AgentInput::user_message("msg-1", "go"), &sink, token)
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::Cancelled));
}
