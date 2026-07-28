//! Cache-usage accounting across the two places a provider may report it.
//!
//! Anthropic reports the cache split in `message_start`. Anthropic-compatible
//! endpoints need not: `https://api.kimi.com/coding/` (model `k3`) reports an
//! uncached `input_tokens` with both cache counters at 0 in `message_start`, and
//! only carries the real split in `message_delta`. These tests pin both shapes.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use async_trait::async_trait;
use horsie_agentcore::{
    AgentEvent, CompletionRequest, ContentPart, EventSink, EventSinkError, LlmProvider, ToolChoice,
};
use horsie_anthropic::AnthropicProvider;
use horsie_models::agent::{Message, Role, TextPart};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct NullSink;

#[async_trait]
impl EventSink for NullSink {
    async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
        Ok(())
    }
}

/// Serve exactly one SSE response made of `events`, then shut down.
async fn serve_once(events: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 8192];
        let _ = sock.read(&mut buf).await;
        let mut body = String::new();
        for e in &events {
            let kind: serde_json::Value = serde_json::from_str(e).unwrap();
            let name = kind["type"].as_str().unwrap();
            body.push_str(&format!("event: {name}\ndata: {e}\n\n"));
        }
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(body.as_bytes()).await.unwrap();
        sock.flush().await.unwrap();
    });
    format!("http://{addr}")
}

fn events(start_usage: serde_json::Value, delta_usage: serde_json::Value) -> Vec<String> {
    vec![
        serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_1", "type": "message", "role": "assistant",
                "model": "k3", "content": [], "stop_reason": null,
                "usage": start_usage
            }
        })
        .to_string(),
        serde_json::json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""}
        })
        .to_string(),
        serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "hi"}
        })
        .to_string(),
        serde_json::json!({"type": "content_block_stop", "index": 0}).to_string(),
        serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": null},
            "usage": delta_usage
        })
        .to_string(),
        serde_json::json!({"type": "message_stop"}).to_string(),
    ]
}

async fn usage_for(
    start_usage: serde_json::Value,
    delta_usage: serde_json::Value,
) -> horsie_models::agent::Usage {
    let url = serve_once(events(start_usage, delta_usage)).await;
    let provider = AnthropicProvider::with_api_key("test-key")
        .unwrap()
        .with_base_url(&url)
        .with_retry_delay_secs(0);
    let messages = vec![Message {
        id: "m1".into(),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart { text: "hi".into() })],
    }];
    let request = CompletionRequest {
        messages: &messages,
        system: None,
        tools: vec![],
        tool_choice: ToolChoice::Auto,
        max_tokens: None,
        thinking_effort: None,
    };
    provider
        .complete(request, "msg-1", &NullSink)
        .await
        .unwrap()
        .usage
}

/// The kimi shape: `message_start` claims a full uncached prefix, `message_delta`
/// reveals that almost all of it was a cache read. The delta wins.
#[tokio::test]
async fn cache_split_from_message_delta_supersedes_message_start() {
    let usage = usage_for(
        serde_json::json!({
            "input_tokens": 7507,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "output_tokens": 0
        }),
        serde_json::json!({
            "input_tokens": 83,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 7424,
            "output_tokens": 32
        }),
    )
    .await;

    assert_eq!(usage.cache_read_tokens, Some(7424));
    assert_eq!(usage.cache_creation_tokens, Some(0));
    assert_eq!(usage.output_tokens, 32);
    // `input_tokens` is the all-in prefix total: 83 fresh + 7424 cached.
    assert_eq!(usage.input_tokens, 7507);
}

/// The Anthropic shape: the split arrives in `message_start` and `message_delta`
/// carries only `output_tokens`. The start values must survive.
#[tokio::test]
async fn cache_split_from_message_start_survives_output_only_delta() {
    let usage = usage_for(
        serde_json::json!({
            "input_tokens": 100,
            "cache_creation_input_tokens": 2000,
            "cache_read_input_tokens": 5000,
            "output_tokens": 0
        }),
        serde_json::json!({"output_tokens": 42}),
    )
    .await;

    assert_eq!(usage.cache_read_tokens, Some(5000));
    assert_eq!(usage.cache_creation_tokens, Some(2000));
    assert_eq!(usage.output_tokens, 42);
    assert_eq!(usage.input_tokens, 7100);
}
