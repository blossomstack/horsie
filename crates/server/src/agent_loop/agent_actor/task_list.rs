//! The `task_list` tool, wired to the actor.
//!
//! The data model and the pure state transitions live in
//! [`crate::agent_loop::task_list`]. This is the tool that reaches them: it
//! executes by `ask`ing the owning [`AgentActor`], so a mutation is journaled
//! like any other durable fact and is never forwarded to the sandboxed runtime.
//!
//! The event carries the full resulting list rather than a delta, which is what
//! lets replay skip re-deriving and re-validating every past mutation.

use super::*;
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect};
use horsie_agentcore::ToolOutcome;
use horsie_agentcore::Toolbox;
use horsie_models::now_ms;
use serde_json::Value;
use std::sync::Arc;

/// Wraps an agent's toolbox, adding the always-available `task_list` tool. It
/// executes by `ask`ing the owning [`AgentActor`] (never forwarded to the
/// sandboxed runtime), so its state is durable -- journaled and replayed
/// exactly like timers (see `crate::agent_loop::task_list`).
pub(super) struct TaskListToolbox {
    pub(super) inner: Arc<dyn Toolbox>,
    pub(super) actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TaskListToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(crate::agent_loop::task_list::task_list_tool_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
        use horsie_agentcore::ToolCallError;
        if name != crate::agent_loop::task_list::TASK_LIST_TOOL {
            return self.inner.execute(name, input, tool_call_id).await;
        }
        let action = crate::agent_loop::task_list::TaskListAction::from_input(&input)?;
        let result = self
            .actor
            .ask(|reply| AgentCommand::TaskList(TaskListCommand::TaskListOp { action, reply }))
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
        result
            .map(|text| ToolOutcome::Result(Value::String(text)))
            .map_err(ToolCallError::InvalidInput)
    }
}

/// The agent's own task list.
pub(super) struct TaskLists;

impl TaskLists {
    pub(super) async fn handle(
        _actor: &mut AgentActor,
        state: &AgentState,
        cmd: TaskListCommand,
        _ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            TaskListCommand::TaskListOp { action, reply } => {
                let mut next = state.task_list.clone();
                match next.apply(action) {
                    Ok(()) => {
                        let text = next.render();
                        let _ = reply.send(Ok(text));
                        CommandEffect::persist(vec![AgentDomainEvent::TaskListChanged {
                            snapshot: next,
                            at_ms: now_ms(),
                        }])
                    }
                    Err(msg) => {
                        let _ = reply.send(Err(msg));
                        CommandEffect::none()
                    }
                }
            }
        }
    }
}

impl Component for TaskLists {
    /// The list as the mutation left it.
    // The fallthrough is unreachable by construction: `AgentActor::apply_event`
    // routes every variant to exactly one module, so an event added later fails
    // to compile *there* — where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::TaskListChanged { snapshot, .. } => state.task_list = snapshot,
            _ => {}
        }
    }
}
