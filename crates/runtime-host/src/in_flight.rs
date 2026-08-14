//! Every request outstanding against one runtime, and who issued it.
//!
//! **Per runtime, not per client.** A [`RuntimeClient`] is rebuilt on every
//! acquisition and each agent caches its own, so one runtime has several clients
//! at once. A reconciler handed one client's set would diff the runtime's answer
//! against one agent's calls and silently conclude that every other agent's
//! tool call on that sandbox was an orphan — then cancel it. So the set is
//! created once per `(runtime, incarnation)` and handed to every client built for
//! it.
//!
//! **A map, not a set.** `cancel_agent` deliberately cancels one agent's calls
//! through that agent's own client: against a flat per-runtime set, cancelling
//! one subagent would abort its siblings' tool calls mid-flight. Keying
//! `call_id → agent_id` serves both readers — the reconciler takes every key,
//! [`InFlight::of_agent`] narrows to one issuer, and orphan detection is the same
//! map read the other way round.
//!
//! [`RuntimeClient`]: crate::RuntimeClient

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// Outstanding `call_id`s against one runtime, each mapped to the agent that
/// issued it.
#[derive(Default, Debug)]
pub struct InFlight(Mutex<HashMap<String, String>>);

impl InFlight {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn track(&self, call_id: &str, agent_id: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(call_id.to_string(), agent_id.to_string());
    }

    pub fn untrack(&self, call_id: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(call_id);
    }

    /// Every outstanding call, whoever issued it — what a reconciler diffs the
    /// runtime's own answer against.
    #[must_use]
    pub fn all(&self) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .keys()
            .cloned()
            .collect()
    }

    /// Only this agent's calls — the bound `cancel_in_flight` must not exceed.
    #[must_use]
    pub fn of_agent(&self, agent_id: &str) -> Vec<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, issuer)| issuer.as_str() == agent_id)
            .map(|(call_id, _)| call_id.clone())
            .collect()
    }

    /// The agent that issued this call, if it is still outstanding. What turns an
    /// id the runtime reports into "somebody is waiting for this" — or, when it
    /// answers `None`, into an orphan.
    #[must_use]
    pub fn issuer_of(&self, call_id: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(call_id)
            .cloned()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn one_agents_calls_are_separable_from_its_siblings() {
        let in_flight = InFlight::new();
        in_flight.track("p1", "parent");
        in_flight.track("c1", "child");
        in_flight.track("c2", "child");

        assert_eq!(in_flight.of_agent("parent"), vec!["p1".to_string()]);
        let mut child = in_flight.of_agent("child");
        child.sort();
        assert_eq!(child, vec!["c1".to_string(), "c2".to_string()]);
        assert_eq!(in_flight.all().len(), 3, "and the reconciler sees them all");
    }

    #[test]
    fn an_untracked_call_has_no_issuer_which_is_what_makes_it_an_orphan() {
        let in_flight = InFlight::new();
        in_flight.track("c1", "a1");
        assert_eq!(in_flight.issuer_of("c1").as_deref(), Some("a1"));

        in_flight.untrack("c1");
        assert_eq!(in_flight.issuer_of("c1"), None);
        assert!(in_flight.is_empty());
    }

    /// A re-issued id must not leave two entries, and must not keep the old
    /// issuer: the second writer is the one waiting for the reply.
    #[test]
    fn tracking_the_same_call_twice_names_the_agent_that_did_it_last() {
        let in_flight = InFlight::new();
        in_flight.track("c1", "first");
        in_flight.track("c1", "second");
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight.issuer_of("c1").as_deref(), Some("second"));
    }
}
