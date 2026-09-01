//! What this agent has spent, and how much of the window it is holding.
//!
//! Its own part rather than a corner of the turn's, because three components
//! write to it: a turn banks what its calls cost, a compaction banks its
//! summarising call, and a summary taken for a sub session banks its own. None
//! of them can reach these fields — they call [`UsageState::bank`], which is
//! the one rule for how a cost is added, stated once.

use super::*;
use horsie_agentcore::Usage;

/// The running bill, and the size of the prompt behind it.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UsageState {
    /// Cumulative usage across everything this agent has ever spent. `u64`, so
    /// a long session's re-sent-context input total cannot overflow the
    /// per-call `u32` wire counters.
    total: UsageTotal,
    /// The most recently completed turn's own usage — summed across that
    /// turn's calls, never across turns. `None` before the first one.
    last_turn: Option<Usage>,
    /// The last provider call's prompt size *alone*, never summed: what is
    /// loaded in this agent's context right now, and what the compaction
    /// trigger is compared against.
    context_tokens: u32,
}

impl UsageState {
    pub(super) fn total(&self) -> UsageTotal {
        self.total
    }

    pub(super) fn last_turn(&self) -> Option<&Usage> {
        self.last_turn.as_ref()
    }

    pub(super) fn context_tokens(&self) -> u32 {
        self.context_tokens
    }

    /// Add what a call cost. Everything spent goes through here, whoever spent
    /// it.
    pub(super) fn bank(&mut self, usage: &Usage) {
        self.total.add(usage);
    }

    /// Record the end of a turn: its own cost, and where it left the context.
    pub(super) fn turn_ended(&mut self, usage: Usage, context_tokens: u32, keep: bool) {
        self.total.add(&usage);
        self.context_tokens = context_tokens;
        // A turn that never finished has no "last turn" figure to show: the
        // number is a completed turn's cost, and half of one would read as a
        // cheap turn rather than an interrupted one.
        if keep {
            self.last_turn = Some(usage);
        }
    }

    /// What the newest provider call was charged for its prompt.
    pub(super) fn context_is(&mut self, tokens: u32) {
        self.context_tokens = tokens;
    }
}

impl PartState for UsageState {
    /// The context size carries; the bill does not. A sub session that
    /// inherited `total` would make the session's aggregate count the same
    /// tokens twice, once under each session — while the history it adopts
    /// really does occupy that much of a window.
    fn carried(&self) -> Option<Self> {
        Some(Self {
            total: UsageTotal::default(),
            last_turn: None,
            context_tokens: self.context_tokens,
        })
    }
}
