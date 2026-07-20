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
    Agent, AgentConfig, AgentError, AgentInput, AgentResult, CompletedOutput, EmptyToolbox,
    LlmError, LlmProvider,
    testkit::{CollectingEventSink, MockToolbox},
};
use horsie_anthropic::AnthropicProvider;
use horsie_mock_llm::MockLlmServer;
use horsie_openai::OpenAiProvider;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderKind {
    Anthropic,
    Openai,
}

/// Every kind the suite runs against. Adding a variant here is the only change
/// needed to subject a new backend to the full suite.
const KINDS: &[ProviderKind] = &[ProviderKind::Anthropic, ProviderKind::Openai];

fn build_provider(kind: ProviderKind, base_url: &str) -> Arc<dyn LlmProvider> {
    // `with_retry_delay_secs(0)` on both: without it a queued 429 costs the
    // suite minutes of real backoff before it fails.
    match kind {
        ProviderKind::Anthropic => Arc::new(
            AnthropicProvider::with_api_key("test-key")
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024))
                .with_retry_delay_secs(0),
        ),
        ProviderKind::Openai => Arc::new(
            OpenAiProvider::with_api_key("test-key")
                .unwrap()
                .with_model("mock-model")
                .with_base_url(base_url)
                .with_max_tokens(Some(1024))
                .with_retry_delay_secs(0),
        ),
    }
}

/// The mock's base URL for a given wire. Both wires are served by the same
/// process on the same port; each provider appends its own path.
fn base_url_for(_kind: ProviderKind, server: &MockLlmServer) -> String {
    server.url()
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

#[tokio::test]
async fn conformance_rate_limit_is_classified() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        // A 429 must be classified as retryable *and*, once the retry budget is
        // exhausted, surface as RateLimit/Overloaded rather than falling through
        // to Network. Queue more errors than any provider's retry count so every
        // attempt gets one — a single queued error would be retried, hit an empty
        // queue, and come back as a normal completion, hiding the bug this guards.
        for _ in 0..12 {
            server.queue_error(429, "slow down");
        }
        let provider = build_provider(kind, &base_url_for(kind, &server));

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox))
            .with_config(AgentConfig {
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
