//! Things written into this agent's log by somebody else.
//!
//! A session lifecycle record, a plugin hook's audit trail, a chunk of the
//! message being streamed. None of them is the agent's own decision, and all
//! three are journaled — or, for a delta, ordered — *here* because the agent is
//! the sole writer of its own log. That is what makes the order deterministic
//! with no merge anywhere, and it is why a client reads one ordered thing
//! instead of reconciling two streams.
//!
//! One of these records does change what the agent may *do*: the runtime
//! arriving. It is read off the record by [`runtime_readiness`] rather than
//! announced separately, so a record that says nothing about the runtime cannot
//! start a turn — which is what keeps recovery quiet.

use super::*;
use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor};
use horsie_agentcore::{AgentLogBody, LifecycleEvent};
use horsie_models::now_ms;

/// What a lifecycle record says about this agent's runtime, if anything.
///
/// Exhaustive on purpose: a variant added later has to state whether it bears
/// on whether this agent may run, rather than silently answering "no".
pub(super) fn runtime_readiness(event: &LifecycleEvent) -> Option<bool> {
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
        // A fork branching off says nothing about *this* agent's runtime:
        // they share the session's, and it was already up for the fork to
        // have been taken at all.
        | LifecycleEvent::Forked(_)
        // A compaction declining to fold anything is an answer to a typed
        // command. It touches neither the runtime nor the history.
        | LifecycleEvent::CompactionSkipped(_)
        | LifecycleEvent::Step(_)
        | LifecycleEvent::TaskList(_) => None,
    }
}

/// Things written into this agent's log by somebody else.
pub(super) struct LogWrites;

impl LogWrites {
    pub(super) async fn handle(
        actor: &mut AgentActor,
        state: &AgentState,
        cmd: LogCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            LogCommand::RecordLifecycle { event, at_ms } => {
                // Almost every one of these is something a reader sees and this
                // agent does nothing about. The runtime arriving is the one
                // that changes what it may *do* — so it is read off the record
                // rather than announced separately, and a record that says
                // nothing about the runtime cannot start a turn. That is what
                // keeps recovery quiet: it journals a `TurnEnded(Interrupted)`,
                // which is not a runtime fact and drains nothing.
                let moved = runtime_readiness(&event).filter(|next| *next != actor.ready);
                if let Some(next) = moved {
                    actor.ready = next;
                }
                let recorded = AgentDomainEvent::LifecycleRecorded { event, at_ms };
                if moved != Some(true) {
                    return CommandEffect::persist(vec![recorded]);
                }
                let folded = AgentActor::apply_event(state.clone(), recorded.clone());
                let mut events = vec![recorded];
                events.extend(actor.try_drain(&folded, ctx).await);
                CommandEffect::persist(events)
            }
            LogCommand::RecordDelta { text } => {
                actor.deltas.push(text);
                actor.publish_revision();
                CommandEffect::none()
            }
            LogCommand::HooksRan { records } => {
                let at_ms = now_ms();
                // Counted here, against the state as it stands, and carried on
                // the event: `agent_frame` sees only the event, so deriving the
                // id at fold time would give the live stream different cursors
                // than `/history`.
                let mut seq = state.hook_entry_count();
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

impl Component for LogWrites {
    /// What the session did, and what a plugin did to a tool call.
    // The fallthrough is unreachable by construction: `AgentActor::apply_event`
    // routes every variant to exactly one module, so an event added later fails
    // to compile *there* — where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::LifecycleRecorded { event, at_ms } => {
                state.push(at_ms, AgentLogBody::Lifecycle(event));
            }
            AgentDomainEvent::HookRan { record, seq, at_ms } => {
                state.push(at_ms, AgentLogBody::Hook(hook_entry(record, seq, at_ms)));
            }
            _ => {}
        }
    }
}
