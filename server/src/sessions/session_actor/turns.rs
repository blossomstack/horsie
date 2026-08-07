//! The conversation: what a person sends, and how a turn ends.
//!
//! A user message is made durable *before* anything is done with it, so an
//! accepted message survives a crash and is still owed an answer. Queued
//! messages merge into one turn at the next boundary, because Anthropic requires
//! alternating roles and consecutive user turns are not portable.
//!
//! Silent when `state.run` is set: a run works from its definition and there is
//! nobody to send it a message.

use super::InboxMessage;
use super::LifecycleCommand;
use super::component::{ActionCx, Component};
use super::{
    AgentAction, CommandEffect, SessionActor, SessionCommand, SessionDomainEvent, SessionState,
    TurnCommand,
};
use super::{AnswerError, AskAnswer, derive_title};
use crate::sessions::UserMessageError;
use crate::sessions::spec::PendingAsk;
use crate::sessions::spec::SessionStatus;
use horsie_actor::{ActorContext, EventSourcedActor};
use horsie_models::agent::ToolResultInput;
use horsie_models::now_ms;
use horsie_workflow::AgentCommand;
use std::collections::HashSet;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Turns.
pub(super) struct Turns;

impl Turns {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: TurnCommand,
        ctx: &ActorContext<SessionActor>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            TurnCommand::UserMessage { text, reply } => {
                // A run works from its definition; there is nobody to send it
                // a message. Read off the spec rather than off `state.run`: a
                // run that has not started its first step has no run state, and
                // would otherwise accept a message it can never answer.
                if actor.spec.workflow.is_some() {
                    let _ = reply.send(Err(UserMessageError::Rejected(
                        "this session is a workflow run; it takes no messages".to_string(),
                    )));
                    return CommandEffect::none();
                }
                actor.on_user_message(state, text, reply, ctx).await
            }
            TurnCommand::Stop { reply } => {
                if state.status != SessionStatus::Running {
                    let _ = reply.send(());
                    return CommandEffect::none();
                }
                actor.cancel_run().await;
                let _ = reply.send(());
                actor.report(SessionStatus::Idle).await;
                let mut events = vec![SessionDomainEvent::TurnStopped { at_ms: now_ms() }];
                // Stop is a turn boundary like any other, so anything the user
                // queued while the cancelled turn ran starts the next one.
                let next = SessionActor::apply_event(
                    state.clone(),
                    SessionDomainEvent::TurnStopped { at_ms: now_ms() },
                );
                events.extend(actor.flush_then_drain(&next, ctx).await);
                CommandEffect::persist(events)
            }
            TurnCommand::Answer { answers, reply } => actor.on_answer(state, answers, reply).await,
            TurnCommand::ReconcileInterrupted => {
                if state.status == SessionStatus::Running {
                    actor.report(SessionStatus::Idle).await;
                    CommandEffect::persist(vec![SessionDomainEvent::TurnInterrupted {
                        at_ms: now_ms(),
                    }])
                } else {
                    CommandEffect::none()
                }
            }
        }
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// Answer every pending ask at once and resume the turn. A set that does not
    /// cover the pending asks exactly is refused and nothing is journaled: a
    /// half-answered park would leave the run unable to resume and the wire
    /// holding a `tool_use` with no result.
    pub(super) async fn on_answer(
        &mut self,
        state: &SessionState,
        answers: Vec<AskAnswer>,
        reply: oneshot::Sender<Result<(), AnswerError>>,
    ) -> CommandEffect<SessionDomainEvent> {
        let pending: HashSet<String> = state
            .pending_asks
            .iter()
            .filter_map(|a| a.tool_call_id.clone())
            .collect();
        if pending.is_empty() {
            let _ = reply.send(Err(AnswerError::NothingPending));
            return CommandEffect::none();
        }
        let answered: HashSet<String> = answers.iter().map(|a| a.tool_call_id.clone()).collect();
        if answered != pending {
            let mut missing: Vec<String> = pending.difference(&answered).cloned().collect();
            let mut unexpected: Vec<String> = answered.difference(&pending).cloned().collect();
            missing.sort();
            unexpected.sort();
            let _ = reply.send(Err(AnswerError::Incomplete {
                missing,
                unexpected,
            }));
            return CommandEffect::none();
        }

        let results: Vec<ToolResultInput> = answers
            .iter()
            .map(|a| ToolResultInput {
                tool_call_id: a.tool_call_id.clone(),
                output: a.text.clone(),
                is_error: false,
            })
            .collect();
        if let Some(agent) = self.agent() {
            let _ = agent
                .tell(AgentCommand::Resume {
                    results,
                    message: None,
                    subagent_results: Vec::new(),
                })
                .await;
        }
        self.report(SessionStatus::Running).await;
        let _ = reply.send(Ok(()));
        CommandEffect::persist(vec![SessionDomainEvent::TurnBegan {
            at_ms: now_ms(),
            consumed: Vec::new(),
            answering: None,
            answered: answers.into_iter().map(|a| a.tool_call_id).collect(),
        }])
    }
    pub(super) async fn on_user_message(
        &mut self,
        state: &SessionState,
        text: String,
        reply: oneshot::Sender<Result<String, UserMessageError>>,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let SessionStatus::Unrecoverable { reason } = &state.status {
            let _ = reply.send(Err(UserMessageError::Unrecoverable(reason.clone())));
            return CommandEffect::none();
        }
        // An unnamed session is titled from its first message, once.
        if self.spec.name.is_none()
            && let Some(title) = derive_title(&text)
            && let Err(error) = self.rename_session(title).await
        {
            tracing::warn!(session = %self.id, error, "failed to persist fallback session title");
        }

        let queued = SessionDomainEvent::MessageQueued {
            id: Uuid::new_v4().to_string(),
            text,
            at_ms: now_ms(),
        };
        let SessionDomainEvent::MessageQueued { id, .. } = &queued else {
            unreachable!("just constructed")
        };
        let message_id = id.clone();
        let _ = reply.send(Ok(message_id));

        // Fold the queue locally so the drain sees the message it is about to
        // persist — same fold the runtime will apply, just one step early.
        let next = SessionActor::apply_event(state.clone(), queued.clone());
        let mut events = vec![queued];
        // A session whose create failed has no runtime, so the message that the
        // UI invited ("send a message to try again") has to build one rather
        // than start a turn that would ask for it. The message stays queued and
        // the create's own completion drains it, exactly as at session creation.
        if matches!(next.status, SessionStatus::ProvisioningFailed { .. }) {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
                .await;
        } else {
            events.extend(self.flush_then_drain(&next, ctx).await);
        }
        CommandEffect::persist(events)
    }
}

impl Component for Turns {
    /// The main agent's turn, if one is owed. Silent in a run: a run works from
    /// its definition and there is nobody to have typed anything.
    ///
    /// Keyed off the *spec*, not off `state.run`. A run that has not folded a
    /// `StepStarted` yet has no run state at all, so reading the state would
    /// make a just-created run look like a conversation and hand it a main
    /// agent it does not have.
    fn actions(cx: &ActionCx<'_>, state: &SessionState) -> Vec<AgentAction> {
        if cx.spec.workflow.is_some() {
            return Vec::new();
        }
        crate::sessions::orchestrator::main_turn(state)
            .into_iter()
            .collect()
    }

    /// A turn the process died inside is over; recovery records that.
    fn on_load(_cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        (state.status == SessionStatus::Running)
            .then_some(SessionCommand::Turn(TurnCommand::ReconcileInterrupted))
    }

    /// A turn in flight. `WorkflowRun` answers for a step, so this is only ever
    /// asked about a conversation — but `status` is shared, so the check is the
    /// same either way and double-counting is harmless.
    fn busy(state: &SessionState) -> bool {
        matches!(state.status, SessionStatus::Running)
    }

    /// Everything a conversation records. `status` moves here too: a turn
    /// beginning, ending, failing or being interrupted is the session's own
    /// state as much as the turn's.
    ///
    /// Pure, and an associated function rather than a method: replay runs with
    /// no instance in scope, which is what makes a recovered session and a live
    /// one follow the same path.
    // The fallthrough is unreachable by construction: `SessionActor::apply_event`
    // matches every variant explicitly and routes each to exactly one component,
    // so a newly added event fails to compile *there* — which is where it should
    // be classified — rather than silently reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::MessageQueued { id, text, at_ms } => {
                state.inbox.push(InboxMessage { id, text, at_ms });
            }
            SessionDomainEvent::TurnBegan { consumed, .. } => {
                state.status = SessionStatus::Running;
                state.inbox.retain(|m| !consumed.contains(&m.id));
                // A turn beginning ends the park either way: the asks were
                // answered, or the user moved on and they were abandoned. Both
                // record a result for every call before the turn starts.
                state.pending_asks.clear();
                // The previous turn's failure is history once a new turn is
                // under way; leaving it set makes the detail endpoint report a
                // stale error for the rest of the session's life.
                state.last_error = None;
            }
            SessionDomainEvent::AskRecorded {
                tool_call_id,
                question,
                ..
            } => {
                state.pending_asks.push(PendingAsk {
                    tool_call_id,
                    question,
                });
                state.status = SessionStatus::AwaitingInput {
                    asks: state.pending_asks.clone(),
                };
                if let Some(run) = state.run.as_mut() {
                    run.apply_awaiting();
                }
            }
            SessionDomainEvent::TurnEnded { .. }
            | SessionDomainEvent::TurnStopped { .. }
            | SessionDomainEvent::TurnInterrupted { .. } => {
                state.status = SessionStatus::Idle;
            }
            SessionDomainEvent::TurnFailed { error, .. } => {
                state.status = SessionStatus::Failed {
                    reason: error.clone(),
                };
                state.last_error = Some(error);
            }
            SessionDomainEvent::SessionFailed { reason, .. } => {
                state.status = SessionStatus::Unrecoverable {
                    reason: reason.clone(),
                };
                state.last_error = Some(reason);
            }
            other => unreachable!("Turns was handed {other:?}"),
        }
    }
}
