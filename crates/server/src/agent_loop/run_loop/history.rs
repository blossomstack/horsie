//! Session lifecycle and plugin-hook facts written into agent history.
//!
//! The agent remains the sole writer, so transcript projection later preserves
//! their exact order without merging another stream. Runtime lifecycle records
//! also update the process-local readiness gate.

use crate::agent_loop::prelude::*;
use horsie_actor::CommandEffect;
use horsie_agentcore::LifecycleEvent;
use horsie_models::now_ms;

/// What a lifecycle record says about this agent's runtime, if anything.
///
/// Exhaustive on purpose: a variant added later has to state whether it bears
/// on whether this agent may run, rather than silently answering "no".
pub(crate) fn runtime_readiness(event: &LifecycleEvent) -> Option<bool> {
    match event {
        LifecycleEvent::Runtime(runtime) => Some(match runtime.status {
            horsie_agentcore::RuntimeStatus::Ready(_) => true,
            horsie_agentcore::RuntimeStatus::Acquiring(_)
            | horsie_agentcore::RuntimeStatus::Failed(_) => false,
        }),
        // Terminal: the runtime is gone for good and no later message brings it
        // back, so this agent must not start another turn.
        LifecycleEvent::SessionFailed(_) => Some(false),
        LifecycleEvent::Preparing(_)
        | LifecycleEvent::MessageQueued(_)
        | LifecycleEvent::TurnBegan(_)
        | LifecycleEvent::TurnEnded(_)
        | LifecycleEvent::AskRecorded(_)
        | LifecycleEvent::SubAgent(_)
        // A sub session branching off says nothing about *this* agent's
        // runtime: they share the session's, and it was already up for the sub
        // session to have been taken at all.
        | LifecycleEvent::SubSession(_)
        // A compaction declining to fold anything is an answer to a typed
        // command. It touches neither the runtime nor the history.
        | LifecycleEvent::CompactionSkipped(_)
        | LifecycleEvent::Step(_)
        | LifecycleEvent::TaskList(_) => None,
    }
}

/// Things written into this agent's log by somebody else.
pub(crate) struct HistoryHandler;

impl HistoryHandler {
    pub(crate) async fn handle(
        cmd: HistoryCommand,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            HistoryCommand::RecordLifecycle { event, at_ms } => {
                // Almost every one of these is something a reader sees and this
                // agent does nothing about. The runtime arriving is the one
                // that changes what it may *do* — so it is read off the record
                // rather than announced separately, and a record that says
                // nothing about the runtime cannot start a turn. That is what
                // keeps recovery quiet: it journals a `TurnEnded(Interrupted)`,
                // which is not a runtime fact and drains nothing.
                let moved =
                    runtime_readiness(&event).filter(|next| *next != cx.step_run.runtime_ready);
                if let Some(next) = moved {
                    cx.step_run.runtime_ready = next;
                }
                // The runtime arriving is what lets a waiting agent start
                // work. Nothing is told: the advance that follows this write
                // finds the record folded and the gate open.
                CommandEffect::persist(vec![AgentDomainEvent::LifecycleRecorded { event, at_ms }])
            }
            HistoryCommand::HooksRan { records } => {
                let at_ms = now_ms();
                // Counted here, against the state as it stands, and carried on
                // the event: `agent_frame` sees only the event, so deriving the
                // id at fold time would give the live stream different cursors
                // than `/history`.
                let mut seq = cx.state.hook_entry_count();
                let events = records
                    .into_iter()
                    .map(|record| {
                        let event = AgentDomainEvent::HookRan { record, seq, at_ms };
                        seq += 1;
                        event
                    })
                    .collect();
                CommandEffect::persist(events)
            }
        }
    }
}
