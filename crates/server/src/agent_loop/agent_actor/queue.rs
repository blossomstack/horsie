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

    /// Perform one turn decision: record what it consumes and answers, tell
    /// the owner the turn began, then run its pre-start hooks before the run
    /// itself.
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
                // A run in flight means the questions are already gone — a
                // turn beginning is what clears them — so there is nothing to
                // answer.
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
    use crate::agent_loop::agent_actor::testing::*;
    use crate::agent_loop::context::{AgentOutcome, AgentOutcomeSink};
    use horsie_agentcore::AgentLogBody;
    // --- The pre-run hook seam ---
    //
    // `SessionStart` used to fire inside `provide()`, which runs on the run's
    // own task *after* the history snapshot — so a record journaled there first
    // reached the model on the following turn. These pin the seam that moved it
    // ahead of the snapshot, and the once-per-load bookkeeping that came with
    // it.

    mod start_hooks {
        use super::*;
        use horsie_actor::{ActorRef, ActorSystem, InMemoryJournal, Journal};
        use horsie_agentcore::EmptyToolbox;
        use horsie_agentcore::testkit::MockProvider;
        use horsie_models::hooks::{
            ContextInjected, HookAction, HookBlocked, HookRecord, SessionStartOutcome,
            SessionStartRecord, UserPromptSubmitOutcome, UserPromptSubmitRecord,
        };
        use std::sync::Mutex;

        /// A provider that answers `start_hooks` from a script and records
        /// every `StartTurn` it was asked about.
        struct HookingContext {
            llm: Arc<MockProvider>,
            records: Vec<HookRecord>,
            enabled: bool,
            seen: Mutex<Vec<crate::agent_loop::StartTurn>>,
        }

        impl HookingContext {
            fn new(llm: Arc<MockProvider>, records: Vec<HookRecord>) -> Arc<Self> {
                Arc::new(Self {
                    llm,
                    records,
                    enabled: true,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn disabled(llm: Arc<MockProvider>) -> Arc<Self> {
                Arc::new(Self {
                    llm,
                    records: Vec::new(),
                    enabled: false,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn sources(&self) -> Vec<Option<String>> {
                self.seen
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|t| t.start_source.as_ref().map(|s| s.as_wire().to_string()))
                    .collect()
            }
        }

        #[async_trait]
        impl crate::agent_loop::ContextProvider for HookingContext {
            async fn provide(
                &self,
            ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError> {
                Ok(crate::agent_loop::Contexts {
                    provider: self.llm.clone(),
                    toolbox: Arc::new(EmptyToolbox),
                    tool_narrowing: None,
                    system_prompt: None,
                    context_window: None,
                })
            }

            fn has_start_hooks(&self) -> bool {
                self.enabled
            }

            async fn start_hooks(
                &self,
                turn: crate::agent_loop::StartTurn,
            ) -> Result<crate::agent_loop::TurnPreparation, crate::agent_loop::ContextError>
            {
                self.seen.lock().unwrap().push(turn);
                Ok(crate::agent_loop::TurnPreparation {
                    records: self.records.clone(),
                    message: None,
                })
            }
        }

        struct ReportingParent(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
        #[async_trait]
        impl AgentOutcomeSink for ReportingParent {
            async fn deliver(&self, outcome: AgentOutcome) {
                let _ = self.0.send(outcome);
            }
        }

        type Outcomes = tokio::sync::mpsc::UnboundedReceiver<AgentOutcome>;

        fn spawn(provider: Arc<HookingContext>) -> (ActorRef<AgentCommand>, Outcomes) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = AgentRuntimeContext {
                context_provider: provider,
                revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
                parent: Arc::new(ReportingParent(tx)),
                journal_id: uuid::Uuid::new_v4(),
                ready: true,
            };
            let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
            let agent = crate::testing::spawn_detached(
                &ActorSystem::new(journal),
                AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
            );
            (agent, rx)
        }

        async fn prompt(agent: &ActorRef<AgentCommand>, text: &str, rx: &mut Outcomes) {
            agent
                .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                    item: crate::agent_loop::Incoming::User {
                        id: "m2".into(),
                        text: text.into(),
                    },
                    ack: None,
                }))
                .await
                .unwrap();
            terminal_outcome(rx).await;
        }

        /// Read past the outcomes that are not how a turn *ended*: `Started`
        /// precedes the work, and `UsageRecorded` rides alongside the terminal
        /// one.
        async fn terminal_outcome(rx: &mut Outcomes) -> AgentOutcome {
            loop {
                match rx.recv().await.expect("the turn must report an outcome") {
                    AgentOutcome::Started { .. }
                    | AgentOutcome::UsageRecorded { .. }
                    | AgentOutcome::SeedSummary { .. } => continue,
                    outcome => return outcome,
                }
            }
        }

        fn session_start(context: &str) -> HookRecord {
            HookRecord {
                plugin: "boot".into(),
                duration_ms: 1,
                halt: None,
                action: HookAction::SessionStart(SessionStartRecord {
                    source: "startup".into(),
                    system_message: None,
                    outcome: SessionStartOutcome::Ran(ContextInjected {
                        additional_context: Some(context.into()),
                    }),
                }),
            }
        }

        /// The regression the whole seam exists to prevent: `provide()` runs
        /// after the run has already snapshotted its history, so a record
        /// journaled there would first appear on turn two — leaving every
        /// session's opening turn unhooked.
        #[tokio::test]
        async fn session_start_context_reaches_the_very_first_prompt() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![session_start("pins node 22")]);
            let (agent, mut rx) = spawn(provider);

            prompt(&agent, "hi", &mut rx).await;

            let first = llm
                .requests()
                .into_iter()
                .next()
                .expect("one provider call");
            assert!(
                first.texts.iter().any(|t| t.contains("pins node 22")),
                "the first prompt must carry the start hook's context, got {:?}",
                first.texts
            );
        }

        /// `SessionStart` fired on every turn before this: `provide()` is
        /// per-run and its call had no guard, so every message re-ran every
        /// start hook and always reported `source: "startup"`.
        #[tokio::test]
        async fn a_second_turn_does_not_fire_the_start_hook_again() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![session_start("pins node 22")]);
            let (agent, mut rx) = spawn(provider.clone());

            prompt(&agent, "hi", &mut rx).await;
            prompt(&agent, "again", &mut rx).await;

            assert_eq!(
                provider.sources(),
                vec![Some("startup".to_string()), None],
                "the start hook is due once per load; the prompt hook every turn"
            );
        }

        /// A rehydrated agent is a `resume`, and it is the only other lifecycle
        /// transition horsie has. Detected from the transcript rather than a
        /// framework flag: a fresh agent has nothing in it.
        #[tokio::test]
        async fn an_agent_with_recovered_history_reports_source_resume() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![]);
            let (agent, mut rx) = spawn(provider.clone());
            // Stand in for a recovered load: a transcript that predates this
            // actor's first command, which is exactly what folding a journal
            // leaves behind.
            let (ack, done) = tokio::sync::oneshot::channel();
            agent
                .tell(AgentCommand::Run(RunCommand::PersistProgress {
                    events: vec![AgentDomainEvent::InputMessage {
                        message: user_msg("from a previous load"),
                    }],
                    ack: ReplyTo::from_sender(ack),
                }))
                .await
                .unwrap();
            done.await.unwrap().unwrap();

            prompt(&agent, "carry on", &mut rx).await;

            assert_eq!(
                provider.sources(),
                vec![Some("resume".to_string())],
                "a transcript that predates this load means the agent was recovered"
            );
        }

        /// A blocked prompt never becomes a turn: nothing is journaled as input
        /// and no run starts. The record still lands, so the user can see which
        /// plugin refused it.
        #[tokio::test]
        async fn a_blocked_prompt_journals_no_input_and_starts_no_run() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(
                llm.clone(),
                vec![HookRecord {
                    plugin: "guard".into(),
                    duration_ms: 1,
                    halt: None,
                    action: HookAction::UserPromptSubmit(UserPromptSubmitRecord {
                        system_message: None,
                        outcome: UserPromptSubmitOutcome::Blocked(HookBlocked {
                            reason: Some("secrets in the prompt".into()),
                        }),
                    }),
                }],
            );
            let (agent, mut rx) = spawn(provider);

            agent
                .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                    item: crate::agent_loop::Incoming::User {
                        id: "m3".into(),
                        text: "my password is hunter2".into(),
                    },
                    ack: None,
                }))
                .await
                .unwrap();

            match terminal_outcome(&mut rx).await {
                AgentOutcome::Failed { error, .. } => {
                    assert_eq!(error, "secrets in the prompt");
                }
                other => panic!("expected the turn to be refused, got {other:?}"),
            }
            assert_eq!(llm.calls(), 0, "the model must never be reached");

            let page = agent
                .ask(|reply| {
                    AgentCommand::Read(ReadCommand::PageLog {
                        before: None,
                        max: 50,
                        reply,
                    })
                })
                .await
                .unwrap();
            // The queued message, the turn that took it, and the record that
            // refused it — but no input message, because no run began.
            assert!(
                !page
                    .entries
                    .iter()
                    .any(|e| matches!(e.body, AgentLogBody::Llm(_))),
                "a refused prompt must never reach the transcript: {:?}",
                page.entries
            );
            assert!(
                page.entries
                    .iter()
                    .any(|e| matches!(e.body, AgentLogBody::Hook(_))),
                "the refusal is auditable: {:?}",
                page.entries
            );
        }

        /// A preparation failure must classify itself exactly as the same
        /// failure out of `provide` would. Flattening `terminal` here leaves a
        /// session whose sandbox is gone for good reporting a retryable error,
        /// so it never reaches `Unrecoverable` and invites the user to try
        /// again forever.
        #[tokio::test]
        async fn a_terminal_preparation_failure_stays_terminal() {
            struct GoneContext;
            #[async_trait]
            impl crate::agent_loop::ContextProvider for GoneContext {
                async fn provide(
                    &self,
                ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError>
                {
                    Err(crate::agent_loop::ContextError::terminal("runtime is gone"))
                }
                fn has_start_hooks(&self) -> bool {
                    true
                }
                async fn start_hooks(
                    &self,
                    _: crate::agent_loop::StartTurn,
                ) -> Result<crate::agent_loop::TurnPreparation, crate::agent_loop::ContextError>
                {
                    Err(crate::agent_loop::ContextError::terminal("runtime is gone"))
                }
            }

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = AgentRuntimeContext {
                context_provider: Arc::new(GoneContext),
                revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
                parent: Arc::new(ReportingParent(tx)),
                journal_id: uuid::Uuid::new_v4(),
                ready: true,
            };
            let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
            let agent = crate::testing::spawn_detached(
                &ActorSystem::new(journal),
                AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
            );
            agent
                .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                    item: crate::agent_loop::Incoming::User {
                        id: "m4".into(),
                        text: "hi".into(),
                    },
                    ack: None,
                }))
                .await
                .unwrap();

            match terminal_outcome(&mut rx).await {
                AgentOutcome::Failed { terminal, .. } => {
                    assert!(terminal, "a gone sandbox is terminal wherever it surfaces");
                }
                other => panic!("expected the turn to fail, got {other:?}"),
            }
        }

        /// A session with no plugins pays nothing for a seam it cannot use.
        #[tokio::test]
        async fn a_provider_without_start_hooks_makes_no_prepare_round_trip() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::disabled(llm.clone());
            let (agent, mut rx) = spawn(provider.clone());

            prompt(&agent, "hi", &mut rx).await;

            assert!(
                provider.sources().is_empty(),
                "`has_start_hooks() == false` must skip the round-trip entirely"
            );
            assert_eq!(llm.calls(), 1, "the turn still runs");
        }
    }

    #[test]
    fn park_sets_parked_and_input_clears_it() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::Parked { at_ms: 0 });
        assert!(state.parked);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("wake"),
            },
        );
        assert!(!state.parked);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod queue_tests {
    //! The queue as the agent actually runs it: what a not-ready agent does
    //! with a message, what a boundary drains, and what an answer resumes.
    //!
    //! The *rule* is pure and tested in [`crate::agent_loop::inbox`]. These
    //! are about the actor around it — the gates it holds, and the events it
    //! journals.
    use super::*;
    use crate::agent_loop::AgentRunDef;
    use crate::agent_loop::agent_actor::testing::*;
    use crate::agent_loop::context::AgentOutcome;
    use crate::agent_loop::context::{ContextError, ContextProvider, Contexts};
    use horsie_actor::ActorRef;
    use horsie_actor::{ActorSystem, InMemoryJournal, Journal};
    use horsie_agentcore::testkit::MockProvider;
    use horsie_agentcore::{AgentLogBody, LifecycleEvent, LlmProvider};

    /// Hands the agent a provider that always ends the turn with plain text.
    struct TextContext(Arc<dyn LlmProvider>);
    #[async_trait]
    impl ContextProvider for TextContext {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            Ok(Contexts {
                provider: self.0.clone(),
                toolbox: Arc::new(horsie_agentcore::ToolboxImpl::new()),
                tool_narrowing: None,
                system_prompt: None,
                context_window: None,
            })
        }
    }

    fn spawn_with(
        provider: Arc<dyn ContextProvider>,
        ready: bool,
    ) -> (ActorRef<AgentCommand>, Outcomes) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: provider,
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: uuid::Uuid::new_v4(),
            ready,
        };
        let mut params = AgentParams::from_def(&AgentRunDef::default());
        params.interactive = true;
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );
        (agent, rx)
    }

    fn text_agent(ready: bool) -> (ActorRef<AgentCommand>, Outcomes) {
        spawn_with(Arc::new(TextContext(MockProvider::text("done"))), ready)
    }

    /// Exactly what a session sends when its sandbox lands or goes away: the
    /// same `Runtime` record a reader sees in the log, and nothing else.
    async fn set_ready(agent: &ActorRef<AgentCommand>, ready: bool) {
        let status = match ready {
            true => horsie_agentcore::RuntimeStatus::Ready(horsie_agentcore::EmptyOutcome {}),
            false => horsie_agentcore::RuntimeStatus::Acquiring(horsie_agentcore::EmptyOutcome {}),
        };
        agent
            .tell(AgentCommand::Log(LogCommand::RecordLifecycle {
                event: LifecycleEvent::Runtime(horsie_agentcore::RuntimeLifecycle {
                    status,
                    detail: None,
                }),
                at_ms: 0,
            }))
            .await
            .unwrap();
    }

    async fn send(agent: &ActorRef<AgentCommand>, id: &str, text: &str) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: id.into(),
                    text: text.into(),
                },
                ack: Some(ReplyTo::from_sender(tx)),
            }))
            .await
            .unwrap();
        rx.await.unwrap().expect("the message must be durable");
    }

    /// Every lifecycle entry kind in the agent's log, in order.
    async fn lifecycle(agent: &ActorRef<AgentCommand>) -> Vec<String> {
        let page = agent
            .ask(|reply| {
                AgentCommand::Read(ReadCommand::PageLog {
                    before: None,
                    max: 100,
                    reply,
                })
            })
            .await
            .unwrap();
        page.entries
            .iter()
            .filter_map(|e| match &e.body {
                AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(_)) => {
                    Some("MessageQueued".to_string())
                }
                AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(_)) => {
                    Some("TurnBegan".to_string())
                }
                AgentLogBody::Lifecycle(LifecycleEvent::AskRecorded(_)) => {
                    Some("AskRecorded".to_string())
                }
                AgentLogBody::Llm(_)
                | AgentLogBody::Hook(_)
                | AgentLogBody::Lifecycle(_)
                | AgentLogBody::Compaction(_) => None,
            })
            .collect()
    }

    /// Wait for `pred` to hold of the agent's lifecycle entries.
    async fn wait_lifecycle(
        agent: &ActorRef<AgentCommand>,
        what: &str,
        pred: impl Fn(&[String]) -> bool,
    ) {
        for _ in 0..200 {
            let kinds = lifecycle(agent).await;
            if pred(&kinds) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "{what} not reached within 2s; entries: {:?}",
            lifecycle(agent).await
        );
    }

    /// The ack is the promise. It resolves only once the message is written, so
    /// a caller holding it holds something that survives a crash.
    #[tokio::test]
    async fn a_message_is_acked_only_once_it_is_durable() {
        let (agent, _rx) = text_agent(true);
        // `send` awaits the ack, so by the time it returns the write has
        // happened — and the entry is already there to read.
        send(&agent, "m1", "hello").await;
        assert_eq!(
            lifecycle(&agent).await.first().map(String::as_str),
            Some("MessageQueued"),
            "the ack lands after the write, not before it"
        );
    }

    /// The one gate an agent cannot answer for itself. A message under a
    /// session still building its runtime waits — the whole of the fix for a
    /// first turn outrunning its own create — and the readiness that arrives
    /// when the create lands is what releases it.
    #[tokio::test]
    async fn a_message_waits_for_readiness_and_the_flip_releases_it() {
        let (agent, _rx) = text_agent(false);
        send(&agent, "m1", "hello").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            lifecycle(&agent).await,
            vec!["MessageQueued".to_string()],
            "a message with nowhere to run must not begin a turn"
        );

        set_ready(&agent, true).await;
        wait_lifecycle(&agent, "the released turn", |k| {
            k.contains(&"TurnBegan".to_string())
        })
        .await;
    }

    /// Losing readiness starts nothing; it only stops the next drain.
    #[tokio::test]
    async fn losing_readiness_starts_nothing() {
        let (agent, _rx) = text_agent(true);
        set_ready(&agent, false).await;
        send(&agent, "m1", "hello").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(lifecycle(&agent).await, vec!["MessageQueued".to_string()]);
    }

    /// A run in flight is not a reason to refuse a message — it is a reason to
    /// hold it. Two arrive under one hanging run and neither starts a second.
    #[tokio::test]
    async fn messages_arriving_mid_run_queue_rather_than_starting_a_second_turn() {
        let (agent, _rx) = spawn_with(Arc::new(HangingContext), true);
        send(&agent, "m1", "one").await;
        // The first drains immediately and hangs inside `provide`.
        wait_lifecycle(&agent, "the first turn", |k| {
            k.contains(&"TurnBegan".to_string())
        })
        .await;
        send(&agent, "m2", "two").await;
        send(&agent, "m3", "three").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let kinds = lifecycle(&agent).await;
        assert_eq!(
            kinds.iter().filter(|k| *k == "TurnBegan").count(),
            1,
            "a run in flight must never be drained into a second one: {kinds:?}"
        );
        assert_eq!(kinds.iter().filter(|k| *k == "MessageQueued").count(), 3);
    }

    /// `Started` precedes the work and is how the owner learns a turn began at
    /// all — it is no longer the thing that began it.
    #[tokio::test]
    async fn the_owner_is_told_the_turn_began_before_it_runs() {
        let (agent, mut rx) = spawn_with(Arc::new(HangingContext), true);
        send(&agent, "m1", "one").await;
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("the owner must be told")
            .expect("an outcome");
        assert!(
            matches!(first, AgentOutcome::Started { .. }),
            "the first report of a turn is that it started, got {first:?}"
        );
    }

    /// Answering is refused unless it covers the park exactly, and the refusal
    /// journals nothing — which is what makes retrying it free.
    #[tokio::test]
    async fn a_partial_answer_is_refused_and_journals_nothing() {
        let (agent, _rx) = text_agent(true);
        let (tx, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Queue(QueueCommand::Answer {
                answers: vec![crate::agent_loop::AskAnswer {
                    tool_call_id: "call-1".into(),
                    text: "main".into(),
                }],
                reply: ReplyTo::from_sender(tx),
            }))
            .await
            .unwrap();
        assert_eq!(
            rx.await.unwrap(),
            Err(crate::agent_loop::AnswerError::NothingPending)
        );
        assert!(lifecycle(&agent).await.is_empty());
    }
}
