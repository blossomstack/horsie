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

use super::*;
use horsie_agentcore::{ContentPart, Message, Role};
use horsie_models::now_ms;

/// What a synthetic result says stands in for a tool call that never finished.
pub(super) const INTERRUPTED_RESULT: &str = "interrupted, no result was recorded";

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
pub(super) fn parked_call_ids(state: &AgentState) -> Vec<String> {
    state
        .asks
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
pub(super) fn missing_tool_results(messages: &[Message], parked_on: &[String]) -> Vec<Message> {
    let answered: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
            ContentPart::Text(_)
            | ContentPart::ToolCall(_)
            | ContentPart::Thinking(_)
            | ContentPart::SubAgentResult(_) => None,
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
            | ContentPart::SubAgentResult(_) => None,
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
pub(super) fn repair_unanswered_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    repair_dangling(messages, &std::collections::HashSet::new())
}

/// [`repair_unanswered_tool_calls`] for the resume-from-ask path, where
/// `answering` are the tool calls this very command is supplying results for
/// (e.g. every `ask_user` of a parked turn). They are about to be answered for
/// real, so they are not
/// dangling: repairing it too would put *two* results on one `tool_use_id` — the
/// duplicate shape stricter providers reject outright, and pure noise for the
/// ones that don't.
pub(super) fn repair_unanswered_tool_calls_except(
    messages: Vec<Message>,
    answering: &std::collections::HashSet<String>,
) -> Vec<Message> {
    repair_dangling(messages, answering)
}

pub(super) fn repair_dangling(
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
            | ContentPart::SubAgentResult(_) => None,
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
                | ContentPart::SubAgentResult(_) => None,
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

pub(super) fn synthetic_results(ids: Vec<String>) -> impl Iterator<Item = Message> {
    ids.into_iter()
        .map(|id| Message::tool_result(id, INTERRUPTED_RESULT, true, now_ms()))
}
