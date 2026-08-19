//! The vocabulary of forking a conversation.
//!
//! The fork rosters and records that used to live here are gone: a fork is a
//! runner now, and its state is a slice of the session's runner tree. What
//! stays is the one piece both the command surface and the persisted events
//! speak — how a fork's history is seeded.

use serde::{Deserialize, Serialize};

/// How a fork's history was seeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForkMode {
    /// `/fork` — the source's log, copied and scrubbed.
    Copy,
    /// `/summary-n-fork` — a summary of the source, produced out of band.
    Summary,
}

impl ForkMode {
    /// The wire spelling, and what a lifecycle entry carries.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Summary => "summary",
        }
    }
}
