//! The pure decisions a session makes about work it owns: which agents are owed
//! a finished subagent's result, and — for a run — which step goes next.
//!
//! Turns are not among them. An agent holds its own queue and decides when that
//! queue becomes a turn, so what used to be `main_turn` is now
//! [`horsie_workflow::queued_turn`], asked by the agent against its own state.
//! What is left here is delivery: the session owns the forest, so it is the only
//! thing that knows a child's result is owed to a parent, and its job ends at
//! putting that result in the parent's queue.
//!
//! No actors, no I/O, no clock — so this is unit-testable against a hand-built
//! [`SessionState`]. Called from the component that owns it
//! ([`SubAgents`](crate::sessions::session_actor)), which is why there is no
//! strategy trait: the actor concatenates what its components return rather than
//! delegating the whole decision to one object.

use crate::sessions::session_actor::{AgentKey, SessionState};
use crate::sessions::subagents::{SubAgentParent, TreeOwner};
use horsie_models::agent::SubAgentResultPart;
use serde_json::Value;
use uuid::Uuid;

/// Something the actor should do. Every field is what the actor needs to
/// journal the action, so the actor never re-derives a decision.
#[derive(Debug, Clone)]
pub enum AgentAction {
    /// Begin one execution of one workflow step.
    StartStep(StepStart),
    /// The run is over and succeeded, carrying the last step's output.
    Finish { output: Value },
    /// The run is over and failed.
    Fail { error: String },
    /// Put a finished subagent's result in the queue of the agent that spawned
    /// it.
    Deliver(Delivery),
}

/// One execution of one workflow step. Carries everything needed to both spawn
/// the agent and journal the log entry, so the actor never re-derives a
/// decision.
#[derive(Debug, Clone)]
pub struct StepStart {
    pub index: u32,
    pub step: String,
    pub agent: Uuid,
    pub attempt: u32,
    /// The entry this came out of; `None` for the start step.
    pub from: Option<u32>,
    /// The transition condition that matched, if any.
    pub via: Option<String>,
    pub input: String,
}

/// One finished subagent's result, on its way to the agent that is owed it.
#[derive(Debug, Clone)]
pub struct Delivery {
    /// Whose queue it goes in.
    pub to: AgentKey,
    /// The subagent whose result this is, so the session can record that it has
    /// now been sent.
    pub child: Uuid,
    pub part: SubAgentResultPart,
}

/// Every result a child owes a parent that the parent has not been sent.
///
/// Unconditional: whether the recipient is idle, running or parked is no longer
/// a question this has to answer, because the result goes into a queue rather
/// than into a turn. The agent decides when its queue becomes a turn, and a
/// report deliberately waits out a park rather than overriding it.
///
/// Reads the forest rather than one kind's tree, so it delivers to a workflow
/// step's subagent parents as readily as a conversation's. It used to read an
/// accessor that answered empty for a run, which is why a step's subagent could
/// finish and never be heard from.
#[must_use]
pub fn owed_deliveries(state: &SessionState) -> Vec<AgentAction> {
    state
        .subagents
        .owed()
        .into_iter()
        .map(|owed| {
            let to = match owed.parent {
                SubAgentParent::SubAgent(parent) => AgentKey::Sub(parent),
                // A top-level spawn reports to the agent that roots its own
                // tree — the step that spawned it, or the main agent. Read off
                // the tree rather than off the session's *current* root: a step
                // that has since been superseded is still what asked.
                SubAgentParent::Main => match owed.owner {
                    TreeOwner::Step(agent) => AgentKey::Step(agent),
                    TreeOwner::Main => AgentKey::Main,
                },
            };
            AgentAction::Deliver(Delivery {
                to,
                child: owed.child,
                part: owed.part.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Build a state whose main agent is owed one finished subagent's result.
    fn owing_main(label: &str, output: &str) -> (SessionState, Uuid) {
        let mut s = SessionState::default();
        let id = Uuid::new_v4();
        let tree = s.subagents.tree_mut(TreeOwner::Main);
        tree.apply_spawned(
            id,
            SubAgentParent::Main,
            label.into(),
            "t".into(),
            1,
            100,
            None,
        );
        tree.apply_completed(id, output.into(), 400);
        (s, id)
    }

    #[test]
    fn nothing_is_owed_in_a_fresh_session() {
        assert!(owed_deliveries(&SessionState::default()).is_empty());
    }

    #[test]
    fn a_finished_child_is_owed_to_the_main_agent() {
        let (s, id) = owing_main("audit", "three stale crates");
        let actions = owed_deliveries(&s);
        assert_eq!(actions.len(), 1);
        let AgentAction::Deliver(d) = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(d.to, AgentKey::Main);
        assert_eq!(d.child, id);
        assert_eq!(d.part.text, "three stale crates");
        assert_eq!(d.part.label, "audit");
    }

    /// Nothing here gates on what the recipient is doing. A running or parked
    /// agent has a queue; delivering into it is always safe, and *when* that
    /// queue becomes a turn is the agent's own rule.
    #[test]
    fn a_delivery_does_not_wait_for_the_recipient_to_be_idle() {
        let (mut s, _) = owing_main("audit", "done");
        s.status = crate::sessions::spec::SessionStatus::Running;
        assert_eq!(owed_deliveries(&s).len(), 1);
        s.status = crate::sessions::spec::SessionStatus::AwaitingInput;
        assert_eq!(owed_deliveries(&s).len(), 1);
    }

    #[test]
    fn a_result_already_sent_is_not_owed_again() {
        let (mut s, id) = owing_main("audit", "done");
        s.subagents.tree_mut(TreeOwner::Main).apply_notified(id);
        assert!(owed_deliveries(&s).is_empty());
    }

    #[test]
    fn a_nested_child_is_owed_to_the_subagent_that_spawned_it() {
        let mut s = SessionState::default();
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        {
            let tree = s.subagents.tree_mut(TreeOwner::Main);
            tree.apply_spawned(
                parent,
                SubAgentParent::Main,
                "lead".into(),
                "t".into(),
                1,
                100,
                None,
            );
            tree.apply_completed(parent, "waiting".into(), 200);
            tree.apply_notified(parent);
            tree.apply_spawned(
                child,
                SubAgentParent::SubAgent(parent),
                "helper".into(),
                "t".into(),
                2,
                300,
                None,
            );
            tree.apply_completed(child, "kid done".into(), 600);
        }
        let actions = owed_deliveries(&s);
        assert_eq!(actions.len(), 1);
        let AgentAction::Deliver(d) = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(d.to, AgentKey::Sub(parent));
        assert_eq!(d.part.text, "kid done");
    }
}
