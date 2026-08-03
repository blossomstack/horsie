//! Live DeepSeek checks. Ignored by default — they cost money and need a key.
//!
//! Run with:
//!   DEEPSEEK_API_KEY=sk-... cargo test -p horsie-openai --test deepseek_live -- --ignored

#![allow(clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use horsie_agentcore::{
    CompletionRequest, EventSink, EventSinkError, LlmProvider, ThinkingDialect, ThinkingEffort,
    ToolChoice, ToolSpec,
};
use horsie_models::agent::{ContentPart, Message, Role, TextPart};
use horsie_models::events::AgentEvent;
use horsie_openai::OpenAiProvider;

/// Defined here rather than reaching for `agentcore::testkit`, which is behind
/// the `test-util` feature this crate's dev-dependencies do not enable.
struct NullSink;

#[async_trait]
impl EventSink for NullSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        Ok(())
    }
}

fn key() -> String {
    std::env::var("DEEPSEEK_API_KEY").expect("DEEPSEEK_API_KEY must be set for live tests")
}

fn provider(forced_tools_disable_thinking: bool) -> OpenAiProvider {
    OpenAiProvider::with_api_key(key().as_str())
        .unwrap()
        .with_model("deepseek-v4-flash")
        .with_base_url("https://api.deepseek.com")
        .with_thinking_dialect(ThinkingDialect::OpenAiEffort)
        .with_forced_tools_disable_thinking(forced_tools_disable_thinking)
}

fn weather_tool() -> ToolSpec {
    ToolSpec {
        name: "get_weather".into(),
        description: "Get the current weather for a city.".into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        }),
    }
}

fn ask_for_weather() -> Vec<Message> {
    vec![Message {
        created_at_ms: 0,
        started_at_ms: None,
        id: "m1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: "What is the weather in Paris?".into(),
        })],
    }]
}

/// The regression this whole feature exists for: DeepSeek 400s on a pinned
/// tool_choice while thinking is on, so the flag must make it succeed.
#[tokio::test]
#[ignore = "hits the live DeepSeek API"]
async fn pinned_tool_choice_succeeds_with_the_flag() {
    let messages = ask_for_weather();
    let request = CompletionRequest {
        messages: &messages,
        system: None,
        tools: vec![weather_tool()],
        tool_choice: ToolChoice::Any,
        max_tokens: Some(512),
        thinking_effort: ThinkingEffort::parse("high"),
    };

    let resp = provider(true)
        .complete(request, "msg-1", &NullSink)
        .await
        .expect("a pinned tool choice must not 400");

    assert!(
        resp.parts
            .iter()
            .any(|p| matches!(p, ContentPart::ToolCall(_))),
        "expected a tool call",
    );
}

/// The other half of the claim: without the flag the same request really does
/// fail, so the flag is load-bearing rather than decorative.
#[tokio::test]
#[ignore = "hits the live DeepSeek API"]
async fn pinned_tool_choice_is_rejected_without_the_flag() {
    let messages = ask_for_weather();
    let request = CompletionRequest {
        messages: &messages,
        system: None,
        tools: vec![weather_tool()],
        tool_choice: ToolChoice::Any,
        max_tokens: Some(512),
        thinking_effort: ThinkingEffort::parse("high"),
    };

    let err = provider(false)
        .complete(request, "msg-1", &NullSink)
        .await
        .expect_err("DeepSeek rejects a pinned tool choice while thinking is on");

    assert!(
        format!("{err:?}").contains("tool_choice"),
        "expected the tool_choice rejection, got {err:?}",
    );
}
