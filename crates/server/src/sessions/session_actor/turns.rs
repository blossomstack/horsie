//! The conversation: what a person sends, and what a turn's ending means.
//!
//! The session does not hold the message and does not decide the turn. A message
//! is addressed to an *agent* — once it can name a subagent or a workflow step, a
//! session-level queue has nowhere to put it — so this component resolves the
//! agent, forwards the message, and lets the agent's own durable write be what
//! the caller waits on.
//!
//! What is left here is the session's own half: an unnamed session takes its
//! title from its first message, a terminal session refuses one, and the status
//! moves as turns begin and end. All three are facts about the session, not
//! about the queue.
//!
//! Silent when `state.run` is set and no agent is named: a run works from its
//! definition and there is nobody to send *it* a message — though a step of one
//! can still be addressed directly.

use super::component::{ActionCx, Component};
use super::{AgentAction, LifecycleCommand, TurnEnd};
use super::{
    AnswerError, AskAnswer, CommandEffect, SessionActor, SessionCommand, SessionDomainEvent,
    SessionState, TurnCommand,
};
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::UserMessageError;
use crate::sessions::spec::SessionStatus;
use crate::sessions::workflow::WorkflowRunState;
use horsie_actor::ActorContext;
use horsie_actor::ReplyTo;
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

/// Turns.
pub(super) struct Turns;

impl Turns {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: TurnCommand,
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            TurnCommand::UserMessage {
                agent_id,
                text,
                reply,
            } => {
                actor
                    .on_user_message(state, agent_id, text, reply, ctx)
                    .await
            }
            TurnCommand::Stop { reply } => {
                // A run's step in flight is what a stop cancels there, and a
                // step can be *parked* on a question rather than running — the
                // run page offers Interrupt for that too — so the gate is "is
                // there a step to stop", not the session's status.
                let step = state.run.as_ref().and_then(WorkflowRunState::current);
                if step.is_none() && state.status != SessionStatus::Running {
                    let _ = reply.send(());
                    return CommandEffect::none();
                }
                actor.cancel_in_flight(state).await;
                let _ = reply.send(());
                let stopped = match step {
                    // Cancelling the agent is not enough on a run: without this
                    // the step's log entry stays `Running` for ever, so
                    // `current()` never clears and the driver starts nothing
                    // again — the run wedged while its page read "Running".
                    // `StepCancelled` suspends it, which is the state a retry
                    // can move.
                    Some(index) => vec![SessionDomainEvent::StepCancelled {
                        at_ms: now_ms(),
                        index,
                    }],
                    // Stop is a turn boundary like any other: the agent drains
                    // whatever arrived while the cancelled turn ran, because a
                    // stop cancels the turn, not the promise.
                    None => vec![SessionDomainEvent::TurnStopped { at_ms: now_ms() }],
                };
                actor.persist_and_advance(state, stopped, ctx).await
            }
            TurnCommand::Answer {
                agent_id,
                answers,
                reply,
            } => actor.on_answer(state, agent_id, answers, reply, ctx).await,
        }
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// Route a set of answers to the agent that asked.
    ///
    /// Pure routing, and the agent replies to the caller directly: it owns the
    /// questions, so it is the only thing that can tell a complete answer set
    /// from a partial one. A half-answered park would leave the wire holding a
    /// `tool_use` with no result, which is why the check has to live where the
    /// questions do.
    pub(super) async fn on_answer(
        &mut self,
        state: &SessionState,
        agent_id: Option<String>,
        answers: Vec<AskAnswer>,
        reply: ReplyTo<Result<(), AnswerError>>,
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        let Some((_, agent)) = self.resolve_agent(state, ctx, agent_id.as_deref()) else {
            let _ = reply.send(Err(AnswerError::NothingPending));
            return CommandEffect::none();
        };
        if agent
            .tell(AgentCommand::Answer { answers, reply })
            .await
            .is_err()
        {
            tracing::warn!(session = %self.id, "answers could not reach the agent");
        }
        CommandEffect::none()
    }

    /// What the main agent's turn ending means for the session.
    ///
    /// Lives here rather than in the actor's routing because "the turn is over"
    /// is this component's fact — the same outcome means something else entirely
    /// to a step or a subagent.
    ///
    /// No turn starts here: whether another follows is the agent's own decision,
    /// taken against its own queue. The boundary still flushes, because a result
    /// owed to a *subagent* parent strands the moment no further subagent
    /// outcome can arrive, and delivering those is still the session's job.
    pub(super) async fn on_main_outcome(
        &mut self,
        state: &SessionState,
        end: TurnEnd,
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        let events = match end {
            TurnEnd::Concluded { .. } => {
                vec![SessionDomainEvent::TurnEnded { at_ms: now_ms() }]
            }
            TurnEnd::Asked => {
                vec![SessionDomainEvent::AskRecorded { at_ms: now_ms() }]
            }
            // Only from a session that still believes the turn is running. The
            // agent reports what its *own* journal left open, and a turn that
            // failed before the loop began — abandoned by a start hook, or a
            // context that would not build — never banked a boundary there, so
            // the agent still calls it open while the session, which was told
            // directly, has already recorded `TurnFailed`. The session owns the
            // merged status, so the session decides; a report about anything but
            // a live turn is history that is already written.
            TurnEnd::Interrupted if state.status == SessionStatus::Running => {
                vec![SessionDomainEvent::TurnInterrupted { at_ms: now_ms() }]
            }
            TurnEnd::Interrupted => return CommandEffect::none(),
            // A runtime that a live vendor cannot produce is the one terminal
            // failure: re-provisioning would silently rebuild a workspace the
            // user believes they still have. Everything else — provider errors,
            // tool errors, a vendor that is merely offline — is a failed turn
            // they can retry.
            TurnEnd::Failed {
                error,
                terminal: true,
            } => {
                vec![SessionDomainEvent::SessionFailed {
                    at_ms: now_ms(),
                    reason: error,
                }]
            }
            TurnEnd::Failed {
                error,
                terminal: false,
            } => {
                vec![SessionDomainEvent::TurnFailed {
                    at_ms: now_ms(),
                    error,
                }]
            }
            TurnEnd::Parked => {
                let error = "agent parked; timers are not supported in sessions".to_string();
                vec![SessionDomainEvent::TurnFailed {
                    at_ms: now_ms(),
                    error,
                }]
            }
        };
        self.persist_and_advance(state, events, ctx).await
    }

    /// Accept a message for one of this session's agents.
    ///
    /// The reply is answered by the *agent*, once its write is durable — the
    /// oneshot is forwarded rather than resolved here, so this mailbox never
    /// blocks on a journal write while the caller still gets a promise that
    /// survives a crash.
    pub(super) async fn on_user_message(
        &mut self,
        state: &SessionState,
        agent_id: Option<String>,
        text: String,
        reply: ReplyTo<Result<String, UserMessageError>>,
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let SessionStatus::Unrecoverable { reason } = &state.status {
            let _ = reply.send(Err(UserMessageError::Unrecoverable(reason.clone())));
            return CommandEffect::none();
        }
        // A run works from its definition and has no main agent, so an
        // unaddressed message has nobody to reach. Naming a step is fine — that
        // agent exists and can be spoken to like any other.
        if self.spec.workflow.is_some()
            && agent_id.as_deref().unwrap_or(super::MAIN_AGENT_ID) == super::MAIN_AGENT_ID
        {
            let _ = reply.send(Err(UserMessageError::Rejected(
                "this session is a workflow run; name a step agent to message it".to_string(),
            )));
            return CommandEffect::none();
        }
        let Some((_, agent)) = self.resolve_agent(state, ctx, agent_id.as_deref()) else {
            let _ = reply.send(Err(UserMessageError::NotFound));
            return CommandEffect::none();
        };
        // An unnamed session is titled from its first message, once. The rule is
        // `SessionCore`'s — a session's name is its own bookkeeping, not the
        // turn's — so this only says when to apply it.
        self.title_from_first_message(&text).await;

        let id = Uuid::new_v4().to_string();
        // A built-in is resolved here, before anything treats the text as a
        // prompt: `/compact` asks the server to do something and must never
        // reach `expand_invocation`, a template, or the model. Consulted ahead
        // of the plugin catalogue, so an installed bundle cannot take over a
        // control the product owns.
        let item = match horsie_support::plugin::commands::parse_invocation(text.trim(), '/')
            .and_then(|(name, args)| {
                horsie_support::plugin::builtins::builtin(name).map(|b| (b, args))
            }) {
            Some((builtin, args)) if builtin.name == "compact" => Incoming::Compact {
                id: id.clone(),
                instructions: (!args.trim().is_empty()).then(|| args.trim().to_string()),
            },
            // Every other built-in, present and future. Reaching here means the
            // table names something this match does not handle, which is a bug
            // rather than a message: sending it on as a prompt would show the
            // user's `/thing` to the model as if it were prose.
            Some((builtin, _)) => {
                tracing::error!(builtin = builtin.name, "unhandled builtin command");
                Incoming::User {
                    id: id.clone(),
                    text: text.clone(),
                }
            }
            None => Incoming::User {
                id: id.clone(),
                text: text.clone(),
            },
        };
        let (tx, rx) = oneshot::channel();
        let accepted = id.clone();
        tokio::spawn(async move {
            let answer = match rx.await {
                Ok(Ok(())) => Ok(accepted),
                // Never written, so it is not owed an answer, and the caller
                // must not be told it was accepted.
                Ok(Err(e)) => Err(UserMessageError::Rejected(format!("persist message: {e}"))),
                Err(_) => Err(UserMessageError::NotFound),
            };
            let _ = reply.send(answer);
        });
        if agent
            .tell(AgentCommand::Enqueue {
                item,
                ack: Some(ReplyTo::from_sender(tx)),
            })
            .await
            .is_err()
        {
            tracing::warn!(session = %self.id, "message could not reach the agent");
        }

        // A session whose create failed has no runtime, so the message that the
        // UI invited ("send a message to try again") has to build one rather
        // than start a turn that would ask for it. The message waits in the
        // agent's queue and the create's own completion releases it, exactly as
        // at session creation.
        if matches!(state.status, SessionStatus::ProvisioningFailed { .. }) {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
                .await;
        }
        // A person acting is the boundary that flushes results owed to subagent
        // parents. Those strand once every node is terminal — no further
        // subagent outcome will arrive to trigger the flush — so the next thing
        // the user does has to be what delivers them.
        self.persist_and_advance(state, Vec::new(), ctx).await
    }
}

impl Component for Turns {
    /// Nothing. A conversation's turns are the agent's own decision now, taken
    /// against the queue it holds — the session has neither the message nor the
    /// gate any more.
    fn actions(_cx: &ActionCx<'_>, _state: &SessionState) -> Vec<AgentAction> {
        Vec::new()
    }

    /// Nothing. A turn the process died inside is reported by the agent whose
    /// turn it was, from its own recovery, and arrives here as an ordinary
    /// `AgentOutcome::Interrupted`.
    ///
    /// This used to self-send a reconcile command that asked "is the session
    /// `Running`?" — a question the session cannot answer about a *turn*, since
    /// a self-send queues behind everything the supervisor sent while the actor
    /// was loading. A message, an answer or a flushed subagent result handled
    /// first could start a real turn, and the reconcile then recorded *that* one
    /// as interrupted: the client dropped the text it was streaming, the run
    /// carried on generating with nothing able to stop it, and the session
    /// called itself idle while it did. Asking the agent removes the question.
    fn on_load(_cx: &ActionCx<'_>, _state: &SessionState) -> Option<SessionCommand> {
        None
    }

    /// A turn in flight. `WorkflowRun` answers for a step, so this is only ever
    /// asked about a conversation — but `status` is shared, so the check is the
    /// same either way and double-counting is harmless.
    fn busy(state: &SessionState) -> bool {
        matches!(state.status, SessionStatus::Running)
    }

    /// Everything a conversation records, which is now only `status`: a turn
    /// beginning, ending, failing or being interrupted is the session's own
    /// state, and what the turn actually carried belongs to the agent that ran it.
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
            SessionDomainEvent::TurnBegan { .. } => {
                state.status = SessionStatus::Running;
                // The previous turn's failure is history once a new turn is
                // under way; leaving it set makes the detail endpoint report a
                // stale error for the rest of the session's life.
                state.last_error = None;
            }
            SessionDomainEvent::AskRecorded { .. } => {
                state.status = SessionStatus::AwaitingInput;
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    //! The session's half of a conversation: what its status does as turns
    //! begin and end, what it refuses, and what a person acting releases.
    //!
    //! The queue's own rules — what merges, what waits out a park, what an
    //! answer must cover — belong to the agent that holds the queue and are
    //! tested in `crate::agent_loop::inbox`. What is left here is what the
    //! *session* still owns.
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn a_fresh_session_is_idle() {
        assert_eq!(SessionState::default().status, SessionStatus::Idle);
    }

    /// The session learns a turn began because the agent tells it, and that is
    /// the whole of what it records: `Running`, and last turn's failure cleared.
    #[test]
    fn a_turn_beginning_clears_the_previous_failure() {
        let s = fold(vec![
            SessionDomainEvent::TurnFailed {
                at_ms: 0,
                error: "provider exploded".into(),
            },
            SessionDomainEvent::TurnBegan { at_ms: 1 },
        ]);
        assert_eq!(s.status, SessionStatus::Running);
        // The detail endpoint reports `last_error`, so a turn that has just
        // started must not still be advertising the previous turn's failure.
        assert_eq!(s.last_error, None);
    }

    #[test]
    fn a_failed_turn_is_sticky_but_not_terminal() {
        let s = fold(vec![SessionDomainEvent::TurnFailed {
            at_ms: 0,
            error: "provider exploded".into(),
        }]);
        assert!(matches!(s.status, SessionStatus::Failed { .. }));
        assert_eq!(s.last_error.as_deref(), Some("provider exploded"));
    }

    #[test]
    fn stop_and_interrupt_both_land_idle() {
        for boundary in [
            SessionDomainEvent::TurnStopped { at_ms: 1 },
            SessionDomainEvent::TurnInterrupted { at_ms: 1 },
        ] {
            let s = fold(vec![SessionDomainEvent::TurnBegan { at_ms: 0 }, boundary]);
            assert_eq!(s.status, SessionStatus::Idle);
        }
    }

    /// A park is a status and nothing more. Which questions are pending is the
    /// agent's own state — it is what asked them and what answers them — so the
    /// session carries none of them.
    #[test]
    fn an_ask_parks_the_session_without_carrying_the_questions() {
        let s = fold(vec![SessionDomainEvent::AskRecorded { at_ms: 0 }]);
        assert_eq!(s.status, SessionStatus::AwaitingInput);
        let next = SessionActor::apply_event(s, SessionDomainEvent::TurnBegan { at_ms: 1 });
        assert_eq!(next.status, SessionStatus::Running);
    }

    /// A terminal session refuses a message outright rather than queueing one
    /// nothing will ever answer.
    #[tokio::test]
    async fn an_unrecoverable_session_refuses_a_message() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        journal
            .persist(
                &SessionActor::persistence_id_for(id),
                &[serde_json::to_vec(&SessionDomainEvent::SessionFailed {
                    at_ms: 0,
                    reason: "runtime gone".into(),
                })
                .unwrap()],
                0,
            )
            .await
            .unwrap();
        let session =
            horsie_actor::ActorSystem::new(journal.clone()).spawn_persistent(SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps,
                spawn_deaf_supervisor(),
                crate::sessions::Revisions::default(),
            ));

        let refuse = async || {
            session
                .ask(|reply| {
                    SessionCommand::Turn(TurnCommand::UserMessage {
                        agent_id: None,
                        text: "please".into(),
                        reply,
                    })
                })
                .await
                .unwrap()
                .unwrap_err()
        };

        // Refuse once to settle the log — loading writes to it, because a
        // session records its spec the first time it is loaded — then refuse
        // again and assert that second one added nothing.
        let _ = refuse().await;
        let settled = session_journal_len(&journal, id).await;

        let err = session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "please".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, UserMessageError::Unrecoverable(_)), "{err:?}");
        assert_eq!(
            session_journal_len(&journal, id).await,
            settled,
            "a refused message journals nothing, here or on the agent"
        );
    }

    /// Seed a session journal and load it, so the actor recovers from exactly
    /// what a killed process would have left.
    async fn load_from(
        deps: crate::sessions::spec::ServerDeps,
        id: Uuid,
        events: &[SessionDomainEvent],
    ) -> (
        horsie_actor::ActorRef<SessionCommand>,
        Arc<dyn horsie_actor::Journal>,
    ) {
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        let encoded: Vec<Vec<u8>> = events
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap())
            .collect();
        journal
            .persist(&SessionActor::persistence_id_for(id), &encoded, 0)
            .await
            .unwrap();
        let session =
            horsie_actor::ActorSystem::new(journal.clone()).spawn_persistent(SessionActor::new(
                id,
                actor_spec_fixture(),
                deps,
                spawn_deaf_supervisor(),
                crate::sessions::Revisions::default(),
            ));
        (session, journal)
    }

    /// The main agent says the turn its process died inside is over, and the
    /// session records it — the genuine case the repair exists for.
    #[tokio::test]
    async fn a_reported_interruption_ends_the_turn() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        // A journal that ends mid-turn: exactly what a process killed during a
        // run leaves behind.
        let (session, journal) =
            load_from(f.deps, id, &[SessionDomainEvent::TurnBegan { at_ms: 0 }]).await;

        session
            .tell(SessionCommand::AgentOutcome(
                crate::agent_loop::AgentOutcome::Interrupted { agent: id },
            ))
            .await
            .unwrap();

        wait_for_state(&journal, id, "the interrupted turn to be recorded", |s| {
            s.status == SessionStatus::Idle
        })
        .await;
    }

    /// A turn that failed before its loop began banks no boundary in the agent's
    /// journal, so the agent still calls it open and reports it at the next
    /// load. The session was told directly and has already recorded
    /// `TurnFailed`, so it owns the answer: a report about anything but a live
    /// turn is history that is already written.
    ///
    /// Without the check the session would report `Idle` over a failure the
    /// user is still looking at, and journal a turn boundary that never
    /// happened.
    #[tokio::test]
    async fn a_reported_interruption_leaves_a_failed_turn_alone() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let (session, journal) = load_from(
            f.deps,
            id,
            &[
                SessionDomainEvent::TurnBegan { at_ms: 0 },
                SessionDomainEvent::TurnFailed {
                    at_ms: 1,
                    error: "provider said no".into(),
                },
            ],
        )
        .await;

        session
            .tell(SessionCommand::AgentOutcome(
                crate::agent_loop::AgentOutcome::Interrupted { agent: id },
            ))
            .await
            .unwrap();

        // Nothing to wait *for*, so wait on something the same mailbox answers
        // after it: a read replies only once the outcome ahead of it is done.
        let (reply, rx) = oneshot::channel();
        session
            .tell(SessionCommand::Read(ReadCommand::Snapshot {
                reply: ReplyTo::from_sender(reply),
            }))
            .await
            .unwrap();
        assert_eq!(
            rx.await.unwrap().status,
            SessionStatus::Failed {
                reason: "provider said no".into()
            },
            "an interruption reported over a turn that already failed changed the status"
        );
        let events = crate::sessions::events::session_events(&journal, id).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionDomainEvent::TurnInterrupted { .. })),
            "a turn that already failed was journaled as interrupted too: {events:?}"
        );
    }

    /// A run works from its definition; there is nobody to send an unaddressed
    /// message to.
    #[tokio::test]
    async fn a_run_refuses_an_unaddressed_message() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(Script::of([Ok(concludes(
            serde_json::json!({"severity": "p0"}),
        ))]));
        let (_f, session, _id, _journal) = spawn_run_with_provider(provider).await;
        let err = session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "hello".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, UserMessageError::Rejected(_)), "{err:?}");
    }

    /// A message that reaches the agent is answered only once its write is
    /// durable — the promise a `202` makes is that the message survives a
    /// crash, and the agent is the only thing that can keep it.
    #[tokio::test]
    async fn a_message_is_acknowledged_after_the_agents_durable_write() {
        let (_f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let accepted = session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "go".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("an accepted message");
        assert!(!accepted.is_empty(), "the caller is given the message id");
        // The id names an entry in the agent's own log, which is where the
        // queue lives now.
        let page = main_history(&session).await;
        assert!(
            page.entries.iter().any(|e| matches!(
                &e.body,
                horsie_agentcore::AgentLogBody::Lifecycle(
                    horsie_agentcore::LifecycleEvent::MessageQueued(q)
                ) if q.id == accepted && q.text == "go"
            )),
            "the accepted message is an entry in the agent's log: {:?}",
            page.entries
        );
        let _ = id;
    }

    /// News that merely arrived does not override a park; the person's next
    /// message does, and carries the news with it.
    ///
    /// The rule itself is `queued_turn`'s and is tested there. This is the same
    /// rule end to end, through a real session, a real subagent and a real
    /// park — which is what proves the session is not quietly re-deciding it.
    #[tokio::test]
    async fn a_report_waits_out_an_awaiting_input_session() {
        use horsie_agentcore::{
            StopReason,
            testkit::{MockProvider, Script},
        };
        // Main's first call asks the user; every later call (the subagent's
        // run, then the main agent's answer turn) ends with plain text.
        let provider = MockProvider::scripted(
            Script::of([Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::ToolCall(
                    horsie_agentcore::ToolCallPart {
                        id: "ask-1".into(),
                        name: "ask_user".into(),
                        input: serde_json::json!({"question": "which one?"}),
                    },
                )],
                stop_reason: StopReason::ToolUse,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })])
            .then_repeating_with(|| {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "sub answer".to_string(),
                        },
                    )],
                    stop_reason: StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            }),
        );
        let (_f, session, id, journal) = spawn_session_with_provider(provider).await;

        // Park the session on the ask.
        send(&session, "start").await;
        for _ in 0..200 {
            let state = crate::sessions::events::fold_session_state(&journal, id).await;
            if state.status == SessionStatus::AwaitingInput {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A subagent completes while the session is AwaitingInput.
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |t| {
            t.node(sub).is_some_and(|r| {
                r.status == crate::sessions::subagents::SubAgentStatus::Completed && r.notified
            })
        })
        .await;
        // Delivered into the agent's queue, but the park holds: a report has no
        // opinion about the question, so it waits rather than overriding it.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.status,
            SessionStatus::AwaitingInput,
            "a report must not answer the question for the user"
        );

        // The user's reply is what releases it, and carries the report along.
        send(&session, "the first one").await;
        wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
        for _ in 0..200 {
            if subagent_texts(&main_history(&session).await)
                .iter()
                .any(|t| t.contains("[subagent \"research\" completed]"))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // A plain message does not answer the question — it abandons it and
        // starts a fresh turn — so the reply and the report ride in the *user
        // message*, while the abandoned ask gets a result of its own.
        let page = main_history(&session).await;
        let (results, texts): (Vec<String>, Vec<String>) = {
            let mut results = Vec::new();
            let mut texts = Vec::new();
            for part in page.messages().flat_map(|m| m.parts.iter()) {
                match part {
                    horsie_agentcore::ContentPart::ToolResult(r) => results.push(r.output.clone()),
                    horsie_agentcore::ContentPart::Text(t) => texts.push(t.text.clone()),
                    horsie_agentcore::ContentPart::ToolCall(_)
                    | horsie_agentcore::ContentPart::Thinking(_)
                    | horsie_agentcore::ContentPart::SubAgentResult(_) => {}
                }
            }
            (results, texts)
        };
        assert!(
            texts.iter().any(|t| t.contains("the first one")),
            "the user's own message must survive the turn: {texts:?}"
        );
        let reports = subagent_texts(&main_history(&session).await);
        assert!(
            reports
                .iter()
                .any(|t| t.contains("[subagent \"research\" completed]")),
            "the report rides the same turn: {reports:?}"
        );
        assert!(
            results.iter().any(|r| r.contains("not answered")),
            "the abandoned ask still gets a result, so nothing dangles: {results:?}"
        );
    }

    /// The whole point of the change: a message names an *agent*, and a
    /// subagent is an agent. It lands in that agent's queue and its log — never
    /// in the main agent's, which is where a session-level queue would have had
    /// to put it.
    #[tokio::test]
    async fn a_message_can_name_a_subagent() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;

        let accepted = session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: Some(sub.to_string()),
                    text: "also check the lockfile".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a subagent takes messages like any other agent");

        let queued_in = |page: &crate::agent_loop::LogPage| {
            page.entries.iter().any(|e| {
                matches!(
                    &e.body,
                    horsie_agentcore::AgentLogBody::Lifecycle(
                        horsie_agentcore::LifecycleEvent::MessageQueued(q)
                    ) if q.id == accepted
                )
            })
        };
        let sub_page = agent_history(&session, Some(sub.to_string())).await;
        assert!(
            queued_in(&sub_page),
            "the message belongs to the agent it named: {:?}",
            sub_page.entries
        );
        assert!(!queued_in(&main_history(&session).await), "and to no other");
    }

    /// An agent that does not exist is a 404, not a message quietly delivered
    /// somewhere else.
    #[tokio::test]
    async fn a_message_naming_an_unknown_agent_is_refused() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let err = session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: Some(Uuid::new_v4().to_string()),
                    text: "hello?".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, UserMessageError::NotFound), "{err:?}");
    }
}
