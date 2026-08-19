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
//! immediately and never parks, so it is a plain result on success and a tool
//! *error* on a rejected action — an error rather than a plain result because
//! the model calling `update_status` with an id that does not exist has made a
//! mistake, and `is_error` is what the loop detector reads.
//!
//! # What a compaction must not lose
//!
//! The list is durable, but durable is not the same as the model knowing it is
//! durable: every trace of it in the transcript is a tool call a compaction
//! summarises away. So this implements [`super::Capability::carried_state`] with the
//! same rendering the tool returns — ids and all, because an agent that reads a
//! paraphrase of its own list cannot call `task_list` against it afterwards.

use crate::agent_loop::AgentCommand;
use crate::agent_loop::task_list::{TaskListAction, TaskListState, task_list_tool_spec};
use crate::agent_loop::toolbox::ClaimedTool;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The permission to keep a list, and nothing more.
///
/// The list itself is [`TaskListState`] on
/// [`AgentState`](crate::agent_loop::AgentState). No wrapper around it: its
/// fields were already private to `agent_loop::task_list`, so a second type
/// would encapsulate nothing the first does not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskListCapability;

impl TaskListCapability {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// An empty list carries nothing across a compaction, so a session that
    /// never made one gets no paragraph saying it has no tasks.
    ///
    /// Here rather than beside the type, because *what a compaction must not
    /// lose* is a question about this capability rather than about the data:
    /// the list is durable either way, and this is the part the model would
    /// otherwise stop knowing about.
    #[must_use]
    pub fn carried_state(list: &TaskListState) -> Option<String> {
        (!list.tasks().is_empty()).then(|| list.render())
    }

    /// The model called `task_list`.
    ///
    /// Applied to a clone so the decision is a pure function of what is folded:
    /// on success the clone becomes the snapshot the actor journals, and on
    /// failure nothing was touched and nothing is journaled.
    #[must_use]
    pub(crate) fn changed(list: &TaskListState, input: &Value) -> Changed {
        let action = match TaskListAction::from_input(input) {
            Ok(action) => action,
            // A capability that owns a tool name owns every call to it,
            // including the malformed ones: declining would hand the call to
            // the open-namespace capability behind it, and the model would be
            // answered by the sandbox instead of told what it got wrong.
            Err(reason) => return Changed::Refused(reason),
        };
        let mut next = list.clone();
        match next.apply(action) {
            Ok(()) => Changed::Changed {
                told: next.render(),
                snapshot: next,
            },
            Err(reason) => Changed::Refused(reason),
        }
    }
}

/// What a call to `task_list` came to.
///
/// The snapshot is the whole list rather than a delta, which is how it was
/// journaled before it was a capability and for the same reason: replay never
/// has to re-derive or re-validate a mutation somebody already accepted.
///
/// Two arms and not three: `list` mutates nothing but still produces a
/// snapshot, because the alternative is a second code path deciding which
/// actions are writes — and the snapshot it produces is identical to what is
/// already folded.
pub(crate) enum Changed {
    /// The model gets a tool *error*, which is what `is_error` on a rejected
    /// action means. Journals nothing: nothing was changed.
    Refused(String),
    /// The list, whole, as it now stands, plus what the model reads back.
    Changed {
        snapshot: TaskListState,
        told: String,
    },
}

impl TaskListCapability {
    pub(crate) fn claims(&self) -> Vec<ClaimedTool> {
        vec![ClaimedTool::new(task_list_tool_spec(), |input, to| {
            AgentCommand::TaskListChange {
                input,
                answering: to,
            }
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
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::super::testing::{advertised_by, facts};
    use super::*;
    use crate::agent_loop::capabilities::Capability;
    use crate::agent_loop::state::AgentDomainEvent;
    use crate::agent_loop::task_list::TASK_LIST_TOOL;

    /// The list this agent is holding, with nothing on it.
    fn list() -> TaskListState {
        TaskListState::default()
    }

    /// Journal what was decided. A decision has not yet changed anything; this
    /// is the step that makes it true, through the same fold the actor uses.
    fn fold(list: TaskListState, snapshot: TaskListState) -> TaskListState {
        crate::agent_loop::AgentState {
            task_list: list,
            ..crate::agent_loop::AgentState::default()
        }
        .apply(AgentDomainEvent::TaskListChanged { snapshot })
        .task_list
    }

    fn answer(changed: Changed) -> String {
        match changed {
            Changed::Changed { told, .. } => told,
            Changed::Refused(reason) => panic!("expected an answer, got a refusal: {reason}"),
        }
    }

    fn refusal(changed: Changed) -> String {
        match changed {
            Changed::Refused(reason) => reason,
            Changed::Changed { told, .. } => panic!("expected a refusal, got an answer: {told}"),
        }
    }

    #[test]
    fn it_advertises_the_task_list_tool() {
        assert_eq!(
            advertised_by(&Capability::TaskList(TaskListCapability::new()), &facts()),
            vec![TASK_LIST_TOOL]
        );
    }

    /// A successful action produces the whole list to journal and answers with
    /// it rendered, which is what the model reads back.
    #[test]
    fn creating_a_list_journals_it_and_answers_with_it() {
        let changed = TaskListCapability::changed(
            &list(),
            &serde_json::json!({"action": "create", "tasks": ["a", "b"]}),
        );
        let Changed::Changed { snapshot, told } = changed else {
            panic!("the create was refused");
        };
        assert!(told.contains("[ ] 1. a"));
        let folded = fold(list(), snapshot);
        assert_eq!(folded.tasks().len(), 2, "one snapshot, not a delta");
        assert_eq!(folded.tasks()[1].content, "b");
    }

    /// The clone the decision was computed against is what gets journaled, so
    /// the folded list and the answer the model saw cannot disagree.
    #[test]
    fn a_status_update_lands_on_the_folded_list() {
        let created = TaskListCapability::changed(
            &list(),
            &serde_json::json!({"action": "create", "tasks": ["a", "b"]}),
        );
        let Changed::Changed { snapshot, .. } = created else {
            panic!("the create was refused");
        };
        let list = fold(list(), snapshot);
        let updated = TaskListCapability::changed(
            &list,
            &serde_json::json!({"action": "update_status", "ids": [1], "status": "completed"}),
        );
        let Changed::Changed { snapshot, told } = updated else {
            panic!("the update was refused");
        };
        assert!(told.contains("Tasks (1/2 done)"));
        let list = fold(list, snapshot);
        assert!(
            TaskListCapability::carried_state(&list)
                .unwrap()
                .contains("Tasks (1/2 done)")
        );
    }

    /// An action that cannot be applied is an error result, not a plain one:
    /// `is_error` is what agentcore's loop detector reads, and a model
    /// repeating the same bad id is exactly the case it exists for.
    #[test]
    fn an_unknown_id_is_refused_and_journals_nothing() {
        let changed = TaskListCapability::changed(
            &list(),
            &serde_json::json!({"action": "update_status", "ids": [9], "status": "completed"}),
        );
        assert!(refusal(changed).contains("unknown task id"));
    }

    /// So is an action that cannot be parsed at all.
    #[test]
    fn an_unreadable_action_is_refused() {
        let changed = TaskListCapability::changed(
            &list(),
            &serde_json::json!({"action": "delete_everything"}),
        );
        assert!(refusal(changed).contains("unknown action"));
    }

    /// `list` mutates nothing but still produces a snapshot, because the
    /// alternative is a second code path deciding which actions are writes —
    /// and the snapshot it produces is identical to what is already folded.
    #[test]
    fn listing_answers_with_the_current_list() {
        let created = TaskListCapability::changed(
            &list(),
            &serde_json::json!({"action": "create", "tasks": ["a"]}),
        );
        let Changed::Changed { snapshot, .. } = created else {
            panic!("the create was refused");
        };
        let list = fold(list(), snapshot);
        assert!(
            answer(TaskListCapability::changed(
                &list,
                &serde_json::json!({"action": "list"})
            ))
            .contains("[ ] 1. a")
        );
    }

    /// An empty list is nothing to carry: a session that never made one should
    /// not get a paragraph of boilerplate at every compaction boundary.
    #[test]
    fn an_empty_list_carries_nothing() {
        assert_eq!(TaskListCapability::carried_state(&list()), None);
    }

    /// The list has to survive the round trip a reload takes, or an agent comes
    /// back holding a plan it cannot see.
    #[test]
    fn the_list_survives_the_journal_round_trip() {
        let created = TaskListCapability::changed(
            &list(),
            &serde_json::json!({"action": "create", "tasks": ["ship it"]}),
        );
        let Changed::Changed { snapshot, .. } = created else {
            panic!("the create was refused");
        };
        let state = crate::agent_loop::AgentState {
            task_list: fold(list(), snapshot),
            ..crate::agent_loop::AgentState::default()
        };
        let written = serde_json::to_string(&state).expect("write");
        let back: crate::agent_loop::AgentState = serde_json::from_str(&written).expect("read");
        assert_eq!(
            back.task_list.tasks()[0].content,
            "ship it",
            "a reload that lost the list leaves the agent holding a plan it cannot see"
        );
    }
}
