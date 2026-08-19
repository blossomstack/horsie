//! `task_list`: the plan an agent keeps for itself.
//!
//! A scratchpad the model writes and reads back — create a list, insert into
//! it, mark tasks done. The domain model, the parsing and the rendering live in
//! [`crate::agent_loop::task_list`]; what is here is the part that belongs to
//! an agent: the list itself, folded from this agent's own journal.
//!
//! # Why it is a capability now
//!
//! It was a capability all along, hand-rolled: a command on the actor, an arm
//! on the domain event, a field on `AgentState` and a toolbox layer of its own.
//! Four places to edit for one tool, and every one of them a place the next
//! tool would have to be added too. A capability says the same thing once.
//!
//! Nothing about the shape had to change to fit. A `task_list` call answers
//! immediately and never parks, so it is [`Act::Answer`] on success and
//! [`Act::Refuse`] on a rejected action — a refusal rather than a plain result
//! because the model calling `update_status` with an id that does not exist has
//! made a mistake, and `is_error` is what the loop detector reads.
//!
//! # What a compaction must not lose
//!
//! The list is durable, but durable is not the same as the model knowing it is
//! durable: every trace of it in the transcript is a tool call a compaction
//! summarises away. So this implements [`super::Capability::carried_state`] with the
//! same rendering the tool returns — ids and all, because an agent that reads a
//! paraphrase of its own list cannot call `task_list` against it afterwards.

use super::{Act, CapCommand, CapEvent, CapSlice, CapView, Decision, Mailbox, Msg};
use crate::agent_loop::task_list::{
    TaskListAction, TaskListState, TaskRecord, task_list_tool_spec,
};
use crate::agent_loop::toolbox::{ClaimedTool, claiming};
use crate::sessions::runners::loading::AgentFacts;
use horsie_agentcore::Toolbox;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// What the model asked of the list.
///
/// One arm because there is one tool. A second tool would be a second arm,
/// decided by the layer that claimed its name — never by a match on the name
/// itself.
pub enum Command {
    /// `task_list`, with its action still unparsed: a malformed one is a
    /// refusal the model has to see, and deciding that is what `handle` is for.
    Change { input: Value },
}

/// What this capability records: the list, whole, after a mutation.
///
/// A snapshot rather than a delta, which is how the list was journaled before
/// it was a capability and for the same reason: replay never has to re-derive
/// or re-validate a mutation somebody already accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Changed { snapshot: TaskListState },
}

/// One agent's task list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskListCapability {
    list: TaskListState,
}

impl TaskListCapability {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The tasks, in list order — what the agent document shows a reader.
    #[must_use]
    pub fn tasks(&self) -> &[TaskRecord] {
        self.list.tasks()
    }

    /// The model called `task_list`.
    ///
    /// Applied to a clone so the decision is a pure function of what is folded:
    /// on success the clone becomes the event, and on failure nothing was
    /// touched and nothing is journaled.
    fn called(&self, call: &str, input: &Value) -> Decision {
        let action = match TaskListAction::from_input(input) {
            Ok(action) => action,
            // A capability that owns a tool name owns every call to it,
            // including the malformed ones: declining would hand the call to
            // the open-namespace capability behind it, and the model would be
            // answered by the sandbox instead of told what it got wrong.
            Err(reason) => return Decision::refuse(call, reason),
        };
        let mut next = self.list.clone();
        match next.apply(action) {
            Ok(()) => {
                let text = next.render();
                Decision::record(vec![CapEvent::TaskList(Event::Changed { snapshot: next })]).then(
                    Act::Answer {
                        call: call.to_string(),
                        text,
                    },
                )
            }
            Err(reason) => Decision::refuse(call, reason),
        }
    }
}

impl TaskListCapability {
    fn claims(&self) -> Vec<ClaimedTool> {
        vec![ClaimedTool::new(task_list_tool_spec(), |input, to| {
            CapCommand::TaskList(Command::Change { input }, to)
        })]
    }
}

/// The methods the [`Capability`](super::Capability) enum dispatches into.
///
/// Inherent rather than a trait impl: the set of capabilities is closed, so
/// the enum's `match` is what reaches these and nothing else needs to.
impl TaskListCapability {
    pub fn name(&self) -> &'static str {
        "task_list"
    }

    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        _facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        claiming(inner, self.claims(), mailbox)
    }

    pub fn command(&self, cmd: &CapCommand) -> Option<Decision> {
        let CapCommand::TaskList(cmd, to) = cmd else {
            return None;
        };
        let Command::Change { input } = cmd;
        Some(self.called(&to.call, input))
    }

    /// Nothing here is this one's: the list changes only when the model changes
    /// it, so a turn boundary, an answer, a child and a load all leave it
    /// exactly where it was.
    pub fn handle(&self, _msg: &Msg) -> Option<Decision> {
        None
    }

    pub fn apply(&mut self, event: &CapEvent) {
        let CapEvent::TaskList(Event::Changed { snapshot }) = event else {
            return;
        };
        self.list = snapshot.clone();
    }

    /// An empty list carries nothing, so a session that never made one gets no
    /// paragraph saying it has no tasks.
    pub fn carried_state(&self) -> Option<String> {
        (!self.list.tasks().is_empty()).then(|| self.list.render())
    }

    /// The list, as the agent document carries it.
    ///
    /// A copy rather than the state behind it: what a client is shown is a
    /// value computed on request, so nothing outside can hold onto a list that
    /// the next `task_list` call has already moved past.
    pub fn view(&self) -> Option<CapView> {
        Some(CapView::TaskList(self.list.tasks().to_vec()))
    }

    pub fn save(&self) -> CapSlice {
        CapSlice::TaskList(self.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{advertised_by, answering, facts, someone_elses};
    use super::*;
    use crate::agent_loop::capabilities::{Capabilities, Capability, TurnEvent};
    use crate::agent_loop::task_list::TASK_LIST_TOOL;

    fn called(cap: &TaskListCapability, input: serde_json::Value) -> Decision {
        cap.command(&CapCommand::TaskList(
            Command::Change { input },
            answering("t1"),
        ))
        .expect("the task list owns its command")
    }

    /// Fold a decision's events back in, the way the actor does, so a test can
    /// make a second call against what the first one left.
    fn fold(cap: &mut TaskListCapability, decision: &Decision) {
        for event in &decision.events {
            cap.apply(event);
        }
    }

    fn answer(decision: &Decision) -> String {
        for act in &decision.acts {
            if let Act::Answer { text, .. } = act {
                return text.clone();
            }
        }
        panic!("expected an answer, got {:?}", decision.acts);
    }

    fn refusal(decision: &Decision) -> String {
        for act in &decision.acts {
            if let Act::Refuse { reason, .. } = act {
                return reason.clone();
            }
        }
        panic!("expected a refusal, got {:?}", decision.acts);
    }

    #[test]
    fn it_advertises_the_task_list_tool() {
        assert_eq!(
            advertised_by(&Capability::TaskList(TaskListCapability::new()), &facts()),
            vec![TASK_LIST_TOOL]
        );
    }

    /// A successful action journals the whole list and answers with it
    /// rendered, which is what the model reads back.
    #[test]
    fn creating_a_list_journals_it_and_answers_with_it() {
        let mut cap = TaskListCapability::new();
        let decision = called(
            &cap,
            serde_json::json!({"action": "create", "tasks": ["a", "b"]}),
        );
        assert_eq!(decision.events.len(), 1, "one snapshot, not a delta");
        assert!(answer(&decision).contains("[ ] 1. a"));
        fold(&mut cap, &decision);
        assert_eq!(cap.tasks().len(), 2);
        assert_eq!(cap.tasks()[1].content, "b");
    }

    /// The clone the decision was computed against is what gets journaled, so
    /// the folded list and the answer the model saw cannot disagree.
    #[test]
    fn a_status_update_lands_on_the_folded_list() {
        let mut cap = TaskListCapability::new();
        let created = called(
            &cap,
            serde_json::json!({"action": "create", "tasks": ["a", "b"]}),
        );
        fold(&mut cap, &created);
        let updated = called(
            &cap,
            serde_json::json!({"action": "update_status", "ids": [1], "status": "completed"}),
        );
        assert!(answer(&updated).contains("Tasks (1/2 done)"));
        fold(&mut cap, &updated);
        assert!(cap.carried_state().unwrap().contains("Tasks (1/2 done)"));
    }

    /// An action that cannot be applied is an error result, not a plain one:
    /// `is_error` is what agentcore's loop detector reads, and a model
    /// repeating the same bad id is exactly the case it exists for.
    #[test]
    fn an_unknown_id_is_refused_and_journals_nothing() {
        let cap = TaskListCapability::new();
        let decision = called(
            &cap,
            serde_json::json!({"action": "update_status", "ids": [9], "status": "completed"}),
        );
        assert!(decision.events.is_empty(), "a refusal is not a fact");
        assert!(refusal(&decision).contains("unknown task id"));
    }

    /// So is an action that cannot be parsed at all.
    #[test]
    fn an_unreadable_action_is_refused() {
        let cap = TaskListCapability::new();
        let decision = called(&cap, serde_json::json!({"action": "delete_everything"}));
        assert!(decision.events.is_empty());
        assert!(refusal(&decision).contains("unknown action"));
    }

    /// `list` mutates nothing but still journals a snapshot, because the
    /// alternative is a second code path deciding which actions are writes —
    /// and the snapshot it writes is identical to what is already folded.
    #[test]
    fn listing_answers_with_the_current_list() {
        let mut cap = TaskListCapability::new();
        let created = called(
            &cap,
            serde_json::json!({"action": "create", "tasks": ["a"]}),
        );
        fold(&mut cap, &created);
        let listed = called(&cap, serde_json::json!({"action": "list"}));
        assert!(answer(&listed).contains("[ ] 1. a"));
    }

    /// It claims its own command and nothing else — every other message belongs
    /// to some other capability, and claiming a lifecycle one would stop the
    /// offer scan.
    #[test]
    fn it_claims_nothing_but_its_own_command() {
        let cap = TaskListCapability::new();
        assert!(cap.command(&someone_elses()).is_none());
        assert!(cap.handle(&Msg::Loaded).is_none());
        assert!(cap.handle(&Msg::Answer(&[])).is_none());
        for boundary in [
            TurnEvent::Began,
            TurnEvent::Ended,
            TurnEvent::Failed,
            TurnEvent::Cancelled,
        ] {
            assert!(cap.handle(&Msg::Turn(boundary)).is_none());
        }
    }

    /// An empty list is nothing to carry: a session that never made one should
    /// not get a paragraph of boilerplate at every compaction boundary.
    #[test]
    fn an_empty_list_carries_nothing() {
        assert_eq!(TaskListCapability::new().carried_state(), None);
    }

    /// The list has to survive the round trip a reload takes, or an agent comes
    /// back holding a plan it cannot see.
    #[test]
    fn the_list_survives_the_journal_round_trip() {
        let mut cap = TaskListCapability::new();
        let created = called(
            &cap,
            serde_json::json!({"action": "create", "tasks": ["ship it"]}),
        );
        fold(&mut cap, &created);
        let caps = Capabilities::new(vec![Capability::TaskList(cap)]);
        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::TaskList(back) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(back.tasks()[0].content, "ship it");
    }
}
