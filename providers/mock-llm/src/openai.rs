//! The OpenAI-compatible wire (`/v1/chat/completions`), served from the same
//! `MockResponse` queue as the Anthropic route. One mock process speaks both
//! protocols, so a conformance test can point either provider at one server.
//!
//! Two deliberate differences from the Anthropic route:
//!
//! - Errors are real HTTP statuses, not SSE error frames. That is how
//!   OpenAI-compatible backends actually signal failure, and it is what the
//!   provider's status-based classification must handle.
//! - `MockResponse::Thinking` renders as an empty turn — OpenAI-shaped backends
//!   have no thinking blocks.

use crate::server::{MockResponse, MockState, ResponseKind, sse_from_pairs};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub(crate) struct ChatRequest {
    #[serde(default)]
    #[allow(dead_code)]
    stream: Option<bool>,
}

fn chunk(id: &str, delta: serde_json::Value, finish: Option<&str>) -> (String, String) {
    let mut choice = serde_json::json!({ "index": 0, "delta": delta });
    if let Some(f) = finish {
        choice["finish_reason"] = serde_json::json!(f);
    } else {
        choice["finish_reason"] = serde_json::Value::Null;
    }
    (
        "message".into(),
        serde_json::json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": 0,
            "model": "mock-model",
            "choices": [choice],
        })
        .to_string(),
    )
}

/// The terminal frame: an empty delta carrying `finish_reason` and usage.
fn final_chunk(id: &str, finish: &str, completion_tokens: u32) -> Vec<(String, String)> {
    vec![
        (
            "message".into(),
            serde_json::json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": 0,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": completion_tokens,
                    "total_tokens": 10 + completion_tokens
                }
            })
            .to_string(),
        ),
        ("message".into(), "[DONE]".into()),
    ]
}

fn text_chunks(id: &str, text: &str) -> Vec<(String, String)> {
    let mut out = vec![chunk(
        id,
        serde_json::json!({ "role": "assistant", "content": text }),
        None,
    )];
    out.extend(final_chunk(id, "stop", 5));
    out
}

/// A response cut off by the output-token ceiling: `finish_reason: length`.
fn truncated_chunks(id: &str, text: &str) -> Vec<(String, String)> {
    let mut out = vec![chunk(
        id,
        serde_json::json!({ "role": "assistant", "content": text }),
        None,
    )];
    out.extend(final_chunk(id, "length", 5));
    out
}

fn text_stream_chunks(id: &str, chunks: &[String]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = chunks
        .iter()
        .map(|c| {
            chunk(
                id,
                serde_json::json!({ "role": "assistant", "content": c }),
                None,
            )
        })
        .collect();
    out.extend(final_chunk(id, "stop", 5));
    out
}

/// A reasoning-model turn: the reasoning trace streams first as
/// `delta.reasoning_content` (DeepSeek/vLLM shape), then the answer content,
/// then a `stop` finish. Mirrors how deepseek-reasoner interleaves the two.
fn reasoning_chunks(id: &str, reasoning: &str, content: &str) -> Vec<(String, String)> {
    let mut out = vec![
        chunk(
            id,
            serde_json::json!({ "role": "assistant", "reasoning_content": reasoning }),
            None,
        ),
        chunk(id, serde_json::json!({ "content": content }), None),
    ];
    out.extend(final_chunk(id, "stop", 5));
    out
}

/// A tool call. Arguments arrive as a single delta here; real backends fragment
/// them, and the provider accumulates either way.
fn tool_call_chunks(
    id: &str,
    call_id: &str,
    name: &str,
    input: &serde_json::Value,
) -> Vec<(String, String)> {
    let args = serde_json::to_string(input).unwrap_or_default();
    let mut out = vec![chunk(
        id,
        serde_json::json!({
            "role": "assistant",
            "tool_calls": [{
                "index": 0,
                "id": call_id,
                "type": "function",
                "function": { "name": name, "arguments": args }
            }]
        }),
        None,
    )];
    out.extend(final_chunk(id, "tool_calls", 10));
    out
}

pub(crate) async fn handle_chat_completions(
    State(state): State<Arc<MockState>>,
    _headers: HeaderMap,
    Json(_req): Json<ChatRequest>,
) -> ResponseKind {
    let entry = state.dequeue_entry();

    if let Some(e) = &entry {
        if let Some(r) = &e.reached {
            r.notify_one();
        }
        if let Some(g) = &e.gate {
            g.notified().await;
        }
    }

    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4());
    let call_id = format!("call_{}", uuid::Uuid::new_v4());

    match entry.map(|e| e.response) {
        Some(MockResponse::Text { content }) => sse_from_pairs(text_chunks(&id, &content)),
        Some(MockResponse::Truncated { content }) => {
            sse_from_pairs(truncated_chunks(&id, &content))
        }
        Some(MockResponse::Reasoning { reasoning, content }) => {
            sse_from_pairs(reasoning_chunks(&id, &reasoning, &content))
        }
        Some(MockResponse::TextStream { chunks }) => {
            sse_from_pairs(text_stream_chunks(&id, &chunks))
        }
        Some(MockResponse::ToolCall { name, input }) => {
            sse_from_pairs(tool_call_chunks(&id, &call_id, &name, &input))
        }
        Some(MockResponse::ToolCallStream {
            name,
            id: tid,
            input,
        }) => sse_from_pairs(tool_call_chunks(&id, &tid, &name, &input)),
        // No OpenAI equivalent — render as an empty assistant turn.
        Some(MockResponse::Thinking { .. }) => sse_from_pairs(text_chunks(&id, "")),
        Some(MockResponse::Error { status, message }) => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            ResponseKind::HttpError(
                code,
                axum::Json(serde_json::json!({
                    "error": {
                        "message": message,
                        "type": match status {
                            429 => "rate_limit_exceeded",
                            500..=599 => "server_error",
                            _ => "invalid_request_error",
                        },
                        "code": status,
                    }
                })),
            )
        }
        None => sse_from_pairs(text_chunks(&id, "No mock response queued")),
    }
}

#[cfg(test)]
mod tests {
    use crate::MockLlmServer;

    async fn post_stream(server: &MockLlmServer) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/v1/chat/completions", server.url()))
            .json(&serde_json::json!({
                "model": "mock-model",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": true
            }))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn chat_completions_streams_queued_text() {
        let server = MockLlmServer::builder().build().await;
        server.queue_response("hi there");

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(body.contains("chat.completion.chunk"), "body was: {body}");
        assert!(body.contains("hi there"), "body was: {body}");
        assert!(body.contains("[DONE]"), "body was: {body}");
    }

    #[tokio::test]
    async fn chat_completions_streams_queued_tool_call() {
        let server = MockLlmServer::builder().build().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(body.contains("tool_calls"), "body was: {body}");
        assert!(body.contains("echo"), "body was: {body}");
    }

    #[tokio::test]
    async fn chat_completions_error_uses_http_status() {
        let server = MockLlmServer::builder().build().await;
        server.queue_error(429, "slow down");

        assert_eq!(post_stream(&server).await.status().as_u16(), 429);
    }

    #[tokio::test]
    async fn chat_completions_truncation_uses_length_finish_reason() {
        let server = MockLlmServer::builder().build().await;
        server.queue_truncated("cut off");

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(
            body.contains("\"finish_reason\":\"length\""),
            "body: {body}"
        );
    }
}
