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
use horsie_agentcore::{AgentLogBody, LifecycleEvent, TaskListLifecycle};
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

impl PartState for TaskListPart {
    /// The plan carries. A sub session branched to continue this work inherits
    /// what the work *is*; losing it would make the branch start over.
    fn carried(&self) -> Option<Self> {
        Some(self.clone())
    }
}

/// The empty list a state with no task-list part answers with.
pub(crate) fn empty_list() -> &'static crate::agent_loop::components::task_list::domain::TaskListState {
    static EMPTY: std::sync::OnceLock<crate::agent_loop::components::task_list::domain::TaskListState> =
        std::sync::OnceLock::new();
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
    let action = crate::agent_loop::components::task_list::domain::TaskListAction::from_input(input)?;
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
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let TaskListCommand::ToolCall(call) = cmd;
        answer_tool_call(call, cx, execute_task_list_tool).await
    }
}

impl TaskLists {
    /// The list as the mutation left it — folded into the agent's state, and
    /// appended to its log.
    ///
    /// The log entry is not a duplicate of the state: the log is the only thing
    /// a client watches live, so without it a plan changing mid-turn reached
    /// nobody until the next turn boundary let the agent document be re-read.
    // `if let` rather than a `match`, because this module owns exactly one
    // variant. Which one is decided in `component::fold`, so an event added
    // later fails to compile *there* rather than silently reaching the wrong
    // fold here.
    pub(crate) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        if let AgentDomainEvent::TaskListChanged { snapshot, at_ms } = event {
            state.push(
                at_ms,
                AgentLogBody::Lifecycle(LifecycleEvent::TaskList(TaskListLifecycle {
                    tasks: snapshot.wire_tasks(),
                })),
            );
            if let Some(part) = state.part_mut::<TaskListPart>() {
                part.0 = snapshot;
            }
        }
    }
}
