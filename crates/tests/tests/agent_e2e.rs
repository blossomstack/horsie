//! End-to-end tests for one `agentcore::run_step` over the Anthropic wire.
//!
//! Tool dispatch and run-loop conclusions belong to `AgentActor`; this file
//! pins only the provider-step boundary and its streamed events.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use async_llm::mock::MockLlmServer;
use async_trait::async_trait;
use horsie_agentcore::{
    AgentEvent, ContentPart, EventSink, EventSinkError, StepError, StepRequest, ToolChoice,
    extract_text, extract_tool_calls, run_step,
};
use horsie_llm_providers::anthropic::AnthropicProvider;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

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

fn request(url: &str) -> StepRequest {
    StepRequest {
        provider: Arc::new(
            AnthropicProvider::with_api_key("test-key")
                .unwrap()
                .with_base_url(url)
                .with_retry_delay_secs(0),
        ),
        conversation_id: "test-conversation".into(),
        system_prompt: "fixed system prompt".into(),
        specs: Vec::new(),
        tool_choice: ToolChoice::Auto,
        max_tokens: None,
        thinking_effort: None,
        artifact_source: None,
    }
}

fn history(text: &str) -> Vec<horsie_agentcore::Message> {
    vec![horsie_agentcore::Message::user("u1", text, 0)]
}

#[tokio::test]
async fn a_text_step_streams_and_returns_one_complete_assistant_message() {
    let mock = MockLlmServer::builder()
        .response_stream(["Hello", " ", "world"])
        .build()
        .await;
    let sink = CollectSink::new();
    let response = run_step(
        &request(&mock.url()),
        &history("hi"),
        &sink,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(extract_text(&response.message.parts), "Hello world");
    assert_eq!(response.message.role, horsie_agentcore::Role::Assistant);
    assert!(response.usage.input_tokens > 0);
    assert!(response.usage.output_tokens > 0);

    let events = sink.events();
    let start = events
        .iter()
        .position(|event| matches!(event, AgentEvent::MessageStart(_)))
        .unwrap();
    let chunk = events
        .iter()
        .position(|event| matches!(event, AgentEvent::TextChunk(_)))
        .unwrap();
    let stop = events
        .iter()
        .rposition(|event| matches!(event, AgentEvent::MessageStop(_)))
        .unwrap();
    assert!(start < chunk && chunk < stop);
}

#[tokio::test]
async fn a_tool_step_returns_the_call_without_executing_it() {
    let mock = MockLlmServer::builder()
        .tool_call("search", serde_json::json!({"q": "rust"}))
        .build()
        .await;
    let response = run_step(
        &request(&mock.url()),
        &history("search"),
        &CollectSink::new(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    let calls = extract_tool_calls(&response.message.parts);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].1, "search");
    assert_eq!(calls[0].2["q"], "rust");
    assert!(
        response
            .message
            .parts
            .iter()
            .any(|part| matches!(part, ContentPart::ToolCall(_)))
    );
}

#[tokio::test]
async fn cancellation_ends_the_provider_step() {
    let mock = MockLlmServer::builder().build().await;
    mock.queue_delayed("late", std::time::Duration::from_secs(30));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        trigger.cancel();
    });

    let result = run_step(
        &request(&mock.url()),
        &history("wait"),
        &CollectSink::new(),
        &cancel,
    )
    .await;
    assert!(matches!(result, Err(StepError::Cancelled)));
}
