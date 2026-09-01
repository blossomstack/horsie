//! The queue component: what this agent has accepted, and the decision to
//! answer it.
//!
//! An accepted message is a promise: it is journaled *before* anything is done
//! with it, so a crash cannot forget it and the ack a caller waits on reports
//! the durable write rather than a mailbox. Whether it becomes a turn is a
//! separate decision — `Drain` — taken against the state the write left
//! behind.
//!
//! Committing to a turn raises the shared `turn_live` gate and tells
//! [`RunCommand::StartTurn`]; the turn component picks it up after the turn's
//! input events are durable and folded. The queue never calls the turn — the
//! mailbox is the only channel between them.

use super::*;
use crate::agent_loop::context::AgentOutcome;
use async_trait::async_trait;
use horsie_actor::CommandEffect;
use horsie_agentcore::{
    AgentInput, AgentLogBody, AskLifecycle, LifecycleEvent, QueuedLifecycle, TurnBeganLifecycle,
};
use horsie_models::now_ms;

/// The queue, and the decision to drain it.
#[derive(Default)]
pub(super) struct Queue {
    /// A prepare step is in flight. Gates a drain exactly as `turn_live` does:
    /// between the drain decision and `StartPrepared` no turn exists yet, so
    /// the gate alone would let two turns through.
    preparing: bool,
    /// Whether this agent load has fired its start hook. Deliberately **not**
    /// journaled — a rehydrated agent fires again, which is precisely what
    /// `source: "resume"` means.
    start_hook_fired: bool,
}

impl Queue {
    /// Whether a turn may not start now: one is committed or in flight, or a
    /// prepare step is.
    fn busy(&self, cx: &Cx<'_>) -> bool {
        cx.scratch.turn_live || self.preparing
    }

    /// Reconsider whether the queue may start a turn, and start it if so.
    ///
    /// Deliberately silent when it decides against — finding a turn already in
    /// flight is the normal case, not a fault, and the queue simply waits for
    /// the boundary's own `Drain`.
    ///
    /// `state` must be the state as the caller's own events leave it, not the
    /// pre-command snapshot: an agent that has just journaled `AskRecorded` is
    /// parked as far as this decision is concerned.
    async fn try_drain(&mut self, state: &AgentState, cx: &mut Cx<'_>) -> Vec<AgentDomainEvent> {
        if self.busy(cx) || !cx.scratch.ready {
            return Vec::new();
        }
        match crate::agent_loop::queued_turn(&state.inbox, &state.asks) {
            Some(turn) => self.begin_turn(turn, state, cx).await,
            None => Vec::new(),
        }
    }

    /// Perform one turn decision: record what it consumes and answers, tell
    /// the owner the turn began, then run its pre-start hooks before the turn
    /// itself.
    ///
    /// `TurnBegan` is journaled here, at the decision, rather than after the
    /// hooks: a crash in the hook window replays with the queue still owed,
    /// which redelivers the message — the same at-least-once the session's
    /// tell-then-persist has always had, and the direction to err in.
    async fn begin_turn(
        &mut self,
        turn: crate::agent_loop::Turn,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> Vec<AgentDomainEvent> {
        let mut events = vec![AgentDomainEvent::TurnBegan {
            consumed: turn.consumed.clone(),
            answered: turn.answered.clone(),
            at_ms: now_ms(),
        }];
        // The owner no longer learns a turn began by being the thing that began
        // it, so it is told. Before the work, not after: this is what moves a
        // session to `Running`.
        cx.runtime
            .parent
            .deliver(AgentOutcome::Started {
                agent: cx.runtime.journal_id,
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
        if nothing_due || !cx.runtime.context_provider.has_start_hooks() {
            events.extend(
                self.start_prepared(
                    PreparedStart {
                        turn,
                        records: Vec::new(),
                        abandon: None,
                    },
                    state,
                    cx,
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
        let provider = cx.runtime.context_provider.clone();
        let self_ref = cx.actor.self_ref();
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

    /// Journal a prepared turn's hook records, then commit it — or abandon it.
    ///
    /// The records are folded into a local copy of state before the prompt is
    /// read, which is the whole point of the prepare step: `state` here is the
    /// pre-command snapshot, and a `SessionStart` record that is not folded in
    /// first would first reach the model on the *next* turn.
    async fn start_prepared(
        &mut self,
        prepared: PreparedStart,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> Vec<AgentDomainEvent> {
        let PreparedStart {
            turn,
            records,
            abandon,
        } = prepared;
        let crate::agent_loop::Turn {
            message,
            artifacts,
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
            folded = Components::apply(folded, event.clone());
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
            cx.runtime
                .parent
                .deliver(AgentOutcome::Failed {
                    agent: cx.runtime.journal_id,
                    error,
                    recoverable,
                    terminal,
                })
                .await;
            // The records are still journaled: a user whose prompt was refused
            // must be able to see which plugin refused it and why.
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
        // The input is journaled here rather than by the turn, so a provider
        // retry can never double-persist it into two consecutive user
        // messages.
        //
        // A summarise-only turn is the one case with no input at all: nothing
        // was typed and nothing is owed, so this would journal the empty `Tool`
        // message `AgentInput::tool_results(vec![])` builds — which the turn
        // never reads, but which every *later* turn would then carry in its
        // prompt.
        if !summarise_only {
            events.push(AgentDomainEvent::InputMessage {
                message: agent_input.to_message(now_ms()),
            });
        }
        // Commit: raise the gate before telling, so nothing drains a second
        // turn into the gap, and let the turn component pick the spec up once
        // these events are durable and folded.
        cx.scratch.turn_live = true;
        cx.tell(AgentCommand::Run(RunCommand::StartTurn {
            summarise,
            summarise_only,
        }))
        .await;
        events
    }
}

#[async_trait]
impl Component for Queue {
    type Command = QueueCommand;

    async fn handle(
        &mut self,
        cmd: QueueCommand,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            QueueCommand::Enqueue { item, ack } => {
                // Decided after the write, never before it: the queue a turn
                // drains has to be the durable one, so the drain arrives as its
                // own command and finds this event already folded in.
                cx.drain().await;
                let effect = CommandEffect::persist(vec![AgentDomainEvent::Received {
                    item,
                    at_ms: now_ms(),
                }]);
                match ack {
                    Some(ack) => effect.and_ack(ack),
                    None => effect,
                }
            }
            QueueCommand::Drain => {
                let state = cx.state.clone();
                CommandEffect::persist(self.try_drain(&state, cx).await)
            }
            QueueCommand::Answer { answers, reply } => {
                // A turn in flight means the questions are already gone — a
                // turn beginning is what clears them — so there is nothing to
                // answer.
                if self.busy(cx) {
                    let _ = reply.send(Err(crate::agent_loop::AnswerError::NothingPending));
                    return CommandEffect::none();
                }
                let state = cx.state.clone();
                match crate::agent_loop::answered_turn(&state.inbox, &state.asks, answers) {
                    Ok(turn) => {
                        let _ = reply.send(Ok(()));
                        CommandEffect::persist(self.begin_turn(turn, &state, cx).await)
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        CommandEffect::none()
                    }
                }
            }
            QueueCommand::StartPrepared(prepared) => {
                self.preparing = false;
                let state = cx.state.clone();
                CommandEffect::persist(self.start_prepared(*prepared, &state, cx).await)
            }
        }
    }

    /// What the queue holds, what a turn took from it, and what it parked on.
    // The fallthrough is unreachable by construction: `component::fold` routes
    // every variant to exactly one component, so an event added later fails to
    // compile *there* — where it should be classified — rather than silently
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
                if let crate::agent_loop::Incoming::User { id, text, .. } = &item {
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
                // messages it is showing as unread. Reports and wakes were
                // never shown as queued, so crossing them off would name ids
                // nothing holds.
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

    /// Re-ask the drain question. Covers the crash window between a boundary's
    /// persist and its `Drain` being handled: without this, a queued message
    /// could sit until the next enqueue happened to arrive.
    async fn on_load(&mut self, cx: &mut Cx<'_>) {
        cx.drain().await;
    }
}
