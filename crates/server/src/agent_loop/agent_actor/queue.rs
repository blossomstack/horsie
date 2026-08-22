//! The queue, and the decision to drain it.
//!
//! An accepted message is a promise: it is journaled *before* anything is done
//! with it, so a crash cannot forget it and the ack a caller waits on reports
//! the durable write rather than a mailbox. Whether it becomes a turn is a
//! separate decision, taken immediately afterwards against the state that write
//! left behind — never against the pre-command snapshot, or an agent that has
//! just parked would drain the report the park is supposed to hold.
//!
//! This is the only module that starts work of its own accord, which is why the
//! agent has no `actions` seam: there would be nobody else to concatenate with.

use super::*;
use crate::agent_loop::context::AgentOutcome;
use horsie_actor::{ActorContext, CommandEffect, EventSourcedActor};
use horsie_agentcore::{
    AgentInput, AgentLogBody, AskLifecycle, LifecycleEvent, QueuedLifecycle, TurnBeganLifecycle,
};
use horsie_models::now_ms;

impl AgentActor {
    /// Reconsider whether the queue may start a turn, and start it if so.
    ///
    /// Called after everything that could have changed the answer: something
    /// arriving, a turn ending, a park, a readiness flip. Deliberately silent
    /// when it decides against — finding a run already in flight is the normal
    /// case, not a fault, and the queue simply waits for the next boundary.
    ///
    /// `state` must be the state as the caller's own events leave it, not the
    /// pre-command snapshot: an agent that has just journaled `AskRecorded` is
    /// parked as far as this decision is concerned, and asking against the
    /// snapshot would drain a report the park is supposed to hold.
    pub(super) async fn try_drain(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> Vec<AgentDomainEvent> {
        if self.busy() || !self.ready {
            return Vec::new();
        }
        match crate::agent_loop::queued_turn(&state.inbox, &state.asks) {
            Some(turn) => self.begin_turn(turn, state, ctx).await,
            None => Vec::new(),
        }
    }

    /// Perform one turn decision: record what it consumes and answers, tell the
    /// owner the turn began, then run its pre-start hooks before the run itself.
    ///
    /// `TurnBegan` is journaled here, at the decision, rather than after the
    /// hooks: a crash in the hook window replays with the queue still owed,
    /// which redelivers the message — the same at-least-once the session's
    /// tell-then-persist has always had, and the direction to err in.
    pub(super) async fn begin_turn(
        &mut self,
        turn: crate::agent_loop::Turn,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> Vec<AgentDomainEvent> {
        let mut events = vec![AgentDomainEvent::TurnBegan {
            consumed: turn.consumed.clone(),
            answered: turn.answered.clone(),
            at_ms: now_ms(),
        }];
        // The owner no longer learns a turn began by being the thing that began
        // it, so it is told. Before the work, not after: this is what moves a
        // session to `Running`.
        self.ctx
            .parent
            .deliver(AgentOutcome::Started {
                agent: self.ctx.journal_id,
            })
            .await;

        let start = crate::agent_loop::StartTurn {
            // An agent that has never spoken to a provider is starting up;
            // anything else was folded from a journal. Read off the *LLM*
            // entries rather than the log, which a queued message alone already
            // appends to.
            start_source: (!self.start_hook_fired).then_some(match state.has_run() {
                false => horsie_models::runtime::SessionStartSource::Startup,
                true => horsie_models::runtime::SessionStartSource::Resume,
            }),
            prompt: turn.message.clone(),
        };
        let nothing_due = start.start_source.is_none() && start.prompt.is_none();
        if nothing_due || !self.ctx.context_provider.has_start_hooks() {
            events.extend(
                self.start_prepared(
                    PreparedStart {
                        turn,
                        records: Vec::new(),
                        abandon: None,
                    },
                    state,
                    ctx,
                )
                .await,
            );
            return events;
        }
        self.preparing = true;
        // Set when the prepare task is *spawned*, not when it returns: a
        // failed prepare must not re-fire the start hook on the next turn,
        // which would inject its context a second time.
        self.start_hook_fired = true;
        let provider = self.ctx.context_provider.clone();
        let self_ref = ctx.self_ref();
        tokio::spawn(async move {
            let prepared = match provider.start_hooks(start).await {
                Ok(prep) => PreparedStart {
                    abandon: crate::agent_loop::start_blocked(&prep.records)
                        .map(AbandonedStart::Blocked),
                    records: prep.records,
                    // A rewritten prompt replaces the turn's input; an absent
                    // one leaves what the user actually sent.
                    turn: crate::agent_loop::Turn {
                        message: prep.message.or(turn.message),
                        ..turn
                    },
                },
                Err(error) => PreparedStart {
                    turn,
                    records: Vec::new(),
                    abandon: Some(AbandonedStart::Failed(error)),
                },
            };
            let _ = self_ref
                .tell(AgentCommand::Queue(QueueCommand::StartPrepared(Box::new(
                    prepared,
                ))))
                .await;
        });
        events
    }

    /// Journal a prepared turn's hook records, then start it — or abandon it.
    ///
    /// The records are folded into a local copy of state before the prompt is
    /// read, which is the whole point of the prepare step: `state` here is the
    /// pre-command snapshot, and a `SessionStart` record that is not folded in
    /// first would first reach the model on the *next* turn.
    pub(super) async fn start_prepared(
        &mut self,
        prepared: PreparedStart,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> Vec<AgentDomainEvent> {
        let PreparedStart {
            turn,
            records,
            abandon,
        } = prepared;
        let crate::agent_loop::Turn {
            message,
            subagent_results,
            results,
            summarise,
            ..
        } = turn;
        // A turn that carries only a summarisation has nothing to say to the
        // model. Running it would spend a provider call answering a message
        // nobody sent, so the summary *is* the turn.
        let summarise_only = summarise.is_some()
            && message.is_none()
            && subagent_results.is_empty()
            && results.is_empty();

        let at_ms = now_ms();
        let mut events = Vec::new();
        let mut folded = state.clone();
        for (seq, record) in (state.hook_entry_count()..).zip(records) {
            let event = AgentDomainEvent::HookRan { record, seq, at_ms };
            folded = Self::apply_event(folded, event.clone());
            events.push(event);
        }

        if let Some(abandon) = abandon {
            // A preparation failure is reported exactly as the same failure
            // coming out of `provide` would be — `terminal` above all, which is
            // what tells the session its sandbox is gone for good rather than
            // merely unreachable. A refusal is neither: the prompt was read and
            // rejected, so retrying it unchanged would be rejected again.
            let (error, recoverable, terminal) = match abandon {
                AbandonedStart::Blocked(reason) => (reason, false, false),
                AbandonedStart::Failed(e) => (e.message, true, e.terminal),
            };
            self.ctx
                .parent
                .deliver(AgentOutcome::Failed {
                    agent: self.ctx.journal_id,
                    error,
                    recoverable,
                    terminal,
                })
                .await;
            // The records are still journaled: a user whose prompt was refused
            // must be able to see which plugin refused it and why.
            return events;
        }

        // The ids answered here are not dangling, whatever the recovered
        // history says: their results are in this very input.
        let answering: std::collections::HashSet<String> =
            results.iter().map(|r| r.tool_call_id.clone()).collect();
        // Sanitize on every turn start: a history recovered from a
        // mid-turn crash may carry dangling tool calls (a no-op when
        // well-formed).
        let mut history = repair_unanswered_tool_calls_except(folded.prompt_messages(), &answering);

        // Results that precede a user message belong to the history, not
        // to the input: the turn is started by what the user said.
        let starts_a_user_turn = message.is_some() || !subagent_results.is_empty();
        let agent_input = if starts_a_user_turn {
            if !results.is_empty() {
                let recorded = AgentInput::tool_results(results).to_message(now_ms());
                events.push(AgentDomainEvent::InputMessage {
                    message: recorded.clone(),
                });
                history.push(recorded);
            }
            AgentInput::user_message_with_results(
                new_message_id(),
                message.unwrap_or_default(),
                subagent_results,
            )
        } else {
            AgentInput::tool_results(results)
        };
        // Persist the input message here (not via the streaming sink), so a
        // turn-restarting provider retry that re-emits it can never
        // double-persist it into two consecutive user messages.
        //
        // A summarise-only turn is the one case with no input at all: nothing
        // was typed and nothing is owed, so this would journal the empty `Tool`
        // message `AgentInput::tool_results(vec![])` builds — which the run
        // below never reads, but which every *later* turn would then carry in
        // its prompt.
        if !summarise_only {
            events.push(AgentDomainEvent::InputMessage {
                message: agent_input.to_message(now_ms()),
            });
        }
        self.start_run(
            agent_input,
            ctx,
            history,
            folded.context_tokens,
            summarise.clone(),
            summarise_only,
        );
        events
    }
}

/// The queue, and the decision to drain it.
pub(super) struct Queue;

impl Queue {
    pub(super) async fn handle(
        actor: &mut AgentActor,
        state: &AgentState,
        cmd: QueueCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            QueueCommand::Enqueue { item, ack } => {
                // Decided after the write, never before it: the queue a turn
                // drains has to be the durable one, so the drain arrives as its
                // own command and finds this event already folded in.
                let _ = ctx
                    .self_ref()
                    .tell(AgentCommand::Queue(QueueCommand::Drain))
                    .await;
                let effect = CommandEffect::persist(vec![AgentDomainEvent::Received {
                    item,
                    at_ms: now_ms(),
                }]);
                match ack {
                    Some(ack) => effect.and_ack(ack),
                    None => effect,
                }
            }
            QueueCommand::Drain => CommandEffect::persist(actor.try_drain(state, ctx).await),
            QueueCommand::Answer { answers, reply } => {
                // A run in flight means the questions are already gone — a turn
                // beginning is what clears them — so there is nothing to answer.
                if actor.busy() {
                    let _ = reply.send(Err(crate::agent_loop::AnswerError::NothingPending));
                    return CommandEffect::none();
                }
                match crate::agent_loop::answered_turn(&state.inbox, &state.asks, answers) {
                    Ok(turn) => {
                        let _ = reply.send(Ok(()));
                        CommandEffect::persist(actor.begin_turn(turn, state, ctx).await)
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        CommandEffect::none()
                    }
                }
            }
            QueueCommand::StartPrepared(prepared) => {
                actor.preparing = false;
                CommandEffect::persist(actor.start_prepared(*prepared, state, ctx).await)
            }
        }
    }
}

impl Component for Queue {
    /// What the queue holds, what a turn took from it, and what it parked on.
    // The fallthrough is unreachable by construction: `AgentActor::apply_event`
    // routes every variant to exactly one module, so an event added later fails
    // to compile *there* — where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::Received { item, at_ms } => {
                // Only a person's message becomes a visible queue entry. A
                // report and a timer are already narrated elsewhere — the
                // session records a subagent's news on this very log, and a
                // wake becomes the turn's own input message — so surfacing
                // them here would render the same fact twice.
                if let crate::agent_loop::Incoming::User { id, text } = &item {
                    state.push(
                        at_ms,
                        AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(QueuedLifecycle {
                            id: id.clone(),
                            text: text.clone(),
                        })),
                    );
                }
                state.inbox.push(item);
            }
            AgentDomainEvent::TurnBegan {
                consumed,
                answered,
                at_ms,
            } => {
                // The entry names only what a client is tracking — the queued
                // messages it is showing as unread. Reports and wakes were never
                // shown as queued, so crossing them off would name ids nothing
                // holds.
                let visible = state
                    .inbox
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
                state
                    .inbox
                    .retain(|i| !consumed.iter().any(|id| id == i.id()));
                // A turn beginning ends the park either way: the questions were
                // answered, or the user moved on and they were abandoned. Both
                // record a result for every call before the turn starts.
                state.asks.clear();
                state.turn_in_flight = true;
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
                state.asks = asks;
                // Parking on a question is a turn boundary: the run is over and
                // the answer starts the next one.
                state.turn_in_flight = false;
            }
            AgentDomainEvent::InputMessage { message } => {
                // A new turn began — the agent is no longer parked.
                state.parked = false;
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::Parked { .. } => {
                state.parked = true;
                state.turn_in_flight = false;
                // Parking is a turn ending properly: the budget is for turns
                // that end with nothing to wake them.
                state.nudges = 0;
            }
            _ => {}
        }
    }
}
