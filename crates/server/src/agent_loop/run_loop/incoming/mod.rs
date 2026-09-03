//! Durable incoming records and their conversion into the next provider input.
//!
//! Acceptance journals `Received` before acknowledging the sender. Ordering,
//! consumption, and pending asks are derived from history; there is no queue
//! state. [`RunLoop::advance`] alone decides when the next offer is consumed.
//!
//! Consuming an offer journals both `Consumed` and the exact provider input,
//! so retries and recovery cannot insert it twice.

mod pending;

pub use pending::{
    ABANDONED_ASK_RESULT, AnswerError, AskAnswer, Incoming, MERGE_SEPARATOR, PendingInput,
    TurnInput, answered_input, next_input,
};

use crate::agent_loop::prelude::*;
use horsie_actor::CommandEffect;
use horsie_agentcore::{
    AgentInput, AgentLogBody, AskLifecycle, LifecycleEvent, QueuedLifecycle, TurnBeganLifecycle,
};
use horsie_models::now_ms;
/// Accepts incoming records and prepares the next normal agent step.
/// Pending input and asks are derived from history; this handler owns no state
/// of its own.
#[derive(Default)]
pub(crate) struct IncomingHandler;

impl IncomingHandler {
    /// Take this input: cross off what it consumes, journal what the model
    /// will read, and run the turn's pre-start hooks if any are owed.
    ///
    /// `TurnBegan` is journaled here, at the decision, rather than after the
    /// hooks: a crash in the hook window replays with the input still pending,
    /// which redelivers the message — the same at-least-once the session's
    /// tell-then-persist has always had, and the direction to err in.
    pub(crate) async fn take(
        turn: crate::agent_loop::TurnInput,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let state = cx.state.clone();
        CommandEffect::persist(Self::begin_turn(turn, &state, cx).await)
    }

    async fn begin_turn(
        turn: crate::agent_loop::TurnInput,
        state: &AgentState,
        cx: &mut CommandContext<'_>,
    ) -> Vec<AgentDomainEvent> {
        let marker_seq = state.next_history_seq().saturating_add(1);
        let mut events = vec![
            AgentDomainEvent::TurnBegan {
                consumed: turn.consumed.clone(),
                answered: turn.answered.clone(),
                at_ms: now_ms(),
            },
            AgentDomainEvent::StepStarted {
                kind: StepKind::Provider,
            },
        ];
        let start = crate::agent_loop::StartTurn {
            // An agent that has never spoken to a provider is starting up;
            // anything else was folded from a journal. Read off the *LLM*
            // entries rather than the log, which a received message already
            // appends to.
            start_source: (!cx.step_run.start_hooks_ran()).then_some(match state.has_run() {
                false => horsie_models::runtime::SessionStartSource::Startup,
                true => horsie_models::runtime::SessionStartSource::Resume,
            }),
            prompt: turn.message.clone(),
        };
        let nothing_due = start.start_source.is_none() && start.prompt.is_none();
        if nothing_due || !cx.runtime.context_provider.has_start_hooks() {
            events.extend(
                Self::start_prepared(
                    PreparedInput {
                        marker_seq,
                        input: turn,
                        records: Vec::new(),
                        rejection: None,
                    },
                    state,
                    cx,
                )
                .await,
            );
            return events;
        }
        let cancel = cx.step_run.begin_start_hooks(marker_seq);
        // Set when the prepare task is *spawned*, not when it returns: a
        // failed prepare must not re-fire the start hook on the next turn,
        // which would inject its context a second time.
        cx.step_run.mark_start_hooks_ran();
        let provider = cx.runtime.context_provider.clone();
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let outcome = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                outcome = provider.start_hooks(start) => outcome,
            };
            let prepared = match outcome {
                Ok(prep) => PreparedInput {
                    marker_seq,
                    rejection: crate::agent_loop::start_blocked(&prep.records)
                        .map(RejectedInput::Blocked),
                    records: prep.records,
                    // A rewritten prompt replaces the turn's input; an absent
                    // one leaves what the user actually sent.
                    input: crate::agent_loop::TurnInput {
                        message: prep.message.or(turn.message),
                        ..turn
                    },
                },
                Err(error) => PreparedInput {
                    marker_seq,
                    input: turn,
                    records: Vec::new(),
                    rejection: Some(RejectedInput::Failed(error)),
                },
            };
            let _ = self_ref
                .tell(AgentCommand::Incoming(IncomingCommand::InputPrepared(
                    Box::new(prepared),
                )))
                .await;
        });
        events
    }

    /// Journal a prepared turn's hook records, then commit it — or rejection it.
    ///
    /// The records are folded into a local copy of state before the prompt is
    /// read, which is the whole point of the prepare step: `state` here is the
    /// pre-command snapshot, and a `SessionStart` record that is not folded in
    /// first would first reach the model on the *next* turn.
    async fn start_prepared(
        prepared: PreparedInput,
        state: &AgentState,
        _cx: &mut CommandContext<'_>,
    ) -> Vec<AgentDomainEvent> {
        let PreparedInput {
            input,
            records,
            rejection,
            ..
        } = prepared;
        let crate::agent_loop::TurnInput {
            message,
            artifacts,
            subagent_results,
            results,
            ..
        } = input;

        let at_ms = now_ms();
        let mut events = Vec::new();
        for (seq, record) in (state.hook_entry_count()..).zip(records) {
            events.push(AgentDomainEvent::HookRan { record, seq, at_ms });
        }

        if let Some(rejection) = rejection {
            // A preparation failure is reported exactly as the same failure
            // coming out of `provide` would be — `terminal` above all, which is
            // what tells the session its sandbox is gone for good rather than
            // merely unreachable. A refusal is neither: the prompt was read and
            // rejected, so retrying it unchanged would be rejected again.
            let (error, recoverable, terminal) = match rejection {
                RejectedInput::Blocked(reason) => (reason, false, false),
                RejectedInput::Failed(e) => (e.message, true, e.terminal),
            };
            events.extend([
                AgentDomainEvent::StepFailed {
                    reason: StepFailure::Provider(error.clone()),
                },
                AgentDomainEvent::TurnCancelled { at_ms },
                AgentDomainEvent::RunEnded {
                    reason: RunEnd::Failed {
                        error,
                        recoverable,
                        terminal,
                    },
                    at_ms,
                },
            ]);
            return events;
        }

        // Results that precede a user message belong to the history, not
        // to the input: the turn is started by what the user said.
        let starts_a_user_turn = message.is_some() || !subagent_results.is_empty();
        let agent_input = if starts_a_user_turn {
            if !results.is_empty() {
                let recorded = AgentInput::tool_results(results).to_message(now_ms());
                events.push(AgentDomainEvent::InputMessage { message: recorded });
            }
            AgentInput::user_message_with_results(
                new_message_id(),
                message.unwrap_or_default(),
                subagent_results,
                artifacts,
            )
        } else {
            AgentInput::tool_results(results)
        };
        events.push(AgentDomainEvent::InputMessage {
            message: agent_input.to_message(now_ms()),
        });
        events
    }
}

impl IncomingHandler {
    pub(crate) async fn handle(
        cmd: IncomingCommand,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            IncomingCommand::Receive { item, ack } => {
                // Nothing is decided here. The write is the whole job, and the
                // advance the actor makes once it is durable is what looks at
                // history that now contains this item.
                let effect = CommandEffect::persist(vec![AgentDomainEvent::Received {
                    item,
                    at_ms: now_ms(),
                }]);
                match ack {
                    Some(ack) => effect.and_ack(ack),
                    None => effect,
                }
            }
            IncomingCommand::Answer { answers, reply } => {
                // Work in flight means the questions are already gone — a turn
                // beginning is what clears them — so there is nothing to
                // answer.
                let asks = cx.state.pending_asks();
                if cx.step_run.is_running() || asks.is_empty() {
                    let _ = reply.send(Err(crate::agent_loop::AnswerError::NothingPending));
                    return CommandEffect::none();
                }
                let state = cx.state.clone();
                let incoming = state.pending_incoming();
                match crate::agent_loop::answered_input(&incoming, &asks, answers) {
                    Ok(turn) => {
                        let _ = reply.send(Ok(()));
                        CommandEffect::persist(Self::begin_turn(turn, &state, cx).await)
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        CommandEffect::none()
                    }
                }
            }
            IncomingCommand::InputPrepared(prepared) => {
                if !cx.step_run.finish_start_hooks(prepared.marker_seq) {
                    return CommandEffect::none();
                }
                let state = cx.state.clone();
                CommandEffect::persist(Self::start_prepared(*prepared, &state, cx).await)
            }
        }
    }

    /// Fold accepted input, consumption, and parked questions into read
    /// projections. `RunLoop::apply` already restricts which events arrive.
    #[allow(clippy::wildcard_enum_match_arm)]
    pub(crate) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::Received {
                item: crate::agent_loop::Incoming::User { id, text, .. },
                at_ms,
            } => {
                // Only a person's message becomes visible as queued. Reports
                // and wakes are already narrated by their own history records.
                state.push(
                    at_ms,
                    AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(QueuedLifecycle {
                        id,
                        text,
                    })),
                );
            }
            AgentDomainEvent::Received { .. } => {}
            AgentDomainEvent::Consumed { .. } => {}
            AgentDomainEvent::TurnBegan {
                consumed,
                answered,
                at_ms,
            } => {
                // The entry names only what a client is tracking — the queued
                // messages it is showing as unread. Reports and wakes were
                // never shown as queued, so crossing them off would name ids
                // nothing holds.
                let visible = state
                    .pending_incoming()
                    .iter()
                    .filter(|i| i.is_user() && consumed.iter().any(|id| id == i.id()))
                    .map(|i| i.id().to_string())
                    .collect();
                state.push(
                    at_ms,
                    AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(TurnBeganLifecycle {
                        consumed: visible,
                        answered: answered.clone(),
                    })),
                );
            }
            AgentDomainEvent::AskRecorded { asks, at_ms } => {
                for ask in &asks {
                    state.push(
                        at_ms,
                        AgentLogBody::Lifecycle(LifecycleEvent::AskRecorded(AskLifecycle {
                            tool_call_id: ask.tool_call_id.clone(),
                            question: ask.question.clone(),
                        })),
                    );
                }
            }
            AgentDomainEvent::InputMessage { message } => {
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::Parked { .. } => {}
            _ => {}
        }
    }
}
