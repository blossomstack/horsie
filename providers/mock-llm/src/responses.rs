//! The OpenAI **Responses** wire (`/responses`), served from the same
//! `MockResponse` queue as the Anthropic and chat-completions routes. One mock
//! process speaks three protocols, so a conformance test can point any provider
//! at one server.
//!
//! Two things this route models that the chat-completions one cannot:
//!
//! - Every output item arrives twice — `output_item.added` opens it, deltas
//!   stream it, `output_item.done` repeats it complete. The provider takes its
//!   final content from `done`, so the mock must send it.
//! - Reasoning items carry `encrypted_content`. That blob is the only thing
//!   that makes a thinking part replayable, so the mock emits one.

use crate::server::{MockResponse, MockState, ResponseKind, sse_from_pairs};
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use std::sync::Arc;

/// An opaque stand-in for the real encrypted chain of thought. Its only
/// contract is that it survives a round trip unchanged.
const MOCK_ENCRYPTED_REASONING: &str = "gAAAAA-mock-encrypted-reasoning";

fn frame(kind: &str, mut value: serde_json::Value) -> (String, String) {
    value["type"] = serde_json::json!(kind);
    (kind.to_string(), value.to_string())
}

fn created(id: &str) -> (String, String) {
    frame(
        "response.created",
        serde_json::json!({ "response": { "id": id, "status": "in_progress" } }),
    )
}

fn usage(output_tokens: u32) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": 10,
        "input_tokens_details": { "cached_tokens": 4 },
        "output_tokens": output_tokens,
        "total_tokens": 10 + output_tokens,
    })
}

fn completed(id: &str, output_tokens: u32) -> (String, String) {
    frame(
        "response.completed",
        serde_json::json!({
            "response": { "id": id, "status": "completed", "usage": usage(output_tokens) }
        }),
    )
}

/// The terminal frame for a response the output-token ceiling cut off.
fn incomplete(id: &str) -> (String, String) {
    frame(
        "response.incomplete",
        serde_json::json!({
            "response": {
                "id": id,
                "status": "incomplete",
                "incomplete_details": { "reason": "max_output_tokens" },
                "usage": usage(5),
            }
        }),
    )
}

fn message_added(index: u32, item_id: &str) -> (String, String) {
    frame(
        "response.output_item.added",
        serde_json::json!({
            "output_index": index,
            "item": { "type": "message", "id": item_id, "role": "assistant", "content": [] },
        }),
    )
}

fn text_delta(index: u32, item_id: &str, delta: &str) -> (String, String) {
    frame(
        "response.output_text.delta",
        serde_json::json!({ "output_index": index, "item_id": item_id, "delta": delta }),
    )
}

fn message_done(index: u32, item_id: &str, text: &str) -> (String, String) {
    frame(
        "response.output_item.done",
        serde_json::json!({
            "output_index": index,
            "item": {
                "type": "message",
                "id": item_id,
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }],
            },
        }),
    )
}

/// One whole assistant text item: added, one delta, done.
fn text_item(index: u32, item_id: &str, text: &str) -> Vec<(String, String)> {
    vec![
        message_added(index, item_id),
        text_delta(index, item_id, text),
        message_done(index, item_id, text),
    ]
}

fn reasoning_item(index: u32, item_id: &str, summary: &str) -> Vec<(String, String)> {
    vec![
        frame(
            "response.output_item.added",
            serde_json::json!({
                "output_index": index,
                "item": { "type": "reasoning", "id": item_id, "summary": [] },
            }),
        ),
        frame(
            "response.reasoning_summary_text.delta",
            serde_json::json!({ "output_index": index, "item_id": item_id, "delta": summary }),
        ),
        frame(
            "response.output_item.done",
            serde_json::json!({
                "output_index": index,
                "item": {
                    "type": "reasoning",
                    "id": item_id,
                    "summary": [{ "type": "summary_text", "text": summary }],
                    "encrypted_content": MOCK_ENCRYPTED_REASONING,
                },
            }),
        ),
    ]
}

fn function_call_item(
    index: u32,
    item_id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> Vec<(String, String)> {
    vec![
        frame(
            "response.output_item.added",
            serde_json::json!({
                "output_index": index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": "",
                },
            }),
        ),
        frame(
            "response.function_call_arguments.delta",
            serde_json::json!({ "output_index": index, "item_id": item_id, "delta": arguments }),
        ),
        frame(
            "response.output_item.done",
            serde_json::json!({
                "output_index": index,
                "item": {
                    "type": "function_call",
                    "id": item_id,
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                },
            }),
        ),
    ]
}

pub(crate) async fn handle_responses(
    State(state): State<Arc<MockState>>,
    _headers: HeaderMap,
    Json(req): Json<serde_json::Value>,
) -> ResponseKind {
    state.capture(req);
    let entry = state.dequeue_entry();

    if let Some(e) = &entry {
        if let Some(r) = &e.reached {
            r.notify_one();
        }
        if let Some(g) = &e.gate {
            g.notified().await;
        }
        if let Some(d) = e.delay {
            tokio::time::sleep(d).await;
        }
    }

    let id = format!("resp_{}", uuid::Uuid::new_v4());
    let item_id = format!("msg_{}", uuid::Uuid::new_v4());
    let call_id = format!("call_{}", uuid::Uuid::new_v4());

    match entry.map(|e| e.response) {
        Some(MockResponse::Text { content }) => {
            let mut out = vec![created(&id)];
            out.extend(text_item(0, &item_id, &content));
            out.push(completed(&id, 5));
            sse_from_pairs(out)
        }

        Some(MockResponse::Truncated { content }) => {
            let mut out = vec![created(&id)];
            out.extend(text_item(0, &item_id, &content));
            out.push(incomplete(&id));
            sse_from_pairs(out)
        }

        Some(MockResponse::TextStream { chunks }) => {
            let mut out = vec![created(&id), message_added(0, &item_id)];
            out.extend(
                chunks
                    .iter()
                    .map(|c| text_delta(0, &item_id, c))
                    .collect::<Vec<_>>(),
            );
            out.push(message_done(0, &item_id, &chunks.concat()));
            out.push(completed(&id, 5));
            sse_from_pairs(out)
        }

        Some(MockResponse::Reasoning { reasoning, content }) => {
            let mut out = vec![created(&id)];
            out.extend(reasoning_item(0, "rs_mock", &reasoning));
            out.extend(text_item(1, &item_id, &content));
            out.push(completed(&id, 5));
            sse_from_pairs(out)
        }

        // The Anthropic wire's thinking block, rendered as this wire's
        // equivalent: a reasoning item whose signature becomes the encrypted
        // payload.
        Some(MockResponse::Thinking { text, signature }) => {
            let mut out = vec![created(&id)];
            out.push(frame(
                "response.output_item.added",
                serde_json::json!({
                    "output_index": 0,
                    "item": { "type": "reasoning", "id": "rs_mock", "summary": [] },
                }),
            ));
            out.push(frame(
                "response.reasoning_summary_text.delta",
                serde_json::json!({ "output_index": 0, "item_id": "rs_mock", "delta": text }),
            ));
            out.push(frame(
                "response.output_item.done",
                serde_json::json!({
                    "output_index": 0,
                    "item": {
                        "type": "reasoning",
                        "id": "rs_mock",
                        "summary": [{ "type": "summary_text", "text": text }],
                        "encrypted_content": signature,
                    },
                }),
            ));
            out.push(completed(&id, 5));
            sse_from_pairs(out)
        }

        Some(MockResponse::ToolCall { name, input }) => {
            let args = serde_json::to_string(&input).unwrap_or_default();
            let mut out = vec![created(&id)];
            out.extend(function_call_item(0, "fc_mock", &call_id, &name, &args));
            out.push(completed(&id, 10));
            sse_from_pairs(out)
        }

        Some(MockResponse::ToolCallStream {
            name,
            id: tid,
            input,
        }) => {
            let args = serde_json::to_string(&input).unwrap_or_default();
            let mut out = vec![created(&id)];
            out.extend(function_call_item(0, "fc_mock", &tid, &name, &args));
            out.push(completed(&id, 10));
            sse_from_pairs(out)
        }

        // Parallel tool use: one item per call, each at its own output index.
        Some(MockResponse::ToolCalls { calls }) => {
            let mut out = vec![created(&id)];
            for (index, (name, input)) in calls.iter().enumerate() {
                let args = serde_json::to_string(input).unwrap_or_default();
                let idx = u32::try_from(index).unwrap_or(0);
                out.extend(function_call_item(
                    idx,
                    &format!("fc_{index}"),
                    &format!("call_{}", uuid::Uuid::new_v4()),
                    name,
                    &args,
                ));
            }
            out.push(completed(&id, 10));
            sse_from_pairs(out)
        }

        // Deltas only: no `output_item.done`, no terminal frame.
        Some(MockResponse::CutStream { chunks, after }) => {
            let mut out = vec![created(&id), message_added(0, &item_id)];
            out.extend(
                chunks
                    .iter()
                    .take(after)
                    .map(|c| text_delta(0, &item_id, c))
                    .collect::<Vec<_>>(),
            );
            sse_from_pairs(out)
        }

        Some(MockResponse::CutToolCallStream {
            name,
            id: tid,
            partial_input_json,
        }) => sse_from_pairs(vec![
            created(&id),
            frame(
                "response.output_item.added",
                serde_json::json!({
                    "output_index": 0,
                    "item": {
                        "type": "function_call",
                        "id": "fc_mock",
                        "call_id": tid,
                        "name": name,
                        "arguments": "",
                    },
                }),
            ),
            frame(
                "response.function_call_arguments.delta",
                serde_json::json!({
                    "output_index": 0,
                    "item_id": "fc_mock",
                    "delta": partial_input_json,
                }),
            ),
        ]),

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

        None => {
            let mut out = vec![created(&id)];
            out.extend(text_item(0, &item_id, "No mock response queued"));
            out.push(completed(&id, 5));
            sse_from_pairs(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::MockLlmServer;

    async fn post_stream(server: &MockLlmServer) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{}/responses", server.url()))
            .json(&serde_json::json!({
                "model": "mock-model",
                "input": [{"type": "message", "role": "user",
                           "content": [{"type": "input_text", "text": "hi"}]}],
                "stream": true,
                "store": false
            }))
            .send()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn responses_streams_queued_text_and_completes() {
        let server = MockLlmServer::builder().build().await;
        server.queue_response("hi there");

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(body.contains("response.output_text.delta"), "body: {body}");
        assert!(body.contains("hi there"), "body: {body}");
        assert!(body.contains("response.completed"), "body: {body}");
    }

    #[tokio::test]
    async fn responses_streams_a_function_call_item() {
        let server = MockLlmServer::builder().build().await;
        server.queue_tool_call("echo", serde_json::json!({ "value": 42 }));

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(body.contains("function_call"), "body: {body}");
        assert!(body.contains("echo"), "body: {body}");
        assert!(
            body.contains("response.function_call_arguments.delta"),
            "body: {body}"
        );
    }

    /// The blob is what makes a thinking part replayable; without it the
    /// provider has nothing to put back on the next turn.
    #[tokio::test]
    async fn a_reasoning_turn_carries_encrypted_content() {
        let server = MockLlmServer::builder().build().await;
        server.queue_reasoning("weighing it up", "the answer");

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(body.contains("encrypted_content"), "body: {body}");
        assert!(
            body.contains("response.reasoning_summary_text.delta"),
            "body: {body}"
        );
    }

    #[tokio::test]
    async fn truncation_uses_the_incomplete_terminal_frame() {
        let server = MockLlmServer::builder().build().await;
        server.queue_truncated("cut off");

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(body.contains("response.incomplete"), "body: {body}");
        assert!(body.contains("max_output_tokens"), "body: {body}");
    }

    #[tokio::test]
    async fn a_cut_stream_omits_its_terminal_frame() {
        let server = MockLlmServer::builder().build().await;
        server.queue_cut_stream(["hel", "lo"], 1);

        let body = post_stream(&server).await.text().await.unwrap();

        assert!(body.contains("hel"), "body: {body}");
        assert!(
            !body.contains("response.completed"),
            "a cut stream must not complete: {body}"
        );
    }

    #[tokio::test]
    async fn errors_use_real_http_statuses() {
        let server = MockLlmServer::builder().build().await;
        server.queue_error(429, "slow down");

        assert_eq!(post_stream(&server).await.status().as_u16(), 429);
    }
}
