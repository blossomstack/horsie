//! The `task_list` tool, decided on the mailbox.
//!
//! The data model and the pure state transitions live in
//! [`crate::agent_loop::task_list`]. This is the agent-side half: the inline
//! executor the turn routes to, and the fold. An apply-only module — it owns
//! an event but no command, so it is not a component the actor holds.
//!
//! The event carries the full resulting list rather than a delta, which is
//! what lets replay skip re-deriving and re-validating every past mutation.

use super::*;
use horsie_agentcore::{AgentLogBody, LifecycleEvent, TaskListLifecycle};
use horsie_models::now_ms;
use serde_json::Value;

/// Execute the `task_list` tool: the rendered list it answers and the event
/// that records the mutation. A free function over the folded state — no
/// toolbox wrapper, no ask round-trip, no component instance.
pub(super) fn execute_task_list_tool(
    folded: &AgentState,
    input: &Value,
) -> Result<(Value, Vec<AgentDomainEvent>), horsie_agentcore::ToolCallError> {
    let action = crate::agent_loop::task_list::TaskListAction::from_input(input)?;
    let mut next = folded.task_list.clone();
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

/// The agent's own task list — the fold's owner for its one event.
pub(super) struct TaskLists;

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
    pub(super) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        if let AgentDomainEvent::TaskListChanged { snapshot, at_ms } = event {
            state.push(
                at_ms,
                AgentLogBody::Lifecycle(LifecycleEvent::TaskList(TaskListLifecycle {
                    tasks: snapshot.wire_tasks(),
                })),
            );
            state.task_list = snapshot;
        }
    }
}
