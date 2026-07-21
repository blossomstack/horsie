//! Wire types for the OpenAI-compatible `/v1/chat/completions` endpoint.
//!
//! Deliberately lenient: every response field is `#[serde(default)]` and
//! unknown fields are ignored, because "OpenAI-compatible" is a family of
//! near-misses — Ollama, vLLM and llama.cpp each omit or add fields relative
//! to OpenAI proper. Strict typing here would fail on exactly the backends
//! this crate exists to support.

use horsie_models::agent::{ContentPart, Message, Role};
use serde::{Deserialize, Serialize};

// ── request ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct FunctionCall {
    pub name: String,
    /// A JSON *string*, not an object — the OpenAI schema requires this.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    #[must_use]
    pub fn new(role: &str, content: Option<String>) -> Self {
        Self {
            role: role.to_string(),
            content,
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
}

// ── response (streaming chunks) ──────────────────────────────────────────────

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeltaFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeltaToolCall {
    /// Ties fragments together across frames; continuation frames often carry
    /// only this and an `arguments` slice.
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<DeltaFunction>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    /// Reasoning trace on reasoning models. DeepSeek (`deepseek-reasoner`) and
    /// vLLM (started with a `--reasoning-parser`) stream it here, as a separate
    /// channel that precedes `content`.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// OpenRouter streams the same trace under `reasoning` instead.
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<DeltaToolCall>>,
}

impl Delta {
    /// This delta's reasoning trace, under whichever field the backend uses:
    /// `reasoning_content` (DeepSeek, vLLM) or `reasoning` (OpenRouter).
    #[must_use]
    pub fn reasoning_trace(&self) -> Option<&str> {
        self.reasoning_content
            .as_deref()
            .or(self.reasoning.as_deref())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Choice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WireUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// Many compatible backends never send usage on stream frames.
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

// ── mapping ──────────────────────────────────────────────────────────────────

/// Map horsie's provider-neutral history onto OpenAI's message list.
///
/// Three structural differences from the Anthropic mapping:
/// 1. Each `ToolResult` part becomes its own `role: "tool"` message, keyed by
///    `tool_call_id`, rather than a content block on a `user` turn.
/// 2. Assistant tool calls move into the `tool_calls` field, with arguments
///    serialized to a JSON string.
/// 3. Thinking parts are dropped — there is no equivalent, and replaying one
///    would send a field the backend never produced.
#[must_use]
pub fn to_wire_messages(history: &[Message]) -> Vec<ChatMessage> {
    let mut out = Vec::new();

    for msg in history {
        let mut text = String::new();
        let mut calls: Vec<ToolCall> = Vec::new();

        for part in &msg.parts {
            match part {
                ContentPart::Text(t) => text.push_str(&t.text),
                ContentPart::ToolCall(tc) => calls.push(ToolCall {
                    id: tc.id.clone(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: tc.name.clone(),
                        arguments: serde_json::to_string(&tc.input).unwrap_or_default(),
                    },
                }),
                ContentPart::ToolResult(tr) => {
                    let mut m = ChatMessage::new("tool", Some(tr.output.clone()));
                    m.tool_call_id = Some(tr.tool_call_id.clone());
                    out.push(m);
                }
                ContentPart::Thinking(_) => {}
            }
        }

        // A turn that contributed only tool messages (or only dropped thinking)
        // has nothing left to say; emitting an empty message would be a stray
        // turn the backend still charges for.
        if text.is_empty() && calls.is_empty() {
            continue;
        }

        let role = match msg.role {
            Role::Assistant => "assistant",
            Role::User | Role::Tool => "user",
        };

        let mut m = ChatMessage::new(role, (!text.is_empty()).then_some(text));
        if !calls.is_empty() {
            m.tool_calls = Some(calls);
        }
        out.push(m);
    }

    out
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use horsie_models::agent::{TextPart, ThinkingPart, ToolCallPart, ToolResultPart};

    #[test]
    fn tool_results_become_their_own_tool_role_messages() {
        // Anthropic collapses Role::Tool into `user` and carries results as
        // content blocks. OpenAI needs one `role: "tool"` message per result,
        // keyed by tool_call_id. This is the structural remap.
        let history = vec![Message {
            id: "m1".into(),
            role: Role::Tool,
            parts: vec![
                ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "call_a".into(),
                    output: "result a".into(),
                    is_error: false,
                }),
                ContentPart::ToolResult(ToolResultPart {
                    tool_call_id: "call_b".into(),
                    output: "result b".into(),
                    is_error: false,
                }),
            ],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 2, "one tool message per result");
        assert_eq!(wire[0].role, "tool");
        assert_eq!(wire[0].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(wire[0].content.as_deref(), Some("result a"));
        assert_eq!(wire[1].tool_call_id.as_deref(), Some("call_b"));
    }

    #[test]
    fn assistant_tool_calls_move_into_the_tool_calls_field() {
        let history = vec![Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: "call_a".into(),
                name: "echo".into(),
                input: serde_json::json!({"v": 1}),
            })],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "assistant");
        let calls = wire[0].tool_calls.as_ref().expect("tool_calls present");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].function.name, "echo");
        // Arguments are a JSON *string*, not an object — a real OpenAI quirk.
        assert_eq!(calls[0].function.arguments, r#"{"v":1}"#);
    }

    #[test]
    fn thinking_parts_are_dropped() {
        let history = vec![Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![
                ContentPart::Thinking(ThinkingPart {
                    text: "hmm".into(),
                    signature: Some("sig".into()),
                }),
                ContentPart::Text(TextPart {
                    text: "answer".into(),
                }),
            ],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].content.as_deref(), Some("answer"));
    }

    #[test]
    fn a_turn_of_only_thinking_produces_no_message() {
        let history = vec![Message {
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![ContentPart::Thinking(ThinkingPart {
                text: "hmm".into(),
                signature: None,
            })],
        }];

        assert!(to_wire_messages(&history).is_empty());
    }

    #[test]
    fn reasoning_trace_prefers_reasoning_content_then_reasoning() {
        // DeepSeek/vLLM use `reasoning_content`; OpenRouter uses `reasoning`.
        let dcv: Delta =
            serde_json::from_str(r#"{"reasoning_content":"a","reasoning":"b"}"#).unwrap();
        assert_eq!(dcv.reasoning_trace(), Some("a"));

        let orv: Delta = serde_json::from_str(r#"{"reasoning":"b"}"#).unwrap();
        assert_eq!(orv.reasoning_trace(), Some("b"));

        let none: Delta = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert_eq!(none.reasoning_trace(), None);
    }

    #[test]
    fn user_text_maps_to_a_user_message() {
        let history = vec![Message {
            id: "m1".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: "hi".into() })],
        }];

        let wire = to_wire_messages(&history);

        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0].role, "user");
        assert_eq!(wire[0].content.as_deref(), Some("hi"));
    }
}
