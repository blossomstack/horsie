//! The actor-owned run loop.
//!
//! [`RunLoop`] holds the two stateful tools. `machine.rs` shows the complete
//! command-to-transition path; sibling modules execute one kind of step.
mod compaction;
mod context;
mod incoming;
mod machine;
mod provider;
mod reads;
mod seed;

use crate::agent_loop::components::{TaskLists, Timers};
use crate::agent_loop::prelude::*;

pub use incoming::{AnswerError, AskAnswer, Incoming};
pub(crate) use incoming::{PendingInput, TurnInput, drain, messages, next_input, validate_answers};
pub use reads::{ReadOutcome, ReplayWindow};

/// The actor's run-loop driver. It owns only genuine stateful tool
/// components; all other work is expressed by history plus [`StepRun`].
pub(crate) struct RunLoop {
    pub(crate) timers: Timers,
    pub(crate) task_lists: TaskLists,
}

impl RunLoop {
    pub fn new() -> Self {
        Self {
            timers: Timers,
            task_lists: TaskLists,
        }
    }

    /// Toolboxes vended by the two genuine stateful components. Provisioning
    /// composes them ahead of runtime tools, so their names win collisions.
    pub(crate) fn toolboxes(
        &self,
        actor: horsie_actor::ActorRef<AgentCommand>,
    ) -> Vec<std::sync::Arc<dyn horsie_agentcore::Toolbox>> {
        [
            self.timers.toolbox(actor.clone()),
            self.task_lists.toolbox(actor),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Restore process-local work owned by stateful tools. The actor advances
    /// once after all recovery work has been scheduled.
    pub async fn on_load(&mut self, cx: &mut CommandContext<'_>) {
        self.timers.on_load(cx).await;
    }
}

impl RunLoop {
    /// Fold one event through an exhaustive owner map. Live handling, replay,
    /// and fold-forward use this same pure function, so they cannot disagree.
    pub fn apply(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
        // `Seeded` carries a whole `AgentState`; storing that event inside the
        // state it installs would recursively duplicate the source snapshot.
        // The adopted history is already the durable account.
        let history_record =
            (!matches!(&event, AgentDomainEvent::Seeded { .. })).then(|| event.clone());
        match event {
            e @ (AgentDomainEvent::Seeded { .. } | AgentDomainEvent::SeedSummaryTaken { .. }) => {
                seed::apply(&mut state, e)
            }
            e @ AgentDomainEvent::MessageComplete { .. } => provider::apply(&mut state, e),
            e @ AgentDomainEvent::Compacted { .. } => compaction::apply(&mut state, e),
            e @ (AgentDomainEvent::TimerArmed { .. }
            | AgentDomainEvent::TimerCancelled { .. }
            | AgentDomainEvent::TimerFired { .. }) => Timers::apply(&mut state, e),
            e @ AgentDomainEvent::TaskListChanged { .. } => TaskLists::apply(&mut state, e),
            AgentDomainEvent::SystemPromptRecorded { .. }
            | AgentDomainEvent::AgentInitialized { .. }
            | AgentDomainEvent::ConnectionCompleted
            | AgentDomainEvent::StepStarted { .. }
            | AgentDomainEvent::StepFailed { .. }
            | AgentDomainEvent::StopHookCompleted { .. }
            | AgentDomainEvent::RunEnded { .. }
            | AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::Consumed { .. }
            | AgentDomainEvent::MessageAborted { .. }
            | AgentDomainEvent::ToolComplete { .. }
            | AgentDomainEvent::HookRan { .. }
            | AgentDomainEvent::TurnCompleted { .. }
            | AgentDomainEvent::TurnAborted { .. }
            | AgentDomainEvent::TurnCancelled { .. }
            | AgentDomainEvent::Parked { .. }
            | AgentDomainEvent::Nudged { .. }
            | AgentDomainEvent::LifecycleRecorded { .. }
            | AgentDomainEvent::Received { .. }
            | AgentDomainEvent::TurnBegan { .. }
            | AgentDomainEvent::AskRecorded { .. } => {}
        }
        if let Some(history_record) = history_record {
            state.record_history(history_record);
        }
        state
    }

    /// Fold several events forward over a snapshot — what a handler does to
    /// see the state its own events leave behind before deciding what comes
    /// next.
    pub fn apply_all(state: &AgentState, events: &[AgentDomainEvent]) -> AgentState {
        events.iter().cloned().fold(state.clone(), Self::apply)
    }
}
