//! The token budget: whether a turn should compact, and to what target.
//!
//! # Policy, not state
//!
//! `context_tokens` — the last provider call's prompt size — is a projection
//! folded from run events, and it stays on
//! [`crate::agent_loop::state::AgentState`] with every other projection: it is
//! a fact every runner agrees on, and turning it into a capability would buy an
//! enum arm and a state field for nothing. What varies by runner is not the
//! fact but the *policy read against it* — the two percentages a session used
//! to get from server constants. This capability owns exactly that policy and
//! nothing else: no folded state, no events, no commands.
//!
//! # Why a hook and not a getter
//!
//! The one question a capability answers from outside is
//! [`super::Capability::carried_state`], and a compaction target is not it —
//! that is compaction's own input rather than a decision about it. So this
//! capability answers through the same mechanism every other decision in the
//! tree uses: [`super::Msg::TurnProposed`]
//! is broadcast before a turn's run starts, and this capability's answer comes
//! back as [`super::Act::CompactionBudget`]. The actor combines it with the one
//! thing this capability does not and should not hold — the model's own context
//! window, known only once the run's provider is resolved — to build what
//! agentcore's loop checks on every call.
//!
//! An agent equipped with none of these gets no [`super::Act::CompactionBudget`]
//! at all, and therefore never compacts. That silence is deliberate: see
//! [`crate::sessions::runners::assemble`], which is why every agent-owning
//! runner equips one unconditionally, the same as the task list and timers.

use super::{Act, Decision, Msg};
use serde::{Deserialize, Serialize};

/// The share of a model's context window at which an agent compacts, absent a
/// runner saying otherwise.
///
/// The value server constants used to carry outright. It moved here rather
/// than disappearing, because the reasoning that set it — a property of the
/// model, retunable centrally, with headroom for a check that lags one
/// provider call behind `context_tokens` — survives; only *where it lives*
/// changed.
pub const DEFAULT_TRIGGER_AT_PERCENT: u32 = 80;

/// Roughly how much of the window a compaction leaves as raw recent messages,
/// absent a runner saying otherwise.
///
/// Not zero, for the same reason it was not zero as a constant: a summary
/// alone loses the file path or error the agent was part-way through, and
/// those live in the last few messages.
pub const DEFAULT_RETAIN_PERCENT: u32 = 20;

/// One runner's answer to "should this turn compact, and to what target?"
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenBudgetCapability {
    trigger_at_percent: u32,
    retain_percent: u32,
}

impl TokenBudgetCapability {
    #[must_use]
    pub fn new(trigger_at_percent: u32, retain_percent: u32) -> Self {
        Self {
            trigger_at_percent,
            retain_percent,
        }
    }
}

impl Default for TokenBudgetCapability {
    /// The values every runner got before this was a capability. A workflow
    /// step with a fixed brief and a structured result may one day want
    /// something tighter — see the module doc — but nothing in this migration
    /// asks for that, so every runner equips exactly this.
    fn default() -> Self {
        Self::new(DEFAULT_TRIGGER_AT_PERCENT, DEFAULT_RETAIN_PERCENT)
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl TokenBudgetCapability {
    pub fn name(&self) -> &'static str {
        "token_budget"
    }

    pub fn handle(&self, msg: &Msg) -> Option<Decision> {
        match msg {
            Msg::TurnProposed => Some(Decision::default().then(Act::CompactionBudget {
                trigger_at_percent: self.trigger_at_percent,
                retain_percent: self.retain_percent,
            })),
            Msg::Turn(_)
            | Msg::Answer(_)
            | Msg::Child(_)
            | Msg::Reply(_)
            | Msg::Woke { .. }
            | Msg::Concluded
            | Msg::Loaded => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::{Capabilities, Capability, testing::someone_elses};

    fn budget(trigger: u32, retain: u32) -> crate::agent_loop::AgentState {
        crate::agent_loop::AgentState {
            capabilities: Capabilities::new(vec![Capability::TokenBudget(
                TokenBudgetCapability::new(trigger, retain),
            )]),
            ..crate::agent_loop::AgentState::default()
        }
    }

    /// The one thing this capability says, and the only message it says it on:
    /// a turn proposal answers with the config it was equipped with, matching
    /// what every session got when this was two server constants.
    #[test]
    fn a_turn_proposal_answers_with_the_configured_budget() {
        let state = budget(80, 20);
        let decision = super::super::broadcast(&state, &Msg::TurnProposed)
            .acts
            .into_iter()
            .find_map(|act| match act {
                Act::CompactionBudget {
                    trigger_at_percent,
                    retain_percent,
                } => Some((trigger_at_percent, retain_percent)),
                Act::Answer { .. }
                | Act::Park { .. }
                | Act::Resume { .. }
                | Act::Conclude { .. }
                | Act::Hold { .. }
                | Act::Refuse { .. }
                | Act::Enqueue { .. }
                | Act::Record(_)
                | Act::Wake { .. }
                | Act::Ask(_) => None,
            });
        assert_eq!(
            decision,
            Some((80, 20)),
            "the answer did not carry equip-time config"
        );
    }

    /// A runner that never called `new` still gets the values every session
    /// compacted against before this was a capability.
    #[test]
    fn the_default_matches_what_the_server_constants_used_to_say() {
        assert_eq!(
            (DEFAULT_TRIGGER_AT_PERCENT, DEFAULT_RETAIN_PERCENT),
            (80, 20)
        );
    }

    /// It has no tools, so it claims no command — including one built for a
    /// tool name, the same "not mine" shape every other capability is tested
    /// against.
    #[test]
    fn it_claims_no_commands_and_no_other_messages() {
        let state = budget(80, 20);
        assert!(super::super::dispatch(&state, &someone_elses()).is_none());
        for msg in [
            Msg::Turn(super::super::TurnEvent::Ended),
            Msg::Answer(&[]),
            Msg::Concluded,
            Msg::Loaded,
        ] {
            assert!(
                super::super::offer(&state, &msg).is_none(),
                "answered {msg:?} unasked"
            );
        }
    }

    /// Config survives the journal round trip untouched, the way any
    /// policy-only capability's has to for a reload to compact the same way
    /// the process that crashed would have.
    #[test]
    fn config_survives_the_journal_round_trip() {
        let written = serde_json::to_string(&budget(70, 30)).expect("write");
        let read: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        let decision = super::super::broadcast(&read, &Msg::TurnProposed);
        assert!(matches!(
            decision.acts.as_slice(),
            [Act::CompactionBudget {
                trigger_at_percent: 70,
                retain_percent: 30,
            }]
        ));
    }
}
