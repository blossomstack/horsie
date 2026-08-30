//! A durable, at-least-once event log, one stream per project.
//!
//! Distinct from [`crate::bus`], and the two are not alternatives. The bus is
//! live fan-out with no memory: a frame published to nobody is gone, which is
//! the *correct* contract for what rides it — runtime tool calls, where a
//! redelivery would execute the call a second time, and counters a reader can
//! re-derive. This log is the opposite trade: every event is stored, every
//! consumer group has its own cursor, and nothing is dropped because a
//! subscriber happened to be elsewhere.
//!
//! # The id is a function of the event
//!
//! Every event answers [`ProjectEvent::id`], and that answer is computed from
//! the event's own named fields — never from a hash of its serialised bytes.
//! Two reasons. A hash would collapse two genuinely distinct occurrences into
//! one event; and `serde_json`'s map ordering shifts under feature
//! unification, so a hash of the JSON is build-dependent and would differ
//! between a per-crate and a `--workspace` build.
//!
//! Because the id is derived rather than carried, a producer and a consumer
//! cannot disagree about it: there is one definition and both call it. The
//! unique index on `(project, stream, event_id)` then makes appending the same
//! event twice a no-op, which is what lets a producer retry after a crash
//! without writing a duplicate.
//!
//! **An event that describes an occurrence carries its own discriminator.** A
//! status is idempotent — "this agent reached `failed`" is true once and the
//! fields that state it are the natural key. A resumption is not: an agent
//! parks and resumes many times, and collapsing two of those into one event
//! would leave the second one's questions open for ever. Those variants take a
//! producer-minted field (`at_ms`, or a uuid) so that `id()` stays a pure
//! function of the event while still telling two occurrences apart.

mod store;

pub use store::{DbEventLog, Delivered, EventLog, EventLogError};

use serde::{Deserialize, Serialize};

/// Something that happened in a project which somebody may want to react to.
///
/// Not only what a session actor derives. A user action has no journal and no
/// sequence number behind it, and belongs on this log just as much — which is
/// why nothing here assumes a session actor produced it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectEvent {
    /// An agent's run reached a state worth indexing.
    ///
    /// The id covers the fields that *define* that state, so re-observing an
    /// unchanged run dedups away and a status change is a new event. This is
    /// the whole reason the agent-run index can be fed from a lossy producer
    /// without a sequence number.
    AgentRunObserved {
        session_id: String,
        agent_id: String,
        preset: Option<String>,
        status: String,
        started_at: i64,
        ended_at: Option<i64>,
    },
    /// An agent stopped and asked the person a question.
    ///
    /// Keyed by `tool_call_id`, which is unique by construction and is also the
    /// only thing an answer can be addressed to.
    AgentParked {
        session_id: String,
        agent_id: String,
        question: String,
        choices: Vec<String>,
        multiple: bool,
        tool_call_id: String,
    },
    /// An agent that was waiting is waiting no longer.
    ///
    /// `at_ms` is the discriminator, and it is load-bearing rather than
    /// decoration: without it two resumptions of the same agent share an id,
    /// the second dedups away, and every question it should have settled stays
    /// open forever.
    AgentResumed {
        session_id: String,
        agent_id: String,
        at_ms: i64,
    },
}

impl ProjectEvent {
    /// What both sides compute, and what the log dedups on.
    #[must_use]
    pub fn id(&self) -> String {
        match self {
            Self::AgentRunObserved {
                session_id,
                agent_id,
                status,
                ended_at,
                ..
            } => join([
                "agent-run",
                session_id,
                agent_id,
                status,
                &ended_at.map(|v| v.to_string()).unwrap_or_default(),
            ]),
            Self::AgentParked {
                session_id,
                agent_id,
                tool_call_id,
                ..
            } => join(["agent-parked", session_id, agent_id, tool_call_id]),
            Self::AgentResumed {
                session_id,
                agent_id,
                at_ms,
            } => join(["agent-resumed", session_id, agent_id, &at_ms.to_string()]),
        }
    }
}

/// Join id segments so that no combination of field values can produce the id
/// of a different combination.
///
/// `tool_call_id` is provider-supplied and may contain anything at all, so a
/// bare `a:b` join is not safe: `("x:y", "z")` and `("x", "y:z")` would render
/// identically and one event would silently dedup away the other. Escaping the
/// separator — and the escape — removes that.
fn join<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .map(|p| p.replace('\\', r"\\").replace(':', r"\:"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn observed(status: &str, ended_at: Option<i64>) -> ProjectEvent {
        ProjectEvent::AgentRunObserved {
            session_id: "s1".into(),
            agent_id: "main".into(),
            preset: Some("reviewer".into()),
            status: status.into(),
            started_at: 1_000,
            ended_at,
        }
    }

    /// The property the whole design rests on: a producer that re-derives the
    /// same state computes the same id, so a retry after a crash appends
    /// nothing rather than duplicating a row.
    #[test]
    fn re_observing_an_unchanged_run_yields_the_same_id() {
        assert_eq!(
            observed("running", None).id(),
            observed("running", None).id()
        );
    }

    /// And the other half: a state that has actually moved is a new event, or
    /// the index would never learn the run finished.
    #[test]
    fn a_run_that_changed_state_is_a_different_event() {
        assert_ne!(
            observed("running", None).id(),
            observed("failed", Some(5_000)).id()
        );
    }

    /// `preset` and `started_at` are deliberately outside the id: they do not
    /// change over a run's life, and including them would only add ways for two
    /// observations of one state to disagree.
    #[test]
    fn fields_that_cannot_change_do_not_affect_the_id() {
        let mut other = observed("running", None);
        if let ProjectEvent::AgentRunObserved { preset, .. } = &mut other {
            *preset = None;
        }
        assert_eq!(observed("running", None).id(), other.id());
    }

    /// Two resumptions of one agent must not share an id. If they did, the
    /// second would dedup away and the questions it should have settled would
    /// stay open for ever — the failure this discriminator exists to prevent.
    #[test]
    fn two_resumptions_of_the_same_agent_are_two_events() {
        let at = |at_ms| ProjectEvent::AgentResumed {
            session_id: "s1".into(),
            agent_id: "main".into(),
            at_ms,
        };
        assert_ne!(at(1_000).id(), at(2_000).id());
    }

    /// A `tool_call_id` is provider-supplied and may contain the separator.
    /// Without escaping, these two distinct parks render one id and one of the
    /// questions never reaches the inbox.
    #[test]
    fn a_separator_inside_a_field_cannot_forge_another_events_id() {
        let park = |agent: &str, call: &str| ProjectEvent::AgentParked {
            session_id: "s1".into(),
            agent_id: agent.into(),
            question: "?".into(),
            choices: vec![],
            multiple: false,
            tool_call_id: call.into(),
        };
        assert_ne!(park("a:b", "c").id(), park("a", "b:c").id());
    }

    /// Two variants must never collide, whatever their fields. The leading tag
    /// is what guarantees it.
    #[test]
    fn different_kinds_of_event_never_share_an_id() {
        let resumed = ProjectEvent::AgentResumed {
            session_id: "s1".into(),
            agent_id: "main".into(),
            at_ms: 1,
        };
        assert_ne!(observed("running", None).id(), resumed.id());
    }
}
