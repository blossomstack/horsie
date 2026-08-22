//! The pure decisions a session makes about work it owns: which agents are owed
//! a finished child's result, and — for each workflow run — which step goes
//! next.
//!
//! Turns are not among them. An agent holds its own queue and decides when
//! that queue becomes a turn, so what used to be `main_turn` is now
//! [`crate::agent_loop::queued_turn`], asked by the agent against its own
//! state. What is left here is delivery: the session owns the forest, so it is
//! the only thing that knows a child's result is owed to a parent, and its job
//! ends at putting that result in the parent's queue.
//!
//! No actors, no I/O, no clock — so this is unit-testable against a hand-built
//! [`SessionState`]. Called from the components that own the work, which is
//! why there is no strategy trait: the actor concatenates what its components
//! return rather than delegating the whole decision to one object.

use crate::sessions::run_forest::{RunId, RunState};
use crate::sessions::session_actor::{AgentKey, SessionState};
use horsie_models::agent::SubAgentResultPart;
use serde_json::Value;
use uuid::Uuid;

/// Something the actor should do. Every field is what the actor needs to
/// journal the action, so the actor never re-derives a decision.
#[derive(Debug, Clone)]
pub enum AgentAction {
    /// Begin one execution of one workflow step.
    StartStep(StepStart),
    /// One run is over and succeeded, carrying the last step's output.
    Finish { run: RunId, output: Value },
    /// One run is over and failed.
    Fail { run: RunId, error: String },
    /// Put a finished child's result — a subagent's, or an invoked run's — in
    /// the queue of the agent that asked for it.
    Deliver(Delivery),
}

/// One execution of one workflow step. Carries everything needed to both spawn
/// the agent and journal the log entry, so the actor never re-derives a
/// decision.
#[derive(Debug, Clone)]
pub struct StepStart {
    /// The run this execution belongs to.
    pub run: RunId,
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

/// One finished child's result, on its way to the agent that is owed it.
#[derive(Debug, Clone)]
pub struct Delivery {
    /// Whose queue it goes in.
    pub to: AgentKey,
    /// The forest entry whose result this is — a subagent's or a workflow
    /// run's — so the session can record that it has now been sent.
    pub child: RunId,
    pub part: SubAgentResultPart,
}

/// Every result a child owes a parent that the parent has not been sent —
/// finished subagents and finished invoked runs, under one rule.
///
/// Unconditional: whether the recipient is idle, running or parked is no longer
/// a question this has to answer, because the result goes into a queue rather
/// than into a turn. The agent decides when its queue becomes a turn, and a
/// report deliberately waits out a park rather than overriding it.
#[must_use]
pub fn owed_deliveries(state: &SessionState) -> Vec<AgentAction> {
    state
        .forest
        .owed()
        .into_iter()
        .filter_map(|owed| {
            // The recipient's key comes off the entry that hosts it, read from
            // the forest rather than from the session's *current* shape: a
            // step that has since been superseded is still what asked.
            let (_, entry) = state.forest.owner_of_agent(owed.to)?;
            let to = match &entry.state {
                RunState::Main(_) => AgentKey::Main,
                RunState::Sub(_) => AgentKey::Sub(owed.to),
                RunState::Workflow(_) => AgentKey::Step(owed.to),
                RunState::SubSession(_) => AgentKey::SubSession(owed.to),
            };
            Some(AgentAction::Deliver(Delivery {
                to,
                child: owed.child,
                part: owed.part,
            }))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Build a state whose main agent is owed one finished subagent's result.
    fn owing_main(label: &str, output: &str) -> (SessionState, Uuid, Uuid) {
        let mut s = SessionState::default();
        let session = Uuid::new_v4();
        s.forest.apply_root_agent(session, 0);
        let id = Uuid::new_v4();
        s.forest
            .apply_sub_spawned(id, session, label.into(), "t".into(), None, 100);
        s.forest.apply_sub_completed(id, output.into(), 400);
        (s, session, id)
    }

    #[test]
    fn nothing_is_owed_in_a_fresh_session() {
        assert!(owed_deliveries(&SessionState::default()).is_empty());
    }

    #[test]
    fn a_finished_child_is_owed_to_the_main_agent() {
        let (s, _session, id) = owing_main("audit", "three stale crates");
        let actions = owed_deliveries(&s);
        assert_eq!(actions.len(), 1);
        let AgentAction::Deliver(d) = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(d.to, AgentKey::Main);
        assert_eq!(d.child, RunId(id));
        assert_eq!(d.part.text, "three stale crates");
        assert_eq!(d.part.title, "audit");
    }

    /// Nothing here gates on what the recipient is doing. A running or parked
    /// agent has a queue; delivering into it is always safe, and *when* that
    /// queue becomes a turn is the agent's own rule.
    #[test]
    fn a_delivery_does_not_wait_for_the_recipient_to_be_idle() {
        let (mut s, session, _) = owing_main("audit", "done");
        s.forest.apply_turn_began(session);
        assert_eq!(owed_deliveries(&s).len(), 1);
        s.forest.apply_asked(session);
        assert_eq!(owed_deliveries(&s).len(), 1);
    }

    #[test]
    fn a_result_already_sent_is_not_owed_again() {
        let (mut s, _session, id) = owing_main("audit", "done");
        s.forest.apply_sub_notified(id);
        assert!(owed_deliveries(&s).is_empty());
    }

    #[test]
    fn a_nested_child_is_owed_to_the_subagent_that_spawned_it() {
        let mut s = SessionState::default();
        let session = Uuid::new_v4();
        s.forest.apply_root_agent(session, 0);
        let parent = Uuid::new_v4();
        let child = Uuid::new_v4();
        s.forest
            .apply_sub_spawned(parent, session, "lead".into(), "t".into(), None, 100);
        s.forest.apply_sub_completed(parent, "waiting".into(), 200);
        s.forest.apply_sub_notified(parent);
        s.forest
            .apply_sub_spawned(child, parent, "helper".into(), "t".into(), None, 300);
        s.forest.apply_sub_completed(child, "kid done".into(), 600);
        let actions = owed_deliveries(&s);
        assert_eq!(actions.len(), 1);
        let AgentAction::Deliver(d) = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(d.to, AgentKey::Sub(parent));
        assert_eq!(d.part.text, "kid done");
    }

    /// A finished run reaches whoever invoked it through the same rule, keyed
    /// as what its invoker *is* — here a step agent, which is what closes the
    /// deliver-to-a-superseded-step question the same way subagents answer it.
    #[test]
    fn a_finished_invoked_run_is_owed_to_its_invoking_step() {
        let mut s = SessionState::default();
        let session = Uuid::new_v4();
        let graph = std::sync::Arc::new(crate::sessions::workflow::WorkflowRunSpec {
            workflow: "review".into(),
            start: "plan".into(),
            steps: vec![],
            input: "go".into(),
            max_steps: 5,
        });
        s.forest
            .apply_root_workflow(session, "review".into(), graph.clone(), 0);
        let step_agent = Uuid::new_v4();
        s.forest.apply_step_started(
            RunId(session),
            "plan".into(),
            step_agent,
            1,
            None,
            None,
            "go".into(),
            100,
        );
        let invoked = RunId(Uuid::new_v4());
        s.forest
            .apply_run_created(invoked, step_agent, "deploy".into(), graph, 200);
        s.forest
            .apply_run_finished(invoked, serde_json::json!({"description": "shipped"}));
        let actions = owed_deliveries(&s);
        assert_eq!(actions.len(), 1);
        let AgentAction::Deliver(d) = &actions[0] else {
            panic!("expected a delivery");
        };
        assert_eq!(d.to, AgentKey::Step(step_agent));
        assert_eq!(d.child, invoked);
        assert_eq!(d.part.title, "workflow deploy");
    }
}
