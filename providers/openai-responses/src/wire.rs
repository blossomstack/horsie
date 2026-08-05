//! Wire types for the OpenAI Responses API (`POST /responses`).
//!
//! Three structural differences from the chat-completions wire in
//! `horsie-openai`, each of which is why this is a separate crate rather than a
//! flag on that one:
//!
//! 1. History is a flat list of *items*, not messages with nested content —
//!    a tool call and its result are siblings of the assistant's text, not
//!    fields on it.
//! 2. Tool definitions are flat (`{type, name, parameters}`), where the chat
//!    wire nests them under a `function` object.
//! 3. Thinking is **replayed**, not dropped. Because horsie sends
//!    `store: false`, the backend keeps no copy of the turn, so the model sees
//!    its own reasoning again only if we hand the encrypted item back.

use horsie_models::agent::{ContentPart, Message, Role};
use serde::{Deserialize, Serialize};

// ── request ──────────────────────────────────────────────────────────────────

/// A tool, in the Responses API's flat shape.
#[derive(Debug, Clone, Serialize)]
pub struct FunctionTool {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReasoningControl {
    pub effort: String,
    /// Without a summary the UI has nothing to show: the raw chain of thought
    /// only ever comes back encrypted.
    pub summary: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<FunctionTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningControl>,
    /// Always false. horsie owns conversation state, and the ChatGPT backend
    /// rejects a stored response outright.
    pub store: bool,
    pub stream: bool,
    /// Asks for the encrypted chain of thought, which is the only form in which
    /// it can be replayed on the next turn.
    pub include: Vec<&'static str>,
}

// ── reasoning replay ─────────────────────────────────────────────────────────

/// What we must hand back to replay one reasoning item.
///
/// Carried inside `ThinkingPart.signature` — already the "opaque provider bytes,
/// give them back untouched" field that Anthropic uses for its own signature —
/// so replaying reasoning needs no change to the message schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningRef {
    pub id: String,
    #[serde(rename = "enc")]
    pub encrypted: String,
}

impl ReasoningRef {
    /// Parse a signature written by [`ReasoningRef::to_signature`].
    ///
    /// A signature from another provider (Anthropic's is a bare base64 string)
    /// parses as `None` rather than erroring: the same session can hold turns
    /// from more than one provider, and an unreplayable thinking part is
    /// dropped, not fatal.
    #[must_use]
    pub fn from_signature(sig: &str) -> Option<Self> {
        serde_json::from_str(sig).ok()
    }

    #[must_use]
    pub fn to_signature(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

// ── mapping ──────────────────────────────────────────────────────────────────

fn text_item(role: &str, text: &str) -> serde_json::Value {
    // `input_text` on the way in, `output_text` on the way back — the Responses
    // API uses different content types per direction, and rejects the wrong one.
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    serde_json::json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

/// Map horsie's provider-neutral history onto the Responses API's item list.
///
/// Ordering within a turn is preserved: reasoning, then text, then tool calls,
/// in the order the parts were produced. The model's own item order is what it
/// expects to see replayed.
#[must_use]
pub fn to_input_items(history: &[Message]) -> Vec<serde_json::Value> {
    let mut out = Vec::new();

    for msg in history {
        let role = match msg.role {
            Role::Assistant => "assistant",
            Role::User | Role::Tool => "user",
        };

        // Text accumulates across the turn's parts so that a message split into
        // several text parts stays one item, as it was when the model wrote it.
        let mut text = String::new();
        let flush_text = |text: &mut String, out: &mut Vec<serde_json::Value>| {
            if !text.is_empty() {
                out.push(text_item(role, text));
                text.clear();
            }
        };

        for part in &msg.parts {
            match part {
                ContentPart::Text(t) => text.push_str(&t.text),
                // Flattened to the text it has always been: this part is
                // provenance for clients, not a new thing to show the model.
                ContentPart::SubAgentResult(r) => text.push_str(&r.to_wire_text()),
                ContentPart::Thinking(t) => {
                    // Only a reasoning item we can reconstruct exactly is worth
                    // sending. Replaying the *summary* as plain text would put
                    // words in the assistant's mouth that it never said on the
                    // wire, and would not restore the reasoning anyway.
                    if let Some(r) = t
                        .signature
                        .as_deref()
                        .and_then(ReasoningRef::from_signature)
                    {
                        flush_text(&mut text, &mut out);
                        out.push(serde_json::json!({
                            "type": "reasoning",
                            "id": r.id,
                            "encrypted_content": r.encrypted,
                            "summary": [],
                        }));
                    }
                }
                ContentPart::ToolCall(tc) => {
                    flush_text(&mut text, &mut out);
                    out.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.name,
                        // A JSON *string*, not an object — as on the chat wire.
                        "arguments": serde_json::to_string(&tc.input).unwrap_or_default(),
                    }));
                }
                ContentPart::ToolResult(tr) => {
                    flush_text(&mut text, &mut out);
                    out.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": tr.tool_call_id,
                        "output": tr.output,
                    }));
                }
            }
        }

        flush_text(&mut text, &mut out);
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
    use horsie_models::agent::{
        SubAgentResultPart, TextPart, ThinkingPart, ToolCallPart, ToolResultPart,
    };

    fn msg(role: Role, parts: Vec<ContentPart>) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: "m1".into(),
            role,
            parts,
        }
    }

    #[test]
    fn user_text_becomes_an_input_text_message_item() {
        let items = to_input_items(&[msg(
            Role::User,
            vec![ContentPart::Text(TextPart { text: "hi".into() })],
        )]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][0]["text"], "hi");
    }

    /// The direction-specific content type is a real API constraint: an
    /// assistant item carrying `input_text` is rejected.
    #[test]
    fn assistant_text_uses_output_text() {
        let items = to_input_items(&[msg(
            Role::Assistant,
            vec![ContentPart::Text(TextPart {
                text: "answer".into(),
            })],
        )]);

        assert_eq!(items[0]["role"], "assistant");
        assert_eq!(items[0]["content"][0]["type"], "output_text");
    }

    #[test]
    fn a_tool_call_becomes_a_sibling_function_call_item() {
        let items = to_input_items(&[msg(
            Role::Assistant,
            vec![ContentPart::ToolCall(ToolCallPart {
                id: "call_a".into(),
                name: "echo".into(),
                input: serde_json::json!({"v": 1}),
            })],
        )]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_a");
        assert_eq!(items[0]["name"], "echo");
        // Arguments are a JSON string, not an object.
        assert_eq!(items[0]["arguments"], r#"{"v":1}"#);
    }

    #[test]
    fn a_tool_result_becomes_a_function_call_output_item() {
        let items = to_input_items(&[msg(
            Role::Tool,
            vec![ContentPart::ToolResult(ToolResultPart {
                tool_call_id: "call_a".into(),
                output: "result a".into(),
                is_error: false,
            })],
        )]);

        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call_output");
        assert_eq!(items[0]["call_id"], "call_a");
        assert_eq!(items[0]["output"], "result a");
    }

    /// The behaviour this whole crate exists for: with `store: false` the model
    /// only sees its own prior reasoning if we hand the encrypted item back.
    #[test]
    fn thinking_with_a_reasoning_signature_is_replayed_encrypted() {
        let sig = ReasoningRef {
            id: "rs_1".into(),
            encrypted: "gAAAA".into(),
        }
        .to_signature();

        let items = to_input_items(&[msg(
            Role::Assistant,
            vec![
                ContentPart::Thinking(ThinkingPart {
                    text: "summary the user saw".into(),
                    signature: Some(sig),
                }),
                ContentPart::Text(TextPart {
                    text: "answer".into(),
                }),
            ],
        )]);

        assert_eq!(items.len(), 2, "reasoning item, then the text item");
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[0]["id"], "rs_1");
        assert_eq!(items[0]["encrypted_content"], "gAAAA");
        // The human-readable summary is never sent back: it is not what the
        // model produced, and the encrypted blob already carries the real thing.
        assert_eq!(items[0]["summary"], serde_json::json!([]));
        assert_eq!(items[1]["content"][0]["text"], "answer");
    }

    #[test]
    fn thinking_without_a_usable_signature_is_dropped() {
        // Anthropic's signature is a bare string, not our JSON — a session that
        // switched providers must not blow up or send a malformed item.
        for sig in [None, Some("anthropic-style-signature".to_string())] {
            let items = to_input_items(&[msg(
                Role::Assistant,
                vec![ContentPart::Thinking(ThinkingPart {
                    text: "hmm".into(),
                    signature: sig,
                })],
            )]);

            assert!(items.is_empty(), "unreplayable thinking must be dropped");
        }
    }

    #[test]
    fn text_before_and_after_a_tool_call_keeps_its_order() {
        let items = to_input_items(&[msg(
            Role::Assistant,
            vec![
                ContentPart::Text(TextPart {
                    text: "first".into(),
                }),
                ContentPart::ToolCall(ToolCallPart {
                    id: "call_a".into(),
                    name: "echo".into(),
                    input: serde_json::json!({}),
                }),
                ContentPart::Text(TextPart {
                    text: "second".into(),
                }),
            ],
        )]);

        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["content"][0]["text"], "first");
        assert_eq!(items[1]["type"], "function_call");
        assert_eq!(items[2]["content"][0]["text"], "second");
    }

    #[test]
    fn a_turn_with_nothing_replayable_produces_no_items() {
        assert!(to_input_items(&[msg(Role::Assistant, vec![])]).is_empty());
    }

    #[test]
    fn a_subagent_result_reaches_the_wire_as_its_notification_text() {
        let items = to_input_items(&[msg(
            Role::User,
            vec![ContentPart::SubAgentResult(SubAgentResultPart {
                subagent_id: "id".into(),
                label: "audit".into(),
                status: "completed".into(),
                text: "three stale crates".into(),
                spawned_at_ms: 100,
                ended_at_ms: 400,
            })],
        )]);

        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]["content"][0]["text"],
            "[subagent \"audit\" completed]\n\nthree stale crates"
        );
    }

    #[test]
    fn a_reasoning_ref_round_trips_through_its_signature() {
        let r = ReasoningRef {
            id: "rs_1".into(),
            encrypted: "gAAAA".into(),
        };
        assert_eq!(ReasoningRef::from_signature(&r.to_signature()), Some(r));
        assert_eq!(ReasoningRef::from_signature("not json"), None);
    }
}
