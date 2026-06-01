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

use agentcore::{
    Agent, AgentError, AgentEvent, AgentInput, AgentResult, CompletedOutput, ContentPart,
    EventSink, EventSinkError, HandoffOutput, ToolCallError, ToolSpec, Toolbox,
};
use anthropic::AnthropicProvider;
use async_trait::async_trait;
use mock_llm::MockLlmServer;
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
        })
        .collect()
}

/// A toolbox that records invocations and returns a fixed JSON string.
struct FixedToolbox {
    specs: Vec<ToolSpec>,
    output: serde_json::Value,
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
    ) -> Result<serde_json::Value, ToolCallError> {
        self.calls.lock().unwrap().push((name.to_string(), input));
        Ok(self.output.clone())
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Agent receives a plain text response and returns Completed with that text.
#[tokio::test]
async fn test_simple_text_completion() {
    let mock = MockLlmServer::builder()
        .response("Hello, world!")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(provider, Arc::new(agentcore::EmptyToolbox))
        .build()
        .unwrap();
    let sink = CollectSink::new();

    let output = agent
        .run(AgentInput::user_message("msg-1", "hi"), &sink, cancel())
        .await
        .unwrap();

    assert!(
        matches!(output.result, AgentResult::Completed(CompletedOutput { ref text }) if text == "Hello, world!")
    );
}

/// Event sequence for a single-turn text response must be exactly:
/// InputMessage → MessageStart → TextChunk(s) → MessageStop → MessageComplete → RunComplete
#[tokio::test]
async fn test_text_turn_event_sequence() {
    let mock = MockLlmServer::builder().response("done").build().await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(provider, Arc::new(agentcore::EmptyToolbox))
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
async fn test_text_chunks_message_id_and_content() {
    let mock = MockLlmServer::builder()
        .response_stream(["Hello", " ", "world"])
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(provider, Arc::new(agentcore::EmptyToolbox))
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
async fn test_tool_call_cycle() {
    let mock = MockLlmServer::builder()
        .tool_call("search", serde_json::json!({"q": "rust"}))
        .response("found it")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("search", serde_json::json!("search result"));
    let mut agent = Agent::builder(provider, toolbox.clone()).build().unwrap();
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
async fn test_tool_turn_event_sequence() {
    let mock = MockLlmServer::builder()
        .tool_call("lookup", serde_json::json!({"id": 1}))
        .response("here is the result")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("lookup", serde_json::json!("data"));
    let mut agent = Agent::builder(provider, toolbox).build().unwrap();
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
async fn test_tool_call_id_consistency() {
    let mock = MockLlmServer::builder()
        .tool_call("calc", serde_json::json!({"x": 7}))
        .response("result: 7")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("calc", serde_json::json!(7));
    let mut agent = Agent::builder(provider, toolbox).build().unwrap();
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
async fn test_message_complete_contains_full_message() {
    let mock = MockLlmServer::builder()
        .response("the answer")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(provider, Arc::new(agentcore::EmptyToolbox))
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

    assert_eq!(mc.message.role, agentcore::Role::Assistant);
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
async fn test_run_complete_usage_and_iterations() {
    // Two iterations: first a tool call, then text.
    let mock = MockLlmServer::builder()
        .tool_call("noop", serde_json::json!({}))
        .response("done")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("noop", serde_json::json!(null));
    let mut agent = Agent::builder(provider, toolbox).build().unwrap();
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

/// Agent with a handoff tool returns Handoff result immediately without executing the tool.
#[tokio::test]
async fn test_agent_handoff() {
    let mock = MockLlmServer::builder()
        .tool_call("delegate", serde_json::json!({"task": "summarise"}))
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    // The handoff tool must be advertised in the toolbox.
    let toolbox = FixedToolbox::new("delegate", serde_json::json!(null));
    let mut agent = Agent::builder(provider, toolbox)
        .with_handoff_tool("delegate")
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
        AgentResult::Handoff(HandoffOutput { tool_name, data }) => {
            assert_eq!(tool_name, "delegate");
            assert_eq!(data["task"], "summarise");
        }
        other => panic!("expected Handoff, got {:?}", std::mem::discriminant(&other)),
    }

    // No ToolExecuting emitted for a handoff
    assert!(
        !sink
            .events()
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecuting(_)))
    );
}

/// Exactly one RunComplete event is emitted per run() call.
#[tokio::test]
async fn test_exactly_one_run_complete() {
    let mock = MockLlmServer::builder().response("ok").build().await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(provider, Arc::new(agentcore::EmptyToolbox))
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
async fn test_agent_transparent_retry_on_overload() {
    let mock = MockLlmServer::builder()
        .error(529, "overloaded_error")
        .response("recovered")
        .build()
        .await;
    let provider = Arc::new(provider_at(&mock.url()));
    let mut agent = Agent::builder(provider, Arc::new(agentcore::EmptyToolbox))
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
async fn test_cancellation() {
    let mock = MockLlmServer::builder().response("never").build().await;
    let provider = Arc::new(provider_at(&mock.url()));
    let toolbox = FixedToolbox::new("t", serde_json::json!(null));
    let mut agent = Agent::builder(provider, toolbox).build().unwrap();
    let sink = CollectSink::new();
    let token = CancellationToken::new();
    token.cancel();

    let err = agent
        .run(AgentInput::user_message("msg-1", "go"), &sink, token)
        .await
        .unwrap_err();
    assert!(matches!(err, AgentError::Cancelled));
}
