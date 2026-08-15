//! The two id spaces a session addresses, and the vocabulary every runner
//! record is described in.
//!
//! [`RunnerId`] and [`AgentId`] are distinct newtypes over `Uuid` on purpose.
//! The shape this replaces had one flat uuid space in which a fork, a subagent
//! and a workflow step were told apart by probing three registries in a fixed
//! order — and getting that order wrong made a fork of a fork read as a fork of
//! a subagent. Two types, and a lookup rather than a probe, is what removes the
//! question.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Default,
            Serialize,
            Deserialize,
        )]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new_v4() -> Self {
                Self(Uuid::new_v4())
            }

            /// The uuid inside, for the places that still speak in raw ids —
            /// an agent's journal key above all.
            #[must_use]
            pub fn as_uuid(self) -> Uuid {
                self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    RunnerId,
    "One unit of work a session hosts: a conversation, a delegated task, a run."
);
id_type!(
    AgentId,
    "One agent a runner started. Its transcript is the journal `agent/<id>`."
);

/// What a runner is.
///
/// Decides which impl the session instantiates for a record, and nothing else:
/// every behavioural difference lives in that impl rather than in a match on
/// this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunnerKind {
    /// The session's conversation, or one of its forks.
    Conversation,
    /// One delegated worker, which reports once to the agent that asked.
    SubAgent,
    /// One run of a workflow graph, owning step agents over time.
    Workflow,
    /// The sandbox. The only kind that owns no agents.
    Runtime,
}

/// The same six words for every kind of runner.
///
/// What each one *means* is the runner's business; that they are spelled once
/// is what stops a session's status and a runner's from disagreeing — the
/// shape this replaces derived the session's status from thirteen separate
/// literals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum RunnerStatus {
    /// Created, and nothing started yet. Where every runner begins, and the
    /// state `actions()` reads as "start my first agent".
    #[default]
    Pending,
    Running,
    /// Parked on a question. Only a runner whose agents can ask reaches it.
    AwaitingInput,
    Done,
    Failed,
    Cancelled,
}

impl RunnerStatus {
    /// Whether this runner will start nothing further by itself.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// The two id spaces must not be interchangeable by accident. This is a
    /// compile-time property; the test exists so that anyone tempted to make
    /// one a type alias of the other has to delete a test that says why not.
    #[test]
    fn the_two_id_spaces_are_distinct_types() {
        let r = RunnerId::new_v4();
        let a = AgentId::new_v4();
        assert_ne!(r.as_uuid(), a.as_uuid());
    }

    #[test]
    fn only_the_three_endings_are_terminal() {
        assert!(RunnerStatus::Done.is_terminal());
        assert!(RunnerStatus::Failed.is_terminal());
        assert!(RunnerStatus::Cancelled.is_terminal());
        assert!(!RunnerStatus::Pending.is_terminal());
        assert!(!RunnerStatus::Running.is_terminal());
        assert!(!RunnerStatus::AwaitingInput.is_terminal());
    }
}
