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

use async_llm::mock::MockLlmServer;
use horsie_agentcore::{
    Agent, AgentConfig, AgentError, AgentInput, AgentResult, CompletedOutput, EmptyToolbox,
    LlmError, LlmProvider,
    testkit::{CollectingEventSink, MockToolbox},
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
    /// The Responses wire, exercised with an API key. The ChatGPT-plan
    /// credential reaches the identical code path with a different token and
    /// host, so the suite covers it too — everything but the handshake.
    OpenaiResponses,
}

/// Every kind the suite runs against. Adding a variant here is the only change
/// needed to subject a new backend to the full suite.
const KINDS: &[ProviderKind] = &[
    ProviderKind::Anthropic,
    ProviderKind::Openai,
    ProviderKind::OpenaiResponses,
];

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
                .with_retry_delay_secs(0)
                // Short enough that the stalled-peer test reaches its deadline in
                // seconds rather than minutes.
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

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
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

        let mut agent = Agent::builder(provider, MockToolbox::echo("echo"), "test-conversation")
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

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
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
async fn conformance_max_tokens_truncation_surfaces() {
    for &kind in KINDS {
        let server = spawn_mock().await;
        server.queue_truncated("cut off here");
        let provider = build_provider(kind, &base_url_for(kind, &server));

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
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
            .expect_err("truncation must surface as an error");

        assert!(
            matches!(err, AgentError::Truncated { .. }),
            "{kind:?}: expected Truncated, got {err:?}",
        );
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

        let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
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

// ── fault cases (#61) ────────────────────────────────────────────────────────

/// #61 item 1a: a stream that ends without its terminal event is currently
/// returned as `Ok(CompletionResponse { stop_reason: EndTurn })` — an empty or
/// truncated assistant answer, journaled and shown to the user as success.
/// OpenAI: `Err(StreamEnded) => break` (`providers/openai/src/lib.rs:392`).
/// Anthropic: the `while let` exits with `last_error: None` (`:510`).
#[tokio::test]
async fn a_cut_stream_is_an_error_not_an_empty_success() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for &kind in KINDS {
            let server = spawn_mock().await;
            // message_start + content_block_start + one delta, then nothing.
            server.queue_cut_stream(["par", "tial"], 3);
            let provider = build_provider(kind, &base_url_for(kind, &server));

            let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
                .build()
                .unwrap();
            let sink = CollectingEventSink::new();
            let result = agent
                .run(
                    AgentInput::user_message("msg-1", "hi"),
                    &sink,
                    CancellationToken::new(),
                )
                .await;

            assert!(
                result.is_err(),
                "{kind:?}: a truncated stream must fail the turn, got {:?}",
                result.map(|o| std::mem::discriminant(&o.result))
            );
        }
    })
    .await
    .expect("test timed out");
}

/// #61 item 1b: a half-streamed tool call is dispatched anyway with fabricated
/// input — OpenAI substitutes `json!({})` or `Value::Null`
/// (`providers/openai/src/lib.rs:442-453`), Anthropic an empty object
/// (`providers/anthropic/src/lib.rs:537-548`). The tool then fails with a
/// confusing `InvalidInput` instead of the run failing with a provider error.
#[tokio::test]
async fn a_tool_call_with_unparseable_input_is_never_dispatched() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for &kind in KINDS {
            let server = spawn_mock().await;
            server.queue_cut_tool_call("echo", "call_1", "{\"value\": 4");
            let provider = build_provider(kind, &base_url_for(kind, &server));

            let calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let seen = calls.clone();
            let toolbox = MockToolbox::new(
                vec![horsie_agentcore::ToolSpec {
                    name: "echo".into(),
                    description: "echo".into(),
                    input_schema: serde_json::json!({ "type": "object" }),
                }],
                Arc::new(move |name: &str, input: serde_json::Value| {
                    seen.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(name.to_string());
                    Ok(horsie_agentcore::ToolOutcome::Result(input))
                }),
            );

            let mut agent = Agent::builder(provider, toolbox, "test-conversation")
                .build()
                .unwrap();
            let sink = CollectingEventSink::new();
            let _ = agent
                .run(
                    AgentInput::user_message("msg-1", "hi"),
                    &sink,
                    CancellationToken::new(),
                )
                .await;

            let dispatched = calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            assert!(
                dispatched.is_empty(),
                "{kind:?}: a tool call whose input JSON does not parse must not be \
                 dispatched, but the toolbox saw {dispatched:?}"
            );
        }
    })
    .await
    .expect("test timed out");
}

/// #61 item 5a: neither provider sets `.timeout()`, `.connect_timeout()` or
/// `.read_timeout()` (`providers/anthropic/src/lib.rs:93-101`,
/// `providers/openai/src/lib.rs:74-76`), and reqwest's default is unlimited.
/// Every other HTTP client in the repo does set one, so this is an oversight
/// rather than a decision.
#[tokio::test]
async fn a_slow_provider_gives_up_rather_than_waiting_forever() {
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        for &kind in KINDS {
            let server = spawn_mock().await;
            server.queue_delayed("eventually", std::time::Duration::from_secs(30));
            let provider = build_provider(kind, &base_url_for(kind, &server));

            let mut agent = Agent::builder(provider, Arc::new(EmptyToolbox), "test-conversation")
                .build()
                .unwrap();
            let sink = CollectingEventSink::new();
            // The provider is expected to bound its own wait. If it does not, this
            // inner timeout fires and the assertion below names the reason.
            let settled = tokio::time::timeout(
                std::time::Duration::from_secs(4),
                agent.run(
                    AgentInput::user_message("msg-1", "hi"),
                    &sink,
                    CancellationToken::new(),
                ),
            )
            .await;

            assert!(
                settled.is_ok(),
                "{kind:?}: the provider must give up on a stalled peer, but it was \
                 still waiting after 4s with no deadline of its own"
            );
        }
    })
    .await
    .expect("test timed out");
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
