//! The task-list component.
//!
//! The data model and the pure state transitions live in
//! [`crate::agent_loop::task_list`]. This is the agent-side half: the
//! `task_list` tool call the turn routes here, and the fold.
//!
//! The event carries the full resulting list rather than a delta, which is
//! what lets replay skip re-deriving and re-validating every past mutation.

pub mod domain;

use crate::agent_loop::prelude::*;
use async_trait::async_trait;
use horsie_actor::{ActorRef, CommandEffect};
use horsie_models::now_ms;
use serde_json::Value;

/// The agent's plan, as the model last left it.
///
/// A newtype over the domain state so that "the durable part" and "the list"
/// are the same thing without the domain module having to know it is one.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TaskListPart(crate::agent_loop::components::task_list::domain::TaskListState);

impl TaskListPart {
    pub(crate) fn list(&self) -> &crate::agent_loop::components::task_list::domain::TaskListState {
        &self.0
    }
}

impl CarriedComponentState for TaskListPart {
    /// The plan carries. A sub session branched to continue this work inherits
    /// what the work *is*; losing it would make the branch start over.
    fn carried(&self) -> Option<Self> {
        Some(self.clone())
    }
}

/// The empty list a state with no task-list part answers with.
pub(crate) fn empty_list()
-> &'static crate::agent_loop::components::task_list::domain::TaskListState {
    static EMPTY: std::sync::OnceLock<
        crate::agent_loop::components::task_list::domain::TaskListState,
    > = std::sync::OnceLock::new();
    EMPTY.get_or_init(crate::agent_loop::components::task_list::domain::TaskListState::default)
}

/// Execute the `task_list` tool: the rendered list it answers and the event
/// that records the mutation.
fn execute_task_list_tool(
    folded: &AgentState,
    _name: &str,
    input: &Value,
    _self_ref: ActorRef<AgentCommand>,
) -> Result<(Value, Vec<AgentDomainEvent>), horsie_agentcore::ToolCallError> {
    let action =
        crate::agent_loop::components::task_list::domain::TaskListAction::from_input(input)?;
    let mut next = folded.task_list().clone();
    match next.apply(action) {
        Ok(()) => {
            let text = next.render();
            Ok((
                Value::String(text),
                vec![AgentDomainEvent::TaskListChanged {
                    snapshot: next,
                    at_ms: now_ms(),
                }],
            ))
        }
        Err(msg) => Err(horsie_agentcore::ToolCallError::InvalidInput(msg)),
    }
}

/// The agent's own task list.
pub(crate) struct TaskLists;

#[async_trait]
impl Component for TaskLists {
    type Command = TaskListCommand;

    async fn handle(
        &mut self,
        cmd: TaskListCommand,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let TaskListCommand::ToolCall(call) = cmd;
        answer_tool_call(call, cx, execute_task_list_tool).await
    }

    /// The task-list toolbox: the one `task_list` tool, running on the actor
    /// and answered like any remote tool.
    fn toolbox(
        &self,
        actor: horsie_actor::ActorRef<AgentCommand>,
    ) -> Option<std::sync::Arc<dyn horsie_agentcore::Toolbox>> {
        Some(crate::agent_loop::components::ActorToolbox::new(
            vec![domain::task_list_tool_spec()],
            |call| AgentCommand::TaskList(TaskListCommand::ToolCall(call)),
            actor,
        ))
    }
}

impl TaskLists {
    pub(crate) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        if let AgentDomainEvent::TaskListChanged { snapshot, .. } = event
            && let Some(part) = state.component_state_mut::<TaskListPart>()
        {
            part.0 = snapshot;
        }
    }
}
