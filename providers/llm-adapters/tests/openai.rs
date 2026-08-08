#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)]

use async_trait::async_trait;
use horsie_agentcore::{
    CompletionRequest, EventSink, EventSinkError, LlmProvider, StopReason, TextPart,
};
use horsie_llm_adapters::OpenAiProvider;
use horsie_models::{
    agent::{ContentPart, Message, Role},
    events::AgentEvent,
};
use std::sync::{Mutex, PoisonError};

struct RecordingSink(Mutex<Vec<AgentEvent>>);

impl RecordingSink {
    fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    fn events(&self) -> Vec<AgentEvent> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl EventSink for RecordingSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(event);
        Ok(())
    }
}

fn request(messages: &[Message]) -> CompletionRequest<'_> {
    CompletionRequest {
        messages,
        system: None,
        tools: vec![],
        tool_choice: horsie_agentcore::ToolChoice::Auto,
        max_tokens: Some(64),
        thinking_effort: None,
        conversation_id: "test-conversation",
    }
}

#[tokio::test]
async fn streams_openai_text_into_horsie_parts_and_events() {
    let server = async_llm::mock::MockLlmServer::builder()
        .response("hello from OpenAI")
        .build()
        .await;
    let provider = OpenAiProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url());
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "hello".into(),
        })],
    }];
    let sink = RecordingSink::new();

    let response = provider
        .complete(request(&messages), "assistant-1", &sink)
        .await
        .expect("a completed OpenAI stream");

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
    assert!(matches!(
        response.parts.as_slice(),
        [ContentPart::Text(TextPart { text })] if text == "hello from OpenAI"
    ));
    assert!(sink.events().iter().any(
        |event| matches!(event, AgentEvent::TextChunk(chunk) if chunk.text == "hello from OpenAI")
    ));
}

#[tokio::test]
async fn streams_reasoning_before_text_with_thinking_events() {
    let server = async_llm::mock::MockLlmServer::builder().build().await;
    server.queue_reasoning("checking", "answer");
    let provider = OpenAiProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url());
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "why?".into(),
        })],
    }];
    let sink = RecordingSink::new();

    let response = provider
        .complete(request(&messages), "assistant-1", &sink)
        .await
        .expect("a completed OpenAI stream");

    assert!(matches!(
        response.parts.as_slice(),
        [ContentPart::Thinking(thinking), ContentPart::Text(text)]
            if thinking.text == "checking" && thinking.signature.is_none() && text.text == "answer"
    ));
    assert!(sink.events().iter().any(
        |event| matches!(event, AgentEvent::ThinkingChunk(chunk) if chunk.text == "checking")
    ));
}

#[tokio::test]
async fn streams_tool_calls_and_reports_tool_use() {
    let server = async_llm::mock::MockLlmServer::builder().build().await;
    server.queue_tool_call("echo", serde_json::json!({"value": 42}));
    let provider = OpenAiProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url());
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "echo 42".into(),
        })],
    }];
    let request = CompletionRequest {
        messages: &messages,
        system: None,
        tools: vec![horsie_agentcore::ToolSpec {
            name: "echo".into(),
            description: "Echo an input.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        tool_choice: horsie_agentcore::ToolChoice::Auto,
        max_tokens: Some(64),
        thinking_effort: None,
        conversation_id: "test-conversation",
    };
    let sink = RecordingSink::new();

    let response = provider
        .complete(request, "assistant-1", &sink)
        .await
        .expect("a completed OpenAI stream");

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert!(matches!(
        response.parts.as_slice(),
        [ContentPart::ToolCall(call)]
            if call.name == "echo" && call.input == serde_json::json!({"value": 42})
    ));
    assert!(sink
        .events()
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCallInputDelta(delta) if delta.delta == r#"{"value":42}"#)));
}

#[tokio::test]
async fn maps_openai_http_errors_to_horsie_errors() {
    let server = async_llm::mock::MockLlmServer::builder()
        .error(400, "invalid request")
        .build()
        .await;
    let provider = OpenAiProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url())
        .with_retry_delay_secs(0);
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "hello".into(),
        })],
    }];

    let error = provider
        .complete(request(&messages), "assistant-1", &RecordingSink::new())
        .await
        .expect_err("invalid OpenAI requests do not complete");

    assert!(matches!(
        error,
        horsie_agentcore::LlmError::ApiError { status: 400, message } if message.contains("invalid request")
    ));
}
#[tokio::test]
async fn streams_responses_text_into_horsie_parts_and_events() {
    let server = async_llm::mock::MockLlmServer::builder()
        .response("hello from Responses")
        .build()
        .await;
    let provider = horsie_llm_adapters::ResponsesProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url());
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "hello".into(),
        })],
    }];
    let sink = RecordingSink::new();

    let response = provider
        .complete(request(&messages), "assistant-1", &sink)
        .await
        .expect("a completed Responses stream");

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);
    assert!(matches!(
        response.parts.as_slice(),
        [ContentPart::Text(TextPart { text })] if text == "hello from Responses"
    ));
    assert!(sink.events().iter().any(
        |event| matches!(event, AgentEvent::TextChunk(chunk) if chunk.text == "hello from Responses")
    ));
}

#[tokio::test]
async fn preserves_responses_reasoning_encrypted_signature() {
    let server = async_llm::mock::MockLlmServer::builder().build().await;
    server.queue_reasoning("checking", "answer");
    let provider = horsie_llm_adapters::ResponsesProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url());
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "why?".into(),
        })],
    }];

    let response = provider
        .complete(request(&messages), "assistant-1", &RecordingSink::new())
        .await
        .expect("a completed Responses stream");

    let ContentPart::Thinking(thinking) = &response.parts[0] else {
        panic!("expected a thinking part");
    };
    assert_eq!(thinking.text, "checking");
    let signature: serde_json::Value = serde_json::from_str(
        thinking
            .signature
            .as_deref()
            .expect("encrypted reasoning has a replay signature"),
    )
    .expect("the signature is a Responses reasoning reference");
    assert_eq!(signature["id"], "rs_mock");
    assert_eq!(signature["enc"], "gAAAAA-mock-encrypted-reasoning");
}

#[tokio::test]
async fn streams_responses_tool_calls_and_reports_tool_use() {
    let server = async_llm::mock::MockLlmServer::builder().build().await;
    server.queue_tool_call("echo", serde_json::json!({"value": 42}));
    let provider = horsie_llm_adapters::ResponsesProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url());
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "echo 42".into(),
        })],
    }];
    let request = CompletionRequest {
        messages: &messages,
        system: None,
        tools: vec![horsie_agentcore::ToolSpec {
            name: "echo".into(),
            description: "Echo an input.".into(),
            input_schema: serde_json::json!({"type": "object"}),
        }],
        tool_choice: horsie_agentcore::ToolChoice::Auto,
        max_tokens: Some(64),
        thinking_effort: None,
        conversation_id: "test-conversation",
    };
    let sink = RecordingSink::new();

    let response = provider
        .complete(request, "assistant-1", &sink)
        .await
        .expect("a completed Responses stream");

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert!(matches!(
        response.parts.as_slice(),
        [ContentPart::ToolCall(call)]
            if call.name == "echo" && call.input == serde_json::json!({"value": 42})
    ));
    assert!(sink
        .events()
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolCallInputDelta(delta) if delta.delta == r#"{"value":42}"#)));
}

#[tokio::test]
async fn maps_responses_http_errors_to_horsie_errors() {
    let server = async_llm::mock::MockLlmServer::builder()
        .error(400, "invalid request")
        .build()
        .await;
    let provider = horsie_llm_adapters::ResponsesProvider::with_api_key("test-key")
        .expect("constructs")
        .with_model("mock-model")
        .with_base_url(server.url())
        .with_retry_delay_secs(0);
    let messages = vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "user-1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "hello".into(),
        })],
    }];

    let error = provider
        .complete(request(&messages), "assistant-1", &RecordingSink::new())
        .await
        .expect_err("invalid Responses requests do not complete");

    assert!(matches!(
        error,
        horsie_agentcore::LlmError::ApiError { status: 400, message } if message.contains("invalid request")
    ));
}
