//! Provider conformance through `agentcore::run_step`.
//!
//! The actor owns the loop; this suite therefore checks only the portable
//! contract of one provider call. Multi-step cases explicitly append the first
//! response and tool result before making the next call.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use async_llm::mock::MockLlmServer;
use horsie_agentcore::{
    LlmError, LlmProvider, Message, StepError, StepRequest, StopReason, ToolChoice, ToolSpec,
    extract_text, extract_tool_calls, run_step, testkit::CollectingEventSink,
};
use horsie_llm_providers::anthropic::AnthropicProvider;
use horsie_llm_providers::openai::OpenAiProvider;
use horsie_llm_providers::responses::ResponsesProvider;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Anthropic,
    Openai,
    OpenaiResponses,
}

const KINDS: &[ProviderKind] = &[
    ProviderKind::Anthropic,
    ProviderKind::Openai,
    ProviderKind::OpenaiResponses,
];

fn build_provider(kind: ProviderKind, base_url: &str) -> Arc<dyn LlmProvider> {
    match kind {
        ProviderKind::Anthropic => Arc::new(
            AnthropicProvider::with_api_key("test-key")
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024))
                .with_retry_delay_secs(0)
                .with_read_timeout_secs(2),
        ),
        ProviderKind::Openai => Arc::new(
            OpenAiProvider::with_api_key("test-key")
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024))
                .with_retry_delay_secs(0)
                .with_read_timeout_secs(2),
        ),
        ProviderKind::OpenaiResponses => Arc::new(
            ResponsesProvider::with_api_key("test-key")
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024))
                .with_retry_delay_secs(0)
                .with_read_timeout_secs(2),
        ),
    }
}

fn base_url_for(_kind: ProviderKind, server: &MockLlmServer) -> String {
    server.url()
}

async fn spawn_mock() -> MockLlmServer {
    MockLlmServer::builder().build().await
}

fn step_request(provider: Arc<dyn LlmProvider>, specs: Vec<ToolSpec>) -> StepRequest {
    StepRequest {
        provider,
        conversation_id: "test-conversation".into(),
        system_prompt: String::new(),
        specs,
        tool_choice: ToolChoice::Auto,
        max_tokens: None,
        thinking_effort: None,
        artifact_source: None,
    }
}

fn user(text: &str) -> Message {
    Message::user(uuid::Uuid::new_v4().to_string(), text, 0)
}

async fn call(
    provider: Arc<dyn LlmProvider>,
    specs: Vec<ToolSpec>,
    history: &[Message],
    sink: &CollectingEventSink,
) -> Result<horsie_agentcore::StepResponse, StepError> {
    run_step(
        &step_request(provider, specs),
        history,
        sink,
        &CancellationToken::new(),
    )
    .await
}

#[tokio::test]
async fn conformance_plain_text_step() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_response("Hello from the mock");
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let sink = CollectingEventSink::new();
        let response = call(provider, Vec::new(), &[user("hi")], &sink)
            .await
            .unwrap_or_else(|error| panic!("{kind:?}: step failed: {error}"));
        assert_eq!(extract_text(&response.message.parts), "Hello from the mock");
    }
}

#[tokio::test]
async fn conformance_tool_result_reaches_the_next_step() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));
        server.queue_response("tool said 42");
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let specs = vec![ToolSpec {
            name: "echo".into(),
            description: "echo".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        }];
        let sink = CollectingEventSink::new();
        let mut history = vec![user("use the tool")];
        let first = call(provider.clone(), specs.clone(), &history, &sink)
            .await
            .unwrap();
        let calls = extract_tool_calls(&first.message.parts);
        assert_eq!(calls.len(), 1, "{kind:?}");
        history.push(first.message);
        history.push(Message::tool_result(
            calls[0].0.clone(),
            "42",
            false,
            Vec::new(),
            0,
        ));

        let second = call(provider, specs, &history, &sink).await.unwrap();
        assert_eq!(
            extract_text(&second.message.parts),
            "tool said 42",
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn conformance_history_is_replayed() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_response("first");
        server.queue_response("second");
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let sink = CollectingEventSink::new();
        let mut history = vec![user("one")];
        let first = call(provider.clone(), Vec::new(), &history, &sink)
            .await
            .unwrap();
        history.push(first.message);
        history.push(user("two"));
        let second = call(provider, Vec::new(), &history, &sink).await.unwrap();
        assert_eq!(extract_text(&second.message.parts), "second", "{kind:?}");
    }
}

#[tokio::test]
async fn conformance_max_tokens_is_reported_by_the_step() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_truncated("cut off here");
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let response = call(
            provider,
            Vec::new(),
            &[user("hi")],
            &CollectingEventSink::new(),
        )
        .await
        .unwrap();
        assert_eq!(response.stop_reason, StopReason::MaxTokens, "{kind:?}");
    }
}

#[tokio::test]
async fn conformance_rate_limit_is_classified() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        for _ in 0..12 {
            server.queue_error(429, "slow down");
        }
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let error = call(
            provider,
            Vec::new(),
            &[user("hi")],
            &CollectingEventSink::new(),
        )
        .await
        .expect_err("expected the 429 to surface");
        assert!(
            matches!(
                error,
                StepError::Provider(LlmError::RateLimit { .. } | LlmError::Overloaded)
            ),
            "{kind:?}: {error:?}"
        );
    }
}

#[tokio::test]
async fn a_cut_stream_is_an_error_not_an_empty_success() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_cut_stream(["par", "tial"], 3);
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let result = call(
            provider,
            Vec::new(),
            &[user("hi")],
            &CollectingEventSink::new(),
        )
        .await;
        assert!(result.is_err(), "{kind:?}: cut stream succeeded");
    }
}

#[tokio::test]
async fn a_tool_call_with_unparseable_input_fails_the_step() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_cut_tool_call("echo", "call_1", "{\"value\": 4");
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let result = call(
            provider,
            vec![ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            &[user("hi")],
            &CollectingEventSink::new(),
        )
        .await;
        assert!(result.is_err(), "{kind:?}: malformed tool input succeeded");
    }
}

#[tokio::test]
async fn a_slow_provider_gives_up_rather_than_waiting_forever() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_delayed("eventually", std::time::Duration::from_secs(30));
        let provider = build_provider(kind, &base_url_for(kind, &server));
        let settled = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            call(
                provider,
                Vec::new(),
                &[user("hi")],
                &CollectingEventSink::new(),
            ),
        )
        .await;
        assert!(settled.is_ok(), "{kind:?}: provider had no read deadline");
    }
}

fn user_history() -> Vec<horsie_agentcore::Message> {
    vec![horsie_agentcore::Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "m1".into(),
        role: horsie_agentcore::Role::User,
        parts: vec![horsie_agentcore::ContentPart::Text(
            horsie_agentcore::TextPart { text: "hi".into() },
        )],
    }]
}

fn request_for(messages: &[horsie_agentcore::Message]) -> horsie_agentcore::CompletionRequest<'_> {
    horsie_agentcore::CompletionRequest {
        artifacts: horsie_agentcore::ArtifactBytes::empty(),
        messages,
        system: None,
        tools: vec![],
        tool_choice: horsie_agentcore::ToolChoice::Auto,
        max_tokens: None,
        thinking_effort: None,
        conversation_id: "test-conversation",
    }
}

/// #61 item 6: Anthropic maps `BadRequest` to `LlmError::Network`
/// (`providers/anthropic/src/lib.rs:58-60`), discarding the status, so a permanent
/// 400 — context-length exceeded, a malformed tool `input_schema`, an unanswered
/// `tool_use` — is reported to the user as a transient network error with
/// `recoverable: true`. The OpenAI provider already classifies by status, and its
/// own comment calls the Anthropic approach out as the anti-pattern.
///
/// These two call the provider directly rather than through `Agent`, because the
/// assertion is about `LlmError`'s variant, which `Agent` wraps.
#[tokio::test]
async fn anthropic_reports_a_400_as_an_api_error_with_its_status() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let server = spawn_mock().await;
        server.queue_error(400, "context length exceeded");
        let provider = build_provider(
            ProviderKind::Anthropic,
            &base_url_for(ProviderKind::Anthropic, &server),
        );
        let sink = CollectingEventSink::new();
        let messages = user_history();

        let result = provider
            .complete(request_for(&messages), "msg-1", &sink)
            .await;

        assert!(
            matches!(result, Err(LlmError::ApiError { status: 400, .. })),
            "a 400 must keep its status, got {:?}",
            result.map(|_| ())
        );
    })
    .await
    .expect("test timed out");
}

/// The Responses wire signals with real HTTP statuses too, so the same
/// assertion holds — and must, because a ChatGPT-plan 400 (a model the
/// subscription cannot reach, say) has to arrive as itself rather than as a
/// network fault.
#[tokio::test]
async fn responses_reports_a_400_as_an_api_error_with_its_status() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let server = spawn_mock().await;
        server.queue_error(400, "unsupported model for this plan");
        let provider = build_provider(
            ProviderKind::OpenaiResponses,
            &base_url_for(ProviderKind::OpenaiResponses, &server),
        );
        let sink = CollectingEventSink::new();
        let messages = user_history();

        let result = provider
            .complete(request_for(&messages), "msg-1", &sink)
            .await;

        assert!(
            matches!(result, Err(LlmError::ApiError { status: 400, .. })),
            "a 400 must keep its status, got {:?}",
            result.map(|_| ())
        );
    })
    .await
    .expect("test timed out");
}

/// The green control for the test above: the same assertion already holds on the
/// OpenAI wire, which proves it is satisfiable and that Anthropic's failure is a
/// real difference rather than a broken test.
#[tokio::test]
async fn openai_reports_a_400_as_an_api_error_with_its_status() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let server = spawn_mock().await;
        server.queue_error(400, "context length exceeded");
        let provider = build_provider(
            ProviderKind::Openai,
            &base_url_for(ProviderKind::Openai, &server),
        );
        let sink = CollectingEventSink::new();
        let messages = user_history();

        let result = provider
            .complete(request_for(&messages), "msg-1", &sink)
            .await;

        assert!(
            matches!(result, Err(LlmError::ApiError { status: 400, .. })),
            "a 400 must keep its status, got {:?}",
            result.map(|_| ())
        );
    })
    .await
    .expect("test timed out");
}
