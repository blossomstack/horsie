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
use horsie_agentcore::{AgentLogBody, LifecycleEvent, TaskListLifecycle};
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
            .map(|text| ToolOutcome::result(Value::String(text)))
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
    /// The list as the mutation left it — folded into the agent's state, and
    /// appended to its log.
    ///
    /// The log entry is not a duplicate of the state: the log is the only thing
    /// a client watches live, so without it a plan changing mid-turn reached
    /// nobody until the next turn boundary let the agent document be re-read.
    /// It carries the whole list, like every other snapshot on this log, so a
    /// reader folds it without needing the ones before it.
    // `if let` rather than a `match`, because this module owns exactly one
    // variant. Which one is decided in `AgentActor::apply_event`, so an event
    // added later fails to compile *there* — where it has to be classified —
    // rather than silently reaching the wrong fold here.
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    #[test]
    fn task_list_events_fold_into_state() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.task_list.render(), "No tasks.");

        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::agent_loop::task_list::TaskListAction::Create {
                tasks: vec!["a".to_string(), "b".to_string()],
            })
            .unwrap();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot },
        );
        assert!(state.task_list.render().contains("[ ] 1. a"));

        // A later snapshot replaces the whole state -- folding is a plain
        // assignment, not a merge.
        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::agent_loop::task_list::TaskListAction::UpdateStatus {
                ids: vec![1],
                status: crate::agent_loop::task_list::TaskStatus::Completed,
            })
            .unwrap();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot },
        );
        assert!(state.task_list.render().contains("Tasks (1/2 done)"));
    }

    /// The plan is a thing a client watches change, and the log is the only
    /// thing it watches. A mutation that folded into state alone was invisible
    /// until the next turn boundary let the agent document be re-read — which
    /// is exactly the mid-turn window a plan is written to be read in.
    #[test]
    fn a_task_list_change_appends_a_lifecycle_entry_carrying_the_whole_list() {
        let mut state = AgentActor::initial_state();
        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::agent_loop::task_list::TaskListAction::Create {
                tasks: vec!["a".to_string(), "b".to_string()],
            })
            .unwrap();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged {
                at_ms: 1_700_000_000_000,
                snapshot,
            },
        );

        let entry = state.log.last().expect("the change appends an entry");
        assert_eq!(entry.at_ms, 1_700_000_000_000, "stamped from the event");
        let AgentLogBody::Lifecycle(LifecycleEvent::TaskList(list)) = &entry.body else {
            panic!("expected a TaskList lifecycle entry, got {:?}", entry.body);
        };
        // The whole list, not a delta: a reader that joined late folds this one
        // entry and has the current plan.
        assert_eq!(list.tasks.len(), 2);
        assert_eq!(list.tasks[0].content, "a");
        assert_eq!(
            list.tasks[0].status,
            horsie_models::agent::TaskStatus::Pending
        );

        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::agent_loop::task_list::TaskListAction::UpdateStatus {
                ids: vec![1],
                status: crate::agent_loop::task_list::TaskStatus::Completed,
            })
            .unwrap();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged { at_ms: 1, snapshot },
        );
        assert_eq!(state.log.len(), 2, "every mutation is its own entry");
        let AgentLogBody::Lifecycle(LifecycleEvent::TaskList(list)) =
            &state.log.last().unwrap().body
        else {
            panic!("expected a TaskList lifecycle entry");
        };
        assert_eq!(
            list.tasks[0].status,
            horsie_models::agent::TaskStatus::Completed
        );
    }
}
