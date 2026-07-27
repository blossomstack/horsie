//! Wire-boundary redaction: fields that exist only for provider replay and are
//! meaningless to API clients.

use horsie_models::agent::{ContentPart, Message};

/// Drop thinking-block signatures from messages on their way to a client.
///
/// Signatures are opaque provider-replay artifacts — 4-13 KB each, and 37-46%
/// of a typical history response — that no client reads: the web transcript
/// renders `text` only (`clients/web/src/components/ThinkingBlock.tsx`). They
/// stay in the agent's in-memory state and journal, where provider replay needs
/// them; this strips only the copies handed to HTTP and SSE clients.
pub fn strip_thinking_signatures(messages: &mut [Message]) {
    for message in messages.iter_mut() {
        strip_message_signature(message);
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

    fn assistant_with_thinking(signature: Option<&str>) -> Message {
        Message {
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
        let mut msgs = vec![assistant_with_thinking(Some("opaque-blob"))];
        strip_thinking_signatures(&mut msgs);
        match &msgs[0].parts[0] {
            ContentPart::Thinking(th) => {
                assert_eq!(th.signature, None);
                assert_eq!(th.text, "step by step");
            }
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn leaves_non_thinking_parts_untouched() {
        let mut msgs = vec![assistant_with_thinking(Some("opaque-blob"))];
        strip_thinking_signatures(&mut msgs);
        match &msgs[0].parts[1] {
            ContentPart::Text(t) => assert_eq!(t.text, "the answer"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn tolerates_already_absent_signature() {
        let mut msgs = vec![assistant_with_thinking(None)];
        strip_thinking_signatures(&mut msgs);
        match &msgs[0].parts[0] {
            ContentPart::Thinking(th) => assert_eq!(th.signature, None),
            other => panic!("expected Thinking, got {other:?}"),
        }
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
