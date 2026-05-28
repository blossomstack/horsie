#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use agentcore::{
    AgentEvent, CompletionRequest, ContentPart, EventSink, LlmProvider, StopReason, ToolChoice,
    ToolSpec,
};
use anthropic::AnthropicProvider;
use mock_llm::MockLlmServer;
use models::agent::{Message, Role, TextPart};
use std::sync::{Arc, Mutex};

struct CollectSink(Arc<Mutex<Vec<AgentEvent>>>);

impl EventSink for CollectSink {
    fn emit(&self, event: AgentEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn collect_sink() -> (CollectSink, Arc<Mutex<Vec<AgentEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    (CollectSink(events.clone()), events)
}

fn provider_at(url: &str) -> AnthropicProvider {
    AnthropicProvider::with_api_key("test-key")
        .unwrap()
        .with_base_url(url)
        .with_retry_delay_secs(0)
}

fn user_messages(text: &str) -> Vec<Message> {
    vec![Message {
        id: "m1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart { text: text.into() })],
    }]
}

fn no_tools_request(messages: &[Message]) -> CompletionRequest<'_> {
    CompletionRequest {
        messages,
        system: None,
        tools: vec![],
        tool_choice: ToolChoice::Auto,
        max_tokens: None,
    }
}

// ── text response ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_text_response() {
    let mock = MockLlmServer::builder()
        .response("Hello world")
        .build()
        .await;
    let p = provider_at(&mock.url());
    let msgs = user_messages("hi");
    let (sink, events) = collect_sink();
    let resp = p
        .complete(no_tools_request(&msgs), "msg-1", &sink)
        .await
        .unwrap();
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    let text: String = resp
        .parts
        .iter()
        .filter_map(|p| {
            if let ContentPart::Text(t) = p {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, "Hello world");
    let evts = events.lock().unwrap();
    assert!(evts.iter().any(|e| matches!(e, AgentEvent::TextChunk(_))));
}

// ── tool call ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_tool_call_response() {
    let input = serde_json::json!({"q": "rust async"});
    let mock = MockLlmServer::builder()
        .tool_call("search", input.clone())
        .build()
        .await;
    let p = provider_at(&mock.url());
    let msgs = user_messages("search rust");
    let (sink, events) = collect_sink();
    let resp = p
        .complete(no_tools_request(&msgs), "msg-1", &sink)
        .await
        .unwrap();
    assert_eq!(resp.stop_reason, StopReason::ToolUse);
    let tc = resp
        .parts
        .iter()
        .find_map(|p| {
            if let ContentPart::ToolCall(tc) = p {
                Some(tc)
            } else {
                None
            }
        })
        .expect("expected ToolCall part");
    assert_eq!(tc.name, "search");
    assert_eq!(tc.input["q"], "rust async");
    let evts = events.lock().unwrap();
    assert!(
        evts.iter()
            .any(|e| matches!(e, AgentEvent::ToolCallStart(_)))
    );
    assert!(
        evts.iter()
            .any(|e| matches!(e, AgentEvent::ToolCallInputDone(_)))
    );
}

// ── streaming text chunks ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_streaming_text_chunks() {
    let mock = MockLlmServer::builder()
        .response_stream(["Hello", " ", "world"])
        .build()
        .await;
    let p = provider_at(&mock.url());
    let msgs = user_messages("hi");
    let (sink, events) = collect_sink();
    let resp = p
        .complete(no_tools_request(&msgs), "msg-1", &sink)
        .await
        .unwrap();
    let text: String = resp
        .parts
        .iter()
        .filter_map(|p| {
            if let ContentPart::Text(t) = p {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, "Hello world");
    let evts = events.lock().unwrap();
    let chunks: Vec<_> = evts
        .iter()
        .filter_map(|e| {
            if let AgentEvent::TextChunk(c) = e {
                Some(c.text.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(chunks.join(""), "Hello world");
}

// ── retry on 529 ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_retry_on_overload() {
    let mock = MockLlmServer::builder()
        .error(529, "overloaded_error")
        .response("recovered")
        .build()
        .await;
    let p = provider_at(&mock.url());
    let msgs = user_messages("hi");
    let (sink, _events) = collect_sink();
    let resp = p
        .complete(no_tools_request(&msgs), "msg-1", &sink)
        .await
        .unwrap();
    let text: String = resp
        .parts
        .iter()
        .filter_map(|p| {
            if let ContentPart::Text(t) = p {
                Some(t.text.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(text, "recovered");
}

// ── extended thinking ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_thinking_block_and_signature() {
    let mock = MockLlmServer::builder()
        .thinking("I should analyse carefully.", "sig-abc-123")
        .build()
        .await;
    let p = provider_at(&mock.url());
    let msgs = user_messages("think");
    let (sink, events) = collect_sink();
    let resp = p
        .complete(no_tools_request(&msgs), "msg-1", &sink)
        .await
        .unwrap();
    let th = resp
        .parts
        .iter()
        .find_map(|p| {
            if let ContentPart::Thinking(t) = p {
                Some(t)
            } else {
                None
            }
        })
        .expect("expected Thinking part");
    assert_eq!(th.text, "I should analyse carefully.");
    assert_eq!(th.signature.as_deref(), Some("sig-abc-123"));
    let evts = events.lock().unwrap();
    assert!(
        evts.iter()
            .any(|e| matches!(e, AgentEvent::ThinkingChunk(_)))
    );
}

// ── with tools in request ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_with_tools_in_request() {
    let mock = MockLlmServer::builder().response("done").build().await;
    let p = provider_at(&mock.url());
    let msgs = user_messages("run tool");
    let (sink, _) = collect_sink();
    let tools = vec![ToolSpec {
        name: "my_tool".into(),
        description: "does stuff".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
    }];
    let req = CompletionRequest {
        messages: &msgs,
        system: Some("be helpful".into()),
        tools,
        tool_choice: ToolChoice::Auto,
        max_tokens: Some(1024),
    };
    let resp = p.complete(req, "msg-1", &sink).await.unwrap();
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
}
