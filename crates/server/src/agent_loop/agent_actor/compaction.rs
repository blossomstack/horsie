//! Where the prompt starts.
//!
//! A compaction never deletes: the log keeps everything it held and a boundary
//! entry records where the *prompt* now begins. Folding one is therefore
//! deterministic under replay, so a recovered agent prompts from exactly the
//! boundary the live one did — which is the whole reason the boundary is an
//! append rather than a truncation.
//!
//! The one subtlety lives in [`AgentState::resolve_boundary`]: the run that
//! produced the boundary was holding a `Vec<Message>` in *prompt* order, which
//! is not log order. Resolving the two is the fold's job because the fold is the
//! only thing holding the log.

use super::*;
use horsie_agentcore::{AgentLogBody, CompactionEntry, ContentPart, Message, Role};
/// The one message a compaction boundary shows the model.
///
/// Two labelled sections rather than one blob, because they have different
/// truth conditions. The summary is the model's own prose and may be wrong at
/// the edges. The carried state is exact and must survive verbatim — a
/// summariser that renders "task 3: in progress" as prose has destroyed the id
/// the agent needs to call `task_list` correctly.
///
/// A `User` message because that is the role every provider accepts as
/// context-setting; there is no wire on which a synthetic assistant turn with
/// no request behind it is safe.
#[must_use]
pub fn boundary_message(entry: &CompactionEntry, at_ms: u64) -> Message {
    let text = format!(
        "This conversation was compacted: earlier history is summarised below \
         rather than shown in full. The messages after this one are verbatim.\n\n\
         ## Summary of earlier work\n{}\n\n## Current state\n{}",
        entry.summary.trim(),
        entry.carried_state.trim(),
    );
    Message {
        // Derived from the boundary's own position, never generated, for the
        // reason `hook:{n}` is: replay must reproduce the id it produced live.
        id: format!("compaction:{}", entry.covers_through_seq),
        role: Role::User,
        parts: vec![ContentPart::Text(horsie_models::agent::TextPart { text })],
        created_at_ms: at_ms,
        started_at_ms: None,
    }
}

/// The share of a model's context window at which an agent compacts.
///
/// A server constant rather than a session setting: the right value is a
/// property of the model, not of the session, so it stays retunable centrally
/// instead of frozen into everyone's saved presets. The headroom above it is
/// also what absorbs this check's one-iteration lag — `context_tokens` is the
/// last provider call's prompt size and does not count tool results appended
/// since.
pub(super) const COMPACT_AT_PERCENT: u32 = 80;

/// Roughly how much of the window a compaction leaves as raw recent messages.
///
/// Not zero, because a summary alone loses the file path or error the agent was
/// part-way through, and those live in the last few messages.
pub(super) const COMPACT_RETAIN_PERCENT: u32 = 20;

impl AgentState {
    /// What the model sees: the transcript, with every hook entry translated
    /// into the message it injects — most translate to nothing.
    ///
    /// The only way to obtain a `Vec<Message>` from state. `self.history` cannot
    /// be handed to a provider because the element types differ, so every kind of
    /// entry must state what, if anything, it shows the model;
    /// [`crate::agent_loop::hook_translation::translate`] is where that is decided, in one
    /// exhaustive match, and any future non-model entry inherits the obligation.
    pub fn prompt_messages(&self) -> Vec<Message> {
        // Where the prompt starts. Everything below the newest boundary is
        // represented by that boundary's own message and nothing else — which
        // is the only thing compaction changes about this function.
        let boundary = self.last_boundary();
        let from_seq = boundary.map_or(0, |(_, e)| e.retained_from_seq);

        boundary
            .map(|(at_ms, e)| boundary_message(e, at_ms))
            .into_iter()
            .chain(
                self.log
                    .iter()
                    .filter(|e| e.seq >= from_seq)
                    .filter_map(|e| match &e.body {
                        AgentLogBody::Llm(m) => Some(m.clone()),
                        AgentLogBody::Hook(h) => crate::agent_loop::hook_translation::translate(h),
                        // Every lifecycle variant, present and future. This arm is the
                        // reason `Lifecycle` is one union rather than nine flattened
                        // ones: provider isolation cannot be forgotten for a variant
                        // added later.
                        AgentLogBody::Lifecycle(_) => None,
                        // A boundary reached here is never the newest one — the
                        // newest was lifted out above — so it is history. Its
                        // span is already folded into the summary of every
                        // boundary that followed it, and replaying it would put
                        // the same account in the prompt twice.
                        AgentLogBody::Compaction(_) => None,
                    }),
            )
            .collect()
    }

    /// The newest compaction boundary and the moment it was taken, if this
    /// agent has ever compacted.
    ///
    /// A reverse scan rather than a stored pointer: state is a serialization
    /// contract, and a cached index is a second thing that can disagree with
    /// the log after a partial fold. Boundaries are rare and the scan stops at
    /// the first one.
    #[must_use]
    pub fn last_boundary(&self) -> Option<(u64, &CompactionEntry)> {
        self.log.iter().rev().find_map(|e| match &e.body {
            AgentLogBody::Compaction(c) => Some((e.at_ms, c)),
            AgentLogBody::Llm(_) | AgentLogBody::Hook(_) | AgentLogBody::Lifecycle(_) => None,
        })
    }

    /// Turn "the retained window starts at this message" into log sequence
    /// numbers: `(covers_through_seq, retained_from_seq)`.
    ///
    /// Deterministic under replay because it is a search of an append-only log
    /// from a fold that has already applied everything before this event — the
    /// same log, the same id, the same answer.
    ///
    /// Two cases collapse to "retain nothing": no id at all (a summary-only
    /// compaction), and an id the log does not hold. The second should not
    /// happen — the run built its history from this log — but the honest
    /// failure is to show the model the summary alone rather than to guess a
    /// seq and silently resurrect or drop messages around it.
    #[must_use]
    pub(super) fn resolve_boundary(&self, retained_from_message_id: Option<&str>) -> (u64, u64) {
        let retain_nothing = (self.tail_seq().unwrap_or(0), self.next_seq);
        let Some(id) = retained_from_message_id else {
            return retain_nothing;
        };
        let Some(idx) = self
            .log
            .iter()
            .position(|e| e.body.id().is_some_and(|got| got == id))
        else {
            tracing::warn!(
                message_id = id,
                "a compaction named a message this log does not hold; \
                 retaining nothing"
            );
            return retain_nothing;
        };
        let retained_from_seq = self.log[idx].seq;
        // The entry immediately before it in the log, read by position rather
        // than as `seq - 1`: the log is contiguous today, and this stays right
        // if it is ever front-trimmed.
        let covers_through_seq = idx
            .checked_sub(1)
            .map_or(retained_from_seq, |prev| self.log[prev].seq);
        (covers_through_seq, retained_from_seq)
    }

    /// The seq of every compaction boundary, oldest first.
    ///
    /// These are the conversation ids: conversation N is the span
    /// `(previous boundary, this boundary]`, so the boundary that closes a
    /// conversation is what names it. A client seeking across compactions pages
    /// on these.
    #[must_use]
    pub fn boundary_seqs(&self) -> Vec<u64> {
        self.log
            .iter()
            .filter(|e| matches!(e.body, AgentLogBody::Compaction(_)))
            .map(|e| e.seq)
            .collect()
    }
}

/// Where the prompt starts. Apply-only: nothing asks a compaction for
/// anything, it is a consequence of a run.
pub(super) struct Compaction;

impl Component for Compaction {
    /// Where the prompt now starts, and the context size that leaves behind.
    // The fallthrough is unreachable by construction: `AgentActor::apply_event`
    // routes every variant to exactly one module, so an event added later fails
    // to compile *there* — where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::Compacted {
                summary,
                carried_state,
                retained_from_message_id,
                trigger,
                instructions,
                tokens_before,
                tokens_after,
                at_ms,
            } => {
                let (covers_through_seq, retained_from_seq) =
                    state.resolve_boundary(retained_from_message_id.as_deref());
                let entry = CompactionEntry {
                    summary,
                    carried_state,
                    covers_through_seq,
                    retained_from_seq,
                    trigger,
                    instructions,
                    tokens_before,
                    tokens_after,
                };
                // `context_tokens` is what the next auto-compaction check
                // reads, and the whole point of a compaction is that this
                // number just dropped. Leaving it at the pre-compaction size
                // would make the very next turn compact again immediately.
                state.context_tokens = tokens_after;
                state.push(at_ms, AgentLogBody::Compaction(entry));
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
/// A [`CompactionPolicy`](horsie_agentcore::CompactionPolicy) for agents that
/// have no budget, so it is never consulted. Tests that exercise the retry loop
/// need one to pass and nothing to happen.
#[cfg(test)]
pub(super) struct NeverCompacts;

#[cfg(test)]
#[async_trait]
impl horsie_agentcore::CompactionPolicy for NeverCompacts {
    async fn carried_state(&self) -> String {
        String::new()
    }
    async fn before(
        &self,
        _: &horsie_agentcore::CompactionPlan,
    ) -> horsie_agentcore::PreCompactDecision {
        horsie_agentcore::PreCompactDecision::Proceed
    }
    async fn after(&self, _: &horsie_agentcore::CompactionResult) {}
}
