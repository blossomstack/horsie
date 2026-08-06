//! Wire-boundary redaction: fields that exist only for provider replay and are
//! meaningless to API clients.

use horsie_models::agent::{AgentLogBody, AgentLogEntry, ContentPart, Message};

/// Drop thinking-block signatures from messages on their way to a client.
///
/// Signatures are opaque provider-replay artifacts — 4-13 KB each, and 37-46%
/// of a typical history response — that no client reads: the web transcript
/// renders `text` only (`clients/web/src/components/ThinkingBlock.tsx`). They
/// stay in the agent's in-memory state and journal, where provider replay needs
/// them; this strips only the copies handed to HTTP and SSE clients.
/// Neither a hook nor a lifecycle entry has a signature to strip — neither came
/// from a provider — so only the LLM entries are touched.
pub fn strip_entry_signatures(entries: &mut [AgentLogEntry]) {
    for entry in entries.iter_mut() {
        if let AgentLogBody::Llm(message) = &mut entry.body {
            strip_message_signature(message);
        }
    }
}

/// Single-message variant, for the SSE path.
pub fn strip_message_signature(message: &mut Message) {
    for part in message.parts.iter_mut() {
        if let ContentPart::Thinking(thinking) = part {
            thinking.signature = None;
        }
    }
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
    use horsie_models::agent::{Role, TextPart, ThinkingPart};

    fn entries(signature: Option<&str>) -> Vec<HistoryEntry> {
        vec![HistoryEntry::Llm(assistant_with_thinking(signature))]
    }

    fn parts(entries: &[HistoryEntry]) -> &[ContentPart] {
        match &entries[0] {
            HistoryEntry::Llm(m) => &m.parts,
            other => panic!("expected an Llm entry, got {other:?}"),
        }
    }

    fn assistant_with_thinking(signature: Option<&str>) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: "m1".into(),
            role: Role::Assistant,
            parts: vec![
                ContentPart::Thinking(ThinkingPart {
                    text: "step by step".into(),
                    signature: signature.map(Into::into),
                }),
                ContentPart::Text(TextPart {
                    text: "the answer".into(),
                }),
            ],
        }
    }

    #[test]
    fn strips_signature_and_keeps_thinking_text() {
        let mut es = entries(Some("opaque-blob"));
        strip_entry_signatures(&mut es);
        match &parts(&es)[0] {
            ContentPart::Thinking(th) => {
                assert_eq!(th.signature, None);
                assert_eq!(th.text, "step by step");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn leaves_non_thinking_parts_untouched() {
        let mut es = entries(Some("opaque-blob"));
        strip_entry_signatures(&mut es);
        match &parts(&es)[1] {
            ContentPart::Text(t) => assert_eq!(t.text, "the answer"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn tolerates_already_absent_signature() {
        let mut es = entries(None);
        strip_entry_signatures(&mut es);
        match &parts(&es)[0] {
            ContentPart::Thinking(th) => assert_eq!(th.signature, None),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    /// Redaction walks entries it cannot destructure as messages. A hook entry
    /// has no provider artifacts, so it must pass through byte-identical rather
    /// than be skipped by accident of ordering.
    #[test]
    fn a_hook_entry_passes_through_untouched() {
        use horsie_models::agent::HookEntry;
        let record = horsie_models::hooks::HookRecord {
            plugin: "guard".into(),
            duration_ms: 3,
            halt: None,
            action: horsie_models::hooks::HookAction::PreToolUse(
                horsie_models::hooks::PreToolUseRecord {
                    call: horsie_models::hooks::ToolScope {
                        tool: "bash".into(),
                        tool_call_id: "tc1".into(),
                    },
                    system_message: None,
                    outcome: horsie_models::hooks::PreToolUseOutcome::Denied(
                        horsie_models::hooks::HookDenied {
                            reason: Some("denied".into()),
                        },
                    ),
                },
            ),
        };
        let hook = HistoryEntry::Hook(HookEntry {
            id: "hook:0".into(),
            created_at_ms: 7,
            record,
        });
        let mut es = vec![hook.clone()];
        strip_entry_signatures(&mut es);
        assert_eq!(
            serde_json::to_string(&es[0]).unwrap(),
            serde_json::to_string(&hook).unwrap()
        );
    }

    #[test]
    fn single_message_variant_strips() {
        let mut msg = assistant_with_thinking(Some("opaque-blob"));
        strip_message_signature(&mut msg);
        match &msg.parts[0] {
            ContentPart::Thinking(th) => assert_eq!(th.signature, None),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }
}
