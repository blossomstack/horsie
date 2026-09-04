//! The actor-owned run loop.
//!
//! [`RunLoop`] contains only the two stateful tool components. The modules
//! beside it are stateless handlers over shared history and [`StepRun`].
mod compaction_step;
mod context_step;
mod decision;
mod history;
mod incoming;
mod provider;
mod queries;
mod seed_step;

use crate::agent_loop::components::{TaskLists, Timers};
use crate::agent_loop::prelude::*;
use horsie_actor::CommandEffect;

use compaction_step::CompactionStep;
use context_step::ContextStep;
use history::HistoryHandler;
use incoming::IncomingHandler;
use provider::ProviderStep;
use queries::QueryHandler;
use seed_step::SeedStep;

pub use incoming::{
    ABANDONED_ASK_RESULT, AnswerError, AskAnswer, Incoming, MERGE_SEPARATOR, PendingInput,
    TurnInput, answered_input, next_input,
};
pub use queries::{ReadOutcome, ReplayWindow};

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

    /// Route one command to its handler. Adding a command group must be
    /// classified here.
    ///
    /// `Advance` and `Cancel` stay here because they are decisions of the loop
    /// itself, not commands owned by a component.
    pub async fn handle(
        &mut self,
        cmd: AgentCommand,
        cx: &mut CommandContext<'_>,
    ) -> Option<CommandEffect<AgentDomainEvent>> {
        Some(match cmd {
            AgentCommand::Incoming(c) => IncomingHandler::handle(c, cx).await,
            AgentCommand::Provider(c) => ProviderStep::handle(c, cx).await,
            AgentCommand::Timer(c) => self.timers.handle(c, cx).await,
            AgentCommand::Query(c) => QueryHandler::handle(c, cx).await,
            AgentCommand::History(c) => HistoryHandler::handle(c, cx).await,
            AgentCommand::Seed(c) => SeedStep::handle(c, cx).await,
            AgentCommand::TaskList(c) => self.task_lists.handle(c, cx).await,
            AgentCommand::Context(c) => ContextStep::handle(c, cx).await,
            AgentCommand::Compaction(c) => CompactionStep::handle(c, cx).await,
            AgentCommand::Core(CoreCommand::StopHookReturned { marker_seq, result }) => {
                self.stop_hook_returned(marker_seq, result, cx)
            }
            AgentCommand::Core(CoreCommand::ToolReturned {
                marker_seq,
                tool_call_id,
                outcome,
            }) => {
                self.tool_returned(marker_seq, tool_call_id, outcome, cx)
                    .await
            }
            AgentCommand::Core(CoreCommand::Advance) => self.advance(cx).await,
            AgentCommand::Core(CoreCommand::Cancel { ack }) => self.cancel(ack, cx).await,
            AgentCommand::Core(CoreCommand::Shutdown) => return None,
        })
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
                SeedStep::apply(&mut state, e)
            }
            e @ AgentDomainEvent::MessageComplete { .. } => ProviderStep::apply(&mut state, e),
            e @ AgentDomainEvent::Compacted { .. } => CompactionStep::apply(&mut state, e),
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
