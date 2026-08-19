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
//! # Why the actor asks, and does not read a field
//!
//! The two percentages are config, so a getter would do — but the question is
//! asked at one exact moment and nowhere else. The agent actor asks *before* a
//! turn's run is built, because the answer has to be in hand by the time the
//! run's task combines it with the one thing this capability does not and
//! should not hold: the model's own context window, known only once the run's
//! provider is resolved. Together they are what agentcore's loop checks on
//! every call.
//!
//! An agent equipped with no such capability is asked nothing and never
//! compacts. That silence is deliberate: see
//! [`crate::sessions::runners::assemble`], which is why every agent-owning
//! runner equips one unconditionally, the same as the task list and timers.

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

    /// What this turn should compact at, and to what.
    ///
    /// `(trigger_at_percent, retain_percent)`, in that order, which is the
    /// order the pair is read in everywhere it travels.
    #[must_use]
    pub(crate) fn target(&self) -> (u32, u32) {
        (self.trigger_at_percent, self.retain_percent)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::{Capabilities, Capability};

    fn budget(trigger: u32, retain: u32) -> crate::agent_loop::AgentState {
        crate::agent_loop::AgentState {
            capabilities: Capabilities::new(vec![Capability::TokenBudget(
                TokenBudgetCapability::new(trigger, retain),
            )]),
            ..crate::agent_loop::AgentState::default()
        }
    }

    /// The one thing this capability says: the target a turn proposal is
    /// answered with is the config it was equipped with, matching what every
    /// session got when this was two server constants.
    #[test]
    fn a_turn_proposal_answers_with_the_configured_budget() {
        let state = budget(80, 20);
        assert_eq!(
            state
                .capabilities
                .token_budget()
                .map(TokenBudgetCapability::target),
            Some((80, 20)),
            "the answer did not carry equip-time config"
        );
    }

    /// An agent equipped with no such capability is asked nothing, and its runs
    /// never compact. Deliberately loud in a test, never at runtime.
    #[test]
    fn an_agent_without_one_has_no_target_at_all() {
        let state = crate::agent_loop::AgentState::default();
        assert!(state.capabilities.token_budget().is_none());
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

    /// Config survives the journal round trip untouched, the way any
    /// policy-only capability's has to for a reload to compact the same way
    /// the process that crashed would have.
    #[test]
    fn config_survives_the_journal_round_trip() {
        let written = serde_json::to_string(&budget(70, 30)).expect("write");
        let read: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        assert_eq!(
            read.capabilities
                .token_budget()
                .map(TokenBudgetCapability::target),
            Some((70, 30))
        );
    }
}
