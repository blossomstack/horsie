//! What a dead process left dangling.
//!
//! A tool call the old process was running has no result and never will, and a
//! provider handed a `tool_use` with no matching `tool_result` rejects every
//! later turn. So the repair is a synthetic error result — recorded once, at
//! recovery, where it still belongs at the end of the transcript. Recomputing
//! it per turn instead is what let it drift into the middle of a history nobody
//! could then repair in place.
//!
//! The one call that must *not* be repaired is the ask an agent is parked on:
//! its answer is still coming.

use crate::agent_loop::prelude::*;
use horsie_agentcore::{ContentPart, Message, Role};
use horsie_models::now_ms;

/// What a synthetic result says stands in for a tool call that never finished.
pub(crate) const INTERRUPTED_RESULT: &str = "interrupted, no result was recorded";

/// The synthetic results a history is missing, in call order — the repair as
/// *messages to journal*, where [`repair_unanswered_tool_calls`] returns the
/// repaired history to put on the wire.
///
/// Called at the two moments a call becomes permanently unanswerable — a cancel
/// and a recovery — so the repair is recorded where it belongs, at the end of
/// the transcript as it stands. Nothing else needs to journal it: a call that is
/// still in flight is not missing a result, it just does not have one yet.
///
/// Every tool call this agent is currently parked on.
///
/// The exemption `missing_tool_results` needs, taken from state rather than
/// from tool names: these are the calls an answer will arrive against, so they
/// are exactly the dangling calls that are not wreckage.
pub(crate) fn parked_call_ids(state: &AgentState) -> Vec<String> {
    state
        .asks()
        .iter()
        .filter_map(|a| a.tool_call_id.clone())
        .collect()
}

/// A call the agent is parked on is exempt. Those park the agent — the run
/// ends on the call and the result comes later via `InjectToolResult` — so from
/// a journal alone a parked `ask_user` is indistinguishable from a call the dead
/// process was running, and recovery used to "repair" it. The user's answer was
/// then appended to a synthetic result already bearing the same `tool_use_id`,
/// and every later turn 400d on the duplicate. Idle offload made that routine:
/// any ask left unanswered past the idle timeout unloads and reloads.
///
/// Not journaling the repair is safe because [`repair_unanswered_tool_calls`]
/// still patches the history put on the wire, so an abandoned park can never
/// reach a provider dangling.
pub(crate) fn missing_tool_results(messages: &[Message], parked_on: &[String]) -> Vec<Message> {
    let answered: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
            ContentPart::Text(_)
            | ContentPart::ToolCall(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_)
            | ContentPart::Artifact(_) => None,
        })
        .collect();
    let dangling: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc)
                if !answered.contains(tc.id.as_str()) && !parked_on.contains(&tc.id) =>
            {
                Some(tc.id.clone())
            }
            ContentPart::ToolCall(_)
            | ContentPart::Text(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_)
            | ContentPart::Artifact(_) => None,
        })
        .collect();
    if dangling.is_empty() {
        return Vec::new();
    }
    synthetic_results(dangling).collect()
}

/// Make a history well-formed for the provider: every `tool_use`, in *any*
/// assistant message, must have a matching `tool_result`. Any missing one (a
/// tool call interrupted by Stop or a crash) gets a synthetic error result so
/// the model can retry.
///
/// Repairing only the last assistant message is not enough. A Stop mid-turn
/// journals the assistant's tool call with no outcome (#45); once later turns
/// push that message off the end, a history rebuilt from the journal carries an
/// unanswered `tool_use` mid-history and the provider rejects *every* subsequent
/// turn with a 400 — the session is bricked until the journal is repaired.
///
/// Each repair is placed where the wire expects the result: directly after its
/// assistant message, joining any run of real results already following it —
/// never appended to the end of a history that has moved on to later turns.
///
/// Since [`missing_tool_results`] journals the repair at the moment a call
/// becomes unanswerable, this should now find nothing. It stays as the guard on
/// the one thing that must never reach a provider, and costs one pass over an
/// in-memory history.
pub(crate) fn repair_unanswered_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    repair_dangling(messages, &std::collections::HashSet::new())
}

pub(crate) fn repair_dangling(
    messages: Vec<Message>,
    answering: &std::collections::HashSet<String>,
) -> Vec<Message> {
    let mut answered: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
            ContentPart::Text(_)
            | ContentPart::ToolCall(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_)
            | ContentPart::Artifact(_) => None,
        })
        .collect();
    answered.extend(answering.iter().cloned());

    // Insertion index → the call ids needing a synthetic result there.
    let mut repairs: std::collections::BTreeMap<usize, Vec<String>> =
        std::collections::BTreeMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role != Role::Assistant {
            continue;
        }
        let dangling: Vec<String> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall(tc) if !answered.contains(&tc.id) => Some(tc.id.clone()),
                ContentPart::ToolCall(_)
                | ContentPart::Text(_)
                | ContentPart::ToolResult(_)
                | ContentPart::Thinking(_)
                | ContentPart::SubAgentResult(_)
                | ContentPart::Artifact(_) => None,
            })
            .collect();
        if dangling.is_empty() {
            continue;
        }
        // Past the results this turn *did* record, so a partially-answered
        // parallel batch stays one contiguous run.
        let mut at = i + 1;
        while messages.get(at).is_some_and(|next| next.role == Role::Tool) {
            at += 1;
        }
        repairs.entry(at).or_default().extend(dangling);
    }
    if repairs.is_empty() {
        return messages;
    }

    let mut out =
        Vec::with_capacity(messages.len() + repairs.values().map(Vec::len).sum::<usize>());
    for (i, m) in messages.into_iter().enumerate() {
        if let Some(ids) = repairs.remove(&i) {
            out.extend(synthetic_results(ids));
        }
        out.push(m);
    }
    // Calls left dangling by the final assistant message land past the end.
    for (_, ids) in repairs {
        out.extend(synthetic_results(ids));
    }
    out
}

pub(crate) fn synthetic_results(ids: Vec<String>) -> impl Iterator<Item = Message> {
    ids.into_iter()
        .map(|id| Message::tool_result(id, INTERRUPTED_RESULT, true, Vec::new(), now_ms()))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use crate::agent_loop::prelude::*;
    use crate::agent_loop::agent_actor::testing::*;
    use horsie_agentcore::{ContentPart, Message, Role};
    use horsie_models::agent::{TextPart, ToolCallPart};

    #[test]
    fn repair_appends_error_results_for_dangling_tool_calls() {
        let history = vec![
            user_msg("do it"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc2".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                ],
            },
            Message::tool_result("tc1", "ok", false, Vec::new(), 0),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        // tc2 was dangling → an error tool_result is appended at the end.
        let last = fixed.last().unwrap();
        match &last.parts[0] {
            ContentPart::ToolResult(r) => {
                assert_eq!(r.tool_call_id, "tc2");
                assert!(r.is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn answering_a_pending_ask_does_not_also_repair_it() {
        // The shape every ask_user answer resumes from: the call is dangling
        // *because* the user's answer is the result, arriving as this run's
        // input. Repairing it here would put a synthetic "interrupted" result
        // and the real answer on one tool_use_id.
        let history = vec![
            Message::user("m1", "pick a color", 0),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "m2".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "ask1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({ "question": "which?" }),
                })],
            },
        ];

        let answering = std::collections::HashSet::from(["ask1".to_string()]);
        let fixed = repair_unanswered_tool_calls_except(history.clone(), &answering);
        assert_eq!(fixed.len(), history.len(), "nothing is repaired: {fixed:?}");

        // Without the exclusion it *is* repaired — the bug this guards.
        assert_eq!(repair_unanswered_tool_calls(history).len(), 3);
    }

    /// The history an agent parked on an `ask_user` recovers from: the call is
    /// dangling because the user has not answered *yet*, not because anything
    /// died. Journaling a repair for it here is what put a synthetic
    /// "interrupted" result and the real answer on one `tool_use_id` — the
    /// duplicate every later turn then 400s on.
    fn parked_on_ask() -> Vec<Message> {
        vec![
            user_msg("what should I remove?"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a1".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "ask1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({ "question": "which?" }),
                })],
            },
        ]
    }

    /// Keyed on the *calls* the agent is parked on, taken from `state.asks`,
    /// rather than on tool names: these are exactly the ids an answer will
    /// arrive against, so a call to the same tool in an earlier, finished turn
    /// is still repaired.
    #[test]
    fn recovery_does_not_repair_the_ask_the_session_is_parked_on() {
        let parked = vec!["ask1".to_string()];
        assert!(
            missing_tool_results(&parked_on_ask(), &parked).is_empty(),
            "a parked ask is awaiting its answer, not interrupted"
        );
        // Without the exemption it *is* repaired — the bug this guards, which
        // bricked every session offloaded while awaiting an answer.
        assert_eq!(missing_tool_results(&parked_on_ask(), &[]).len(), 1);
    }

    #[test]
    fn recovery_still_repairs_a_real_tool_call_left_dangling_beside_a_park() {
        let mut history = parked_on_ask();
        history.insert(1, assistant_call("a0", "died"));
        let repairs = missing_tool_results(&history, &["ask1".to_string()]);
        let ids: Vec<String> = repairs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["died".to_string()],
            "only the dead call is repaired"
        );
    }

    #[test]
    fn a_park_is_never_journaled_as_interrupted_but_is_still_repaired_on_the_wire() {
        // The safety net that makes not journaling the repair safe: an ask that
        // really is abandoned still reaches the provider well-formed.
        let history = parked_on_ask();
        assert!(missing_tool_results(&history, &["ask1".to_string()]).is_empty());
        assert!(
            unmatched_tool_uses(&repair_unanswered_tool_calls(history)).is_empty(),
            "the wire history must never carry a dangling tool_use"
        );
    }

    /// Every `tool_use` id in `messages` that has no matching `tool_result`
    /// anywhere — what the provider rejects a request for.
    fn unmatched_tool_uses(messages: &[Message]) -> Vec<String> {
        let answered: std::collections::HashSet<&str> = messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolCall(tc) if !answered.contains(tc.id.as_str()) => {
                    Some(tc.id.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn assistant_call(id: &str, call_id: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: id.into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: call_id.into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            })],
        }
    }

    /// The session-bricking case: a Stop left a dangling call mid-history, and
    /// later turns pushed it off the end. Sanitizing only the last assistant
    /// message leaves it unrepaired, and the provider 400s on every later turn.
    #[test]
    fn repair_fixes_dangling_tool_calls_before_the_last_assistant_message() {
        let history = vec![
            user_msg("read it"),
            assistant_call("a1", "stopped"), // Stop landed here: no result ever journaled
            user_msg("never mind, do this instead"),
            assistant_call("a2", "tc2"),
            Message::tool_result("tc2", "ok", false, Vec::new(), 0),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a3".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
            },
        ];
        let fixed = repair_unanswered_tool_calls(history);
        assert!(
            unmatched_tool_uses(&fixed).is_empty(),
            "dangling calls left in rebuilt history: {:?}",
            unmatched_tool_uses(&fixed)
        );
    }

    /// The repair must land where the wire expects a result — right after the
    /// assistant message that made the call — not appended to the end of a
    /// history that has moved on to later turns.
    #[test]
    fn repair_places_synthetic_result_next_to_its_assistant_message() {
        let history = vec![
            user_msg("read it"),
            assistant_call("a1", "stopped"),
            user_msg("never mind"),
            assistant_call("a2", "tc2"),
            Message::tool_result("tc2", "ok", false, Vec::new(), 0),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        match &fixed[2].parts[0] {
            ContentPart::ToolResult(r) => {
                assert_eq!(r.tool_call_id, "stopped");
                assert!(r.is_error);
            }
            other => panic!("expected the synthetic result at index 2, got {other:?}"),
        }
        assert_eq!(fixed[2].role, Role::Tool);
    }

    /// A partially-answered parallel batch: the synthetic result joins the run
    /// of real results, still ahead of the next user turn.
    #[test]
    fn repair_appends_to_an_existing_run_of_tool_results() {
        let history = vec![
            user_msg("do both"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a1".into(),
                role: Role::Assistant,
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc2".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                ],
            },
            Message::tool_result("tc1", "ok", false, Vec::new(), 0),
            user_msg("stop, do something else"),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        match &fixed[3].parts[0] {
            ContentPart::ToolResult(r) => assert_eq!(r.tool_call_id, "tc2"),
            other => panic!("expected tc2's result after tc1's, got {other:?}"),
        }
        assert_eq!(fixed.last().unwrap().role, Role::User);
    }

    #[test]
    fn repair_leaves_well_formed_history_untouched() {
        let history = vec![
            user_msg("do it"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "tc1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                })],
            },
            Message::tool_result("tc1", "ok", false, Vec::new(), 0),
        ];
        let before = history.len();
        let fixed = repair_unanswered_tool_calls(history);
        assert_eq!(fixed.len(), before);
    }
}
