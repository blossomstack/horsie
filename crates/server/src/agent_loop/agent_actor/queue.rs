//! The queue component: what this agent has accepted, and how it becomes the
//! next input.
//!
//! An accepted message is a promise: it is journaled *before* anything is done
//! with it, so a crash cannot forget it and the ack a caller waits on reports
//! the durable write rather than a mailbox. Whether it is taken, and when, is
//! [`Components::advance`](super::boundary)'s decision — this component only
//! offers ([`crate::agent_loop::queued_offer`]) and takes.
//!
//! Taking a turn's input is a *write*: the items it consumes are crossed off
//! and the input message is journaled here rather than by whatever runs next,
//! so a provider retry can never double-persist it. Nothing is started from
//! this file.

use super::*;
use crate::agent_loop::context::AgentOutcome;
use async_trait::async_trait;
use horsie_actor::CommandEffect;
use horsie_agentcore::{
    AgentInput, AgentLogBody, AskLifecycle, LifecycleEvent, QueuedLifecycle, TurnBeganLifecycle,
};
use horsie_models::now_ms;

/// What this agent has accepted, and what it is waiting to be told.
///
/// The fields are private to this file, which is the enforcement: no other
/// component, and not the actor, can read or move them. What the rest of the
/// server may know about a queue is the four methods below, forwarded by
/// [`AgentState`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct QueueState {
    /// Accepted-but-undelivered things addressed to this agent, oldest first.
    ///
    /// Durable for the same reason timers are — an accepted message is a
    /// promise, and a crash must not forget it. Here rather than on the
    /// session because a message is addressed to an *agent*: once one can name
    /// a subagent or a workflow step, a session-level queue has nowhere to put
    /// it.
    inbox: Vec<crate::agent_loop::Incoming>,
    /// Every question this agent is parked on, oldest first. A turn may ask
    /// several at once, and none of them can be answered alone.
    asks: Vec<crate::agent_loop::AskedQuestion>,
    /// True while the agent has parked itself awaiting something that will
    /// wake it — a timer, a subagent still working.
    parked: bool,
}

impl QueueState {
    pub(super) fn inbox(&self) -> &[crate::agent_loop::Incoming] {
        &self.inbox
    }

    pub(super) fn asks(&self) -> &[crate::agent_loop::AskedQuestion] {
        &self.asks
    }

    pub(super) fn parked(&self) -> bool {
        self.parked
    }

    fn accept(&mut self, item: crate::agent_loop::Incoming) {
        self.inbox.push(item);
    }

    fn cross_off(&mut self, ids: &[String]) {
        self.inbox.retain(|i| !ids.iter().any(|id| id == i.id()));
    }

    fn park_on(&mut self, asks: Vec<crate::agent_loop::AskedQuestion>) {
        self.asks = asks;
    }

    fn clear_asks(&mut self) {
        self.asks.clear();
    }

    fn park(&mut self) {
        self.parked = true;
    }

    fn unpark(&mut self) {
        self.parked = false;
    }
}

impl PartState for QueueState {
    /// Parked on questions, with nothing queued entitled to abandon them.
    ///
    /// Asked of this component's own rule — the same one that decides what a
    /// turn may take — so "what may override a park" is stated once.
    fn blocks(&self, _state: &AgentState) -> Option<Blocked> {
        (!self.asks.is_empty()
            && crate::agent_loop::queued_offer(&self.inbox, &self.asks).is_none())
        .then_some(Blocked::Parked)
    }

    /// Nothing. A sub session that inherited an ask would park on a question
    /// nobody put to it, and an inherited queue would answer messages that
    /// were addressed to another session.
    fn carried(&self) -> Option<Self> {
        None
    }
}

/// The component itself: what this agent has accepted, and how it becomes the
/// next input.
#[derive(Default)]
pub(super) struct Queue {
    /// Whether this agent load has fired its start hook. Deliberately **not**
    /// journaled — a rehydrated agent fires again, which is precisely what
    /// `source: "resume"` means.
    start_hook_fired: bool,
}

impl Queue {
    /// Take this input: cross off what it consumes, journal what the model
    /// will read, and run the turn's pre-start hooks if any are owed.
    ///
    /// `TurnBegan` is journaled here, at the decision, rather than after the
    /// hooks: a crash in the hook window replays with the queue still owed,
    /// which redelivers the message — the same at-least-once the session's
    /// tell-then-persist has always had, and the direction to err in.
    pub(super) async fn take(
        &mut self,
        turn: crate::agent_loop::Turn,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let state = cx.state.clone();
        CommandEffect::persist(self.begin_turn(turn, &state, cx).await)
    }

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
        // session to `Running`. A message joining a turn already in flight is
        // not a new turn and says nothing new.
        if !state.turn_in_flight() {
            cx.runtime
                .parent
                .deliver(AgentOutcome::Started {
                    agent: cx.runtime.journal_id,
                })
                .await;
        }

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
                        work: cx.scratch.work,
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
        let (work, _) = cx.scratch.begin(WorkKind::Hooks);
        // Set when the prepare task is *spawned*, not when it returns: a
        // failed prepare must not re-fire the start hook on the next turn,
        // which would inject its context a second time.
        self.start_hook_fired = true;
        let provider = cx.runtime.context_provider.clone();
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let prepared = match provider.start_hooks(start).await {
                Ok(prep) => PreparedStart {
                    work,
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
                    work,
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
            ..
        } = prepared;
        let crate::agent_loop::Turn {
            message,
            artifacts,
            subagent_results,
            results,
            ..
        } = turn;

        let at_ms = now_ms();
        let mut events = Vec::new();
        for (seq, record) in (state.hook_entry_count()..).zip(records) {
            events.push(AgentDomainEvent::HookRan { record, seq, at_ms });
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
            // must be able to see which plugin refused it and why. The turn
            // this was preparing is abandoned with them — `TurnBegan` already
            // took its input, and the agent is left owing a call it will make
            // with whatever the next message brings.
            events.push(AgentDomainEvent::RunCancelled { at_ms });
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
                // Nothing is decided here. The write is the whole job, and the
                // advance the actor makes once it is durable is what looks at
                // a queue that now holds this item.
                let effect = CommandEffect::persist(vec![AgentDomainEvent::Received {
                    item,
                    at_ms: now_ms(),
                }]);
                match ack {
                    Some(ack) => effect.and_ack(ack),
                    None => effect,
                }
            }
            QueueCommand::Answer { answers, reply } => {
                // Work in flight means the questions are already gone — a turn
                // beginning is what clears them — so there is nothing to
                // answer.
                if cx.scratch.running.is_some() || cx.state.asks().is_empty() {
                    let _ = reply.send(Err(crate::agent_loop::AnswerError::NothingPending));
                    return CommandEffect::none();
                }
                let state = cx.state.clone();
                match crate::agent_loop::answered_turn(state.inbox(), state.asks(), answers) {
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
                if !cx.scratch.finished(prepared.work) {
                    return CommandEffect::none();
                }
                let state = cx.state.clone();
                CommandEffect::persist(self.start_prepared(*prepared, &state, cx).await)
            }
        }
    }

    /// What the queue holds, what a turn took from it, and what it parked on.
    ///
    /// Two of these arms move the *turn's* flag. They do it by calling that
    /// component's own method, never by touching its fields: taking input is
    /// what makes a turn owed, and parking is what ends one, so the decision
    /// belongs here even though the flag does not.
    // The fallthrough is unreachable by construction: `Components::apply`
    // routes every variant to exactly one component, so an event added later
    // fails to compile *there* — where it should be classified — rather than
    // silently reaching the wrong fold here.
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
                if let Some(part) = state.part_mut::<QueueState>() {
                    part.accept(item);
                }
            }
            AgentDomainEvent::Consumed { ids, .. } => {
                if let Some(part) = state.part_mut::<QueueState>() {
                    part.cross_off(&ids);
                }
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
                    .inbox()
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
                if let Some(part) = state.part_mut::<QueueState>() {
                    part.cross_off(&consumed);
                    // A turn beginning ends the park either way: the questions
                    // were answered, or the user moved on and they were
                    // abandoned. Both record a result for every call before
                    // the turn starts.
                    part.clear_asks();
                }
                if let Some(part) = state.part_mut::<TurnState>() {
                    part.began();
                }
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
                if let Some(part) = state.part_mut::<QueueState>() {
                    part.park_on(asks);
                }
                // Parking on a question is a turn boundary: the run is over and
                // the answer starts the next one.
                if let Some(part) = state.part_mut::<TurnState>() {
                    part.ended();
                }
            }
            AgentDomainEvent::InputMessage { message } => {
                // A new turn began — the agent is no longer parked.
                if let Some(part) = state.part_mut::<QueueState>() {
                    part.unpark();
                }
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::Parked { .. } => {
                if let Some(part) = state.part_mut::<QueueState>() {
                    part.park();
                }
                // Parking is a turn ending properly: the nudge budget is for
                // turns that end with nothing to wake them.
                if let Some(part) = state.part_mut::<TurnState>() {
                    part.ended_properly();
                }
            }
            _ => {}
        }
    }
}
