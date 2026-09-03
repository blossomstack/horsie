//! The session: what a person sends, and what a turn's ending means.
//!
//! The session does not hold the message and does not decide the turn. A
//! message is addressed to an *agent* — once it can name a subagent or a
//! workflow step, a session-level queue has nowhere to put it — so this
//! component resolves the agent, forwards the message, and lets the agent's
//! own durable write be what the caller waits on.
//!
//! What is left here is the session's own half: an unnamed session takes its
//! title from its first message, a terminal session refuses one, and the status
//! moves as turns begin and end. All three are facts about the session, not
//! about the queue.
//!
//! Silent when the session is a workflow run and no agent is named: a run
//! works from its definition and there is nobody to send *it* a message —
//! though a step of one can still be addressed directly.

use super::component::Component;
use super::{AgentAction, LifecycleCommand, TurnEnd};
use super::{
    AgentKey, AgentStatus, AnswerError, AskAnswer, CommandEffect, MessageAccepted,
    ProvisioningState, RequestedRuntime, SessionActor, SessionCommand, SessionDomainEvent,
    SessionState, SubSessionCommand, TurnCommand,
};
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::UserMessageError;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::run_forest::{SeedMode, SubAgentStatus, TurnPhase};
use horsie_agentcore::{EmptyOutcome, TurnOutcome};

/// A recognised sub session command: which of the two it was, and what the new
/// session is for.
///
/// A struct rather than two parameters because they are one decision, made in
/// one place and acted on in another.
struct SubSessionRequest {
    seed: SeedMode,
    message: String,
}
use crate::agent_loop::IncomingCommand as AgentIncomingCommand;
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
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            TurnCommand::UserMessage {
                agent_id,
                text,
                artifacts,
                reply,
            } => {
                actor
                    .on_user_message(state, agent_id, text, artifacts, reply, ctx)
                    .await
            }
            TurnCommand::Stop { agent_id, reply } => {
                actor.on_stop(state, &agent_id, reply, ctx).await
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
    /// Cancel one agent's turn, and journal the boundary its kind uses.
    ///
    /// Two questions, deliberately separate. *Which agent* is pure resolution
    /// from state — and unlike
    /// [`resolve_agent`](super::SessionActor::resolve_agent) it never spawns
    /// one, because waking a cold agent in order to stop it is work to achieve
    /// nothing. *Whether it is doing anything* is [`Self::stop_boundary`],
    /// which answers with the event to journal, so the gate and the record
    /// cannot disagree about what was stopped.
    pub(super) async fn on_stop(
        &mut self,
        state: &SessionState,
        agent_id: &str,
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let Some(key) = self.stop_target(state, agent_id) else {
            let _ = reply.send(Err(format!("no such agent: {agent_id}")));
            return CommandEffect::none();
        };
        let Some(stopped) = Self::stop_boundary(state, key) else {
            // Not working is not a failure. A client that pressed Stop as the
            // turn ended on its own has got what it asked for.
            let _ = reply.send(Ok(()));
            return CommandEffect::none();
        };
        self.cancel_agent(key).await;
        let _ = reply.send(Ok(()));
        // Stopping delegated work ends the delegation: everything running
        // under a stopped subagent or step goes with it. A main agent's or a
        // sub session's stop cancels only the *turn* — its subagents and
        // invoked runs are independent work that still delivers.
        let mut events = match key {
            AgentKey::Sub(id) | AgentKey::Step(id) => self.cancel_descendants(state, id).await,
            AgentKey::Main | AgentKey::SubSession(_) => Vec::new(),
        };
        events.push(stopped);
        self.persist_and_advance(state, events, ctx).await
    }

    /// Which agent `agent_id` names, without spawning anything.
    ///
    /// `main` on a workflow session resolves to the root run's step in flight:
    /// a run has no main agent, and at most one of its steps runs at a time,
    /// so there is nothing else an unaddressed stop could mean there.
    fn stop_target(&self, state: &SessionState, agent_id: &str) -> Option<AgentKey> {
        if agent_id == super::MAIN_AGENT_ID {
            return match state.forest.current_root_step_agent() {
                Some(step) => Some(AgentKey::Step(step)),
                None => Some(AgentKey::Main),
            };
        }
        let id = Uuid::parse_str(agent_id).ok()?;
        self.agent_key_of(state, id)
    }

    /// What to journal for stopping `key`, or `None` if it is not working.
    ///
    /// Every kind ends its turn in its own vocabulary — the main entry's turn
    /// phase, a step's log entry, a sub session's status, a subagent's node —
    /// so there is no one event that means "stopped" for all of them, and the
    /// mapping lives here rather than four times over.
    ///
    /// The gate is `Running` and not also `AwaitingInput`, except for a step.
    /// Cancelling does not clear the questions an agent is parked on, so a
    /// boundary journaled over a park would read `Idle` beside questions still
    /// pending. A step escapes that because `StepCancelled` suspends the
    /// execution outright, which is a state its own document can show.
    fn stop_boundary(state: &SessionState, key: AgentKey) -> Option<SessionDomainEvent> {
        let at_ms = now_ms();
        match key {
            // Stop is a turn boundary like any other: the agent drains whatever
            // arrived while the cancelled turn ran, because a stop cancels the
            // turn, not the promise.
            AgentKey::Main => {
                let agent = state.forest.root_id()?.0;
                (state.forest.main_turn() == Some(&TurnPhase::Running))
                    .then_some(SessionDomainEvent::TurnStopped { at_ms, agent })
            }
            // Cancelling the agent is not enough on a run: without this the
            // step's log entry stays `Running` for ever, so `current()` never
            // clears and the driver starts nothing again — the run wedged
            // while its page read "Running". `StepCancelled` suspends it,
            // which is the state a retry can move.
            AgentKey::Step(id) => {
                let (run, index) = state.forest.step_of_agent(id)?;
                (state.forest.workflow(run)?.run.current() == Some(index)).then_some(
                    SessionDomainEvent::StepCancelled {
                        at_ms,
                        run: run.0,
                        index,
                    },
                )
            }
            AgentKey::SubSession(id) => (state.forest.sub_session(id)?.status
                == AgentStatus::Running)
                .then_some(SessionDomainEvent::SubSessionTurnEnded {
                    at_ms,
                    id,
                    outcome: TurnOutcome::Stopped(EmptyOutcome {}),
                }),
            // The parent is blocked on this child's result, so stopping it
            // quietly would leave it waiting for one that can never come. The
            // same shape recovery delivers for a child a crash left running:
            // the parent hears a failure, and carries on.
            AgentKey::Sub(id) => (state.forest.sub(id)?.status == SubAgentStatus::Running)
                .then_some(SessionDomainEvent::SubAgentFailed {
                    at_ms,
                    id,
                    error: crate::sessions::run_forest::STOPPED_ERROR.to_string(),
                }),
        }
    }

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
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let Some((_, agent)) = self.resolve_agent(state, ctx, agent_id.as_deref()) else {
            let _ = reply.send(Err(AnswerError::NothingPending));
            return CommandEffect::none();
        };
        if agent
            .tell(AgentCommand::Incoming(AgentIncomingCommand::Answer {
                answers,
                reply,
            }))
            .await
            .is_err()
        {
            tracing::warn!(session = %self.id, "answers could not reach the agent");
        }
        CommandEffect::none()
    }

    /// What the main agent's turn ending means for the session.
    ///
    /// Lives here rather than in the actor's routing because "the turn is
    /// over" is this component's fact — the same outcome means something else
    /// entirely to a step or a subagent.
    ///
    /// No turn starts here: whether another follows is the agent's own
    /// decision, taken against its own queue. The boundary still flushes,
    /// because a result owed to a *subagent* parent strands the moment no
    /// further subagent outcome can arrive, and delivering those is still the
    /// session's job.
    pub(super) async fn on_main_outcome(
        &mut self,
        state: &SessionState,
        end: TurnEnd,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let agent = self.id;
        let events = match end {
            TurnEnd::Concluded { .. } => {
                vec![SessionDomainEvent::TurnEnded {
                    at_ms: now_ms(),
                    agent,
                }]
            }
            TurnEnd::Asked => {
                vec![SessionDomainEvent::AskRecorded {
                    at_ms: now_ms(),
                    agent,
                }]
            }
            // Only from a session that still believes the turn is running.
            // The agent reports what its *own* journal left open, and a turn
            // that failed before the loop began — abandoned by a start hook, or
            // a context that would not build — never banked a boundary there,
            // so the agent still calls it open while the session, which was
            // told directly, has already recorded `TurnFailed`. The session
            // owns the merged phase, so the session decides; a report about
            // anything but a live turn is history that is already written.
            TurnEnd::Interrupted if state.forest.main_turn() == Some(&TurnPhase::Running) => {
                vec![SessionDomainEvent::TurnInterrupted {
                    at_ms: now_ms(),
                    agent,
                }]
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
                    agent,
                    error,
                }]
            }
            TurnEnd::Parked => {
                let error = "agent parked; timers are not supported in sessions".to_string();
                vec![SessionDomainEvent::TurnFailed {
                    at_ms: now_ms(),
                    agent,
                    error,
                }]
            }
        };
        self.persist_and_advance(state, events, ctx).await
    }

    /// What a sub session command asked for, if the text is one.
    ///
    /// Pure and separate from acting on it, so `on_user_message` can classify
    /// before it gives up ownership of the reply — and so the table of what
    /// counts as a sub session command is testable with no actor in sight.
    fn sub_session_request(text: &str) -> Option<SubSessionRequest> {
        let (builtin, args) = horsie_support::plugin::commands::parse_invocation(text, '/')
            .and_then(|(name, args)| {
                horsie_support::plugin::builtins::builtin(name).map(|b| (b, args))
            })?;
        let seed = match builtin.name {
            "fork" => SeedMode::Copy,
            "summary-n-fork" => SeedMode::Summary,
            _ => return None,
        };
        Some(SubSessionRequest {
            seed,
            message: args.trim().to_string(),
        })
    }

    /// Hand a sub session command to the component that owns sub sessions.
    ///
    /// Recognising one is this component's job — it is where every built-in is
    /// caught, before the text can be treated as a prompt. *Creating* one is
    /// not: `state.sub_sessions` belongs to `SubSessions`, and a component
    /// writes only its own slice. So this decides what was typed and forwards
    /// a command, the same shape `/compact` has, where `Turns` compacts
    /// nothing and hands an `Incoming::Compact` to the agent.
    fn start_sub_session(
        &mut self,
        key: AgentKey,
        req: SubSessionRequest,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let SubSessionRequest { seed, message } = req;
        // A person typing `/fork` names nothing, and a sub session can no
        // longer name itself — `set_session_title` is the main agent's alone.
        // So it is named from its brief, by the same rule a session with no
        // name takes one from its first message. `spawn_subsession` reaches
        // this command with a title the caller chose and never lands here.
        let title =
            super::core::derive_title(&message).unwrap_or_else(|| "Sub session".to_string());
        // Which agent typed it, as an id `Create` can validate. Everything
        // else a sub session needs to be true — a message, a session rather
        // than a run — is checked there, where a sub session is written.
        let parent = match key {
            AgentKey::Main => self.id,
            AgentKey::SubSession(id) => id,
            AgentKey::Sub(id) | AgentKey::Step(id) => id,
        };
        // Off the mailbox: `Create` reads the source agent's log head and then
        // waits on its own write, and holding this mailbox across both would
        // stall every other agent in the session.
        let self_ref = self.me(ctx);
        tokio::spawn(async move {
            let created = self_ref
                .ask(|r| {
                    SessionCommand::SubSession(SubSessionCommand::Create {
                        parent,
                        seed,
                        message,
                        title,
                        // `/fork` and `/summary-n-fork` always inherit: a
                        // person branching a session means the same checkout
                        // and the same edits, which is the whole reason to
                        // branch rather than start something new.
                        env: RequestedRuntime::Inherit,
                        reply: r,
                    })
                })
                .await;
            let answer = match created {
                Ok(Ok(id)) => Ok(MessageAccepted {
                    message_id: id.to_string(),
                    sub_session: Some(id.to_string()),
                }),
                Ok(Err(why)) => Err(UserMessageError::Rejected(why)),
                Err(e) => Err(UserMessageError::Rejected(format!("sub session: {e}"))),
            };
            let _ = reply.send(answer);
        });
        CommandEffect::none()
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
        artifacts: Vec<horsie_models::agent::ArtifactRef>,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        if let Some(reason) = &state.fatal {
            let _ = reply.send(Err(UserMessageError::Unrecoverable(reason.clone())));
            return CommandEffect::none();
        }
        // A run works from its definition and has no main agent, so an
        // unaddressed message has nobody to reach. Naming a step is fine — that
        // agent exists and can be spoken to like any other.
        if self.spec().workflow_run().is_some()
            && agent_id.as_deref().unwrap_or(super::MAIN_AGENT_ID) == super::MAIN_AGENT_ID
        {
            let _ = reply.send(Err(UserMessageError::Rejected(
                "this session is a workflow run; name a step agent to message it".to_string(),
            )));
            return CommandEffect::none();
        }
        let Some((key, agent)) = self.resolve_agent(state, ctx, agent_id.as_deref()) else {
            let _ = reply.send(Err(UserMessageError::NotFound));
            return CommandEffect::none();
        };
        // Resolved before the session is titled, because a sub session command
        // is not a thing to name a session after: it says what the *new*
        // session should do.
        if let Some(req) = Self::sub_session_request(text.trim()) {
            return self.start_sub_session(key, req, reply, ctx);
        }
        // An unnamed session is titled from its first message, once. The rule
        // is `SessionCore`'s — a session's name is its own bookkeeping, not
        // the turn's — so this only says when to apply it.
        let titled = self.title_from_first_message(state, &text).await;

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
                    artifacts: artifacts.clone(),
                }
            }
            None => Incoming::User {
                id: id.clone(),
                text: text.clone(),
                artifacts,
            },
        };
        let (tx, rx) = oneshot::channel();
        let accepted = id.clone();
        tokio::spawn(async move {
            let answer = match rx.await {
                Ok(Ok(())) => Ok(MessageAccepted::queued(accepted)),
                // Never written, so it is not owed an answer, and the caller
                // must not be told it was accepted.
                Ok(Err(e)) => Err(UserMessageError::Rejected(format!("persist message: {e}"))),
                Err(_) => Err(UserMessageError::NotFound),
            };
            let _ = reply.send(answer);
        });
        if agent
            .tell(AgentCommand::Incoming(AgentIncomingCommand::Receive {
                item,
                ack: Some(ReplyTo::from_sender(tx)),
            }))
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
        if matches!(
            state.provisioning_for(self.id),
            Some(ProvisioningState::Failed { .. })
        ) {
            let _ = self
                .me(ctx)
                .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                    owner: self.id,
                    // The record keeps the environment; a retry rebuilds the
                    // same sandbox rather than re-resolving one.
                    env: None,
                }))
                .await;
        }
        // A person acting is the boundary that flushes results owed to subagent
        // parents. Those strand once every node is terminal — no further
        // subagent outcome will arrive to trigger the flush — so the next thing
        // the user does has to be what delivers them.
        self.persist_and_advance(state, titled.into_iter().collect(), ctx)
            .await
    }
}

impl Component for Turns {
    /// Nothing. A session's turns are the agent's own decision now, taken
    /// against the queue it holds — the session has neither the message nor the
    /// gate any more.
    fn actions(_state: &SessionState) -> Vec<AgentAction> {
        Vec::new()
    }

    /// Nothing. A turn the process died inside is reported by the agent whose
    /// turn it was, from its own recovery, and arrives here as an ordinary
    /// `AgentOutcome::Interrupted`.
    ///
    /// This used to self-send a reconcile command that asked "is the session
    /// `Running`?" — a question the session cannot answer about a *turn*,
    /// since a self-send queues behind everything the supervisor sent while
    /// the actor was loading. A message, an answer or a flushed subagent
    /// result handled first could start a real turn, and the reconcile then
    /// recorded *that* one as interrupted: the client dropped the text it was
    /// streaming, the run carried on generating with nothing able to stop it,
    /// and the session called itself idle while it did. Asking the agent
    /// removes the question.
    fn on_load(_state: &SessionState) -> Option<SessionCommand> {
        None
    }

    /// The main session's turn in flight. A step in flight is
    /// `WorkflowRuns`' answer; each component reports only its own slice now
    /// that the phases are separate facts.
    fn busy(state: &SessionState) -> bool {
        state.forest.main_turn() == Some(&TurnPhase::Running)
    }

    /// Everything a session records, which is now only the root entry's
    /// turn phase: a turn beginning, ending, failing or being interrupted is
    /// that entry's own state, and what the turn actually carried belongs to
    /// the agent that ran it.
    ///
    /// Pure, and an associated function rather than a method: replay runs with
    /// no instance in scope, which is what makes a recovered session and a live
    /// one follow the same path.
    // The fallthrough is unreachable by construction:
    // `SessionActor::apply_event` matches every variant explicitly and routes
    // each to exactly one component, so a newly added event fails to compile
    // *there* — which is where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::TurnBegan { agent, .. } => {
                state.forest.apply_turn_began(agent);
            }
            // Routed by the owning entry: the main session parks its turn,
            // a workflow step's ask parks its run.
            SessionDomainEvent::AskRecorded { agent, .. } => {
                state.forest.apply_asked(agent);
            }
            SessionDomainEvent::TurnEnded { agent, .. }
            | SessionDomainEvent::TurnStopped { agent, .. }
            | SessionDomainEvent::TurnInterrupted { agent, .. } => {
                state.forest.apply_turn_idle(agent);
            }
            SessionDomainEvent::TurnFailed { agent, error, .. } => {
                state.forest.apply_turn_failed(agent, error);
            }
            SessionDomainEvent::SessionFailed { reason, .. } => {
                state.fatal = Some(reason);
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
    //! The session's half of a session: what its status does as turns
    //! begin and end, what it refuses, and what a person acting releases.
    //!
    //! The queue's own rules — what merges, what waits out a park, what an
    //! answer must cover — belong to the agent that holds the queue and are
    //! tested in `crate::agent_loop::inbox`. What is left here is what the
    //! *session* still owns.
    use super::super::testing::*;
    use super::super::*;
    use super::*;
    use crate::sessions::session_actor::testing::seed_session;

    use std::sync::Arc;
    use uuid::Uuid;

    use crate::sessions::spec::SessionStatus;

    /// The event that roots the forest: every turn fold resolves its agent
    /// through it now.
    fn rooted(session: Uuid) -> SessionDomainEvent {
        SessionDomainEvent::SpecRecorded {
            at_ms: 0,
            session,
            spec: Box::new(crate::sessions::spec::SessionSpec::for_vendor("mock")),
        }
    }

    #[test]
    fn a_fresh_session_is_idle() {
        assert_eq!(SessionState::default().status(), SessionStatus::Idle);
    }

    /// The session learns a turn began because the agent tells it, and that is
    /// the whole of what it records: `Running`, and last turn's failure
    /// cleared.
    #[test]
    fn a_turn_beginning_clears_the_previous_failure() {
        let session = Uuid::new_v4();
        let s = fold(vec![
            rooted(session),
            SessionDomainEvent::TurnFailed {
                at_ms: 0,
                agent: session,
                error: "provider exploded".into(),
            },
            SessionDomainEvent::TurnBegan {
                at_ms: 1,
                agent: session,
            },
        ]);
        assert_eq!(s.status(), SessionStatus::Running);
        // The detail endpoint reports `last_error`, so a turn that has just
        // started must not still be advertising the previous turn's failure.
        assert_eq!(s.last_error(), None);
    }

    #[test]
    fn a_failed_turn_is_sticky_but_not_terminal() {
        let session = Uuid::new_v4();
        let s = fold(vec![
            rooted(session),
            SessionDomainEvent::TurnFailed {
                at_ms: 0,
                agent: session,
                error: "provider exploded".into(),
            },
        ]);
        assert!(matches!(s.status(), SessionStatus::Failed { .. }));
        assert_eq!(s.last_error().as_deref(), Some("provider exploded"));
    }

    #[test]
    fn stop_and_interrupt_both_land_idle() {
        let session = Uuid::new_v4();
        for boundary in [
            SessionDomainEvent::TurnStopped {
                at_ms: 1,
                agent: session,
            },
            SessionDomainEvent::TurnInterrupted {
                at_ms: 1,
                agent: session,
            },
        ] {
            let s = fold(vec![
                rooted(session),
                SessionDomainEvent::TurnBegan {
                    at_ms: 0,
                    agent: session,
                },
                boundary,
            ]);
            assert_eq!(s.status(), SessionStatus::Idle);
        }
    }

    /// A park is a status and nothing more. Which questions are pending is the
    /// agent's own state — it is what asked them and what answers them — so the
    /// session carries none of them.
    #[test]
    fn an_ask_parks_the_session_without_carrying_the_questions() {
        let session = Uuid::new_v4();
        let s = fold(vec![
            rooted(session),
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                agent: session,
            },
        ]);
        assert_eq!(s.status(), SessionStatus::AwaitingInput);
        let next = SessionActor::apply_event(
            s,
            SessionDomainEvent::TurnBegan {
                at_ms: 1,
                agent: session,
            },
        );
        assert_eq!(next.status(), SessionStatus::Running);
    }

    /// A terminal session refuses a message outright rather than queueing one
    /// nothing will ever answer.
    #[tokio::test]
    async fn an_unrecoverable_session_refuses_a_message() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let journal = f.journal();
        let session = seed_session(
            &f,
            id,
            actor_spec_fixture(),
            &[SessionDomainEvent::SessionFailed {
                at_ms: 0,
                reason: "runtime gone".into(),
            }],
        )
        .await;

        let refuse = async || {
            session
                .ask(|reply| {
                    SessionCommand::Turn(TurnCommand::UserMessage {
                        agent_id: None,
                        text: "please".into(),
                        reply,
                        artifacts: Vec::new(),
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
                    artifacts: Vec::new(),
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
        f: &ActorFixture,
        id: Uuid,
        events: &[SessionDomainEvent],
    ) -> (SessionRef, Arc<dyn horsie_actor::Journal>) {
        (
            seed_session(f, id, actor_spec_fixture(), events).await,
            f.journal(),
        )
    }

    /// The main agent says the turn its process died inside is over, and the
    /// session records it — the genuine case the repair exists for.
    #[tokio::test]
    async fn a_reported_interruption_ends_the_turn() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        // A journal that ends mid-turn: exactly what a process killed during a
        // run leaves behind.
        let (session, journal) = load_from(
            &f,
            id,
            &[SessionDomainEvent::TurnBegan {
                at_ms: 0,
                agent: id,
            }],
        )
        .await;

        session
            .tell(SessionCommand::AgentOutcome(
                crate::agent_loop::AgentOutcome::Interrupted {
                    agent: id,
                    run_id: 0,
                },
            ))
            .await
            .unwrap();

        wait_for_state(&journal, id, "the interrupted turn to be recorded", |s| {
            s.status() == crate::sessions::spec::SessionStatus::Idle
        })
        .await;
    }

    #[tokio::test]
    async fn a_redelivered_agent_run_outcome_is_applied_once() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let (session, journal) = load_from(
            &f,
            id,
            &[SessionDomainEvent::TurnBegan {
                at_ms: 0,
                agent: id,
            }],
        )
        .await;

        session
            .tell(SessionCommand::AgentOutcome(
                crate::agent_loop::AgentOutcome::Interrupted {
                    agent: id,
                    run_id: 7,
                },
            ))
            .await
            .unwrap();
        wait_for_state(&journal, id, "first outcome", |state| {
            state.has_agent_run_outcome(id, 7)
        })
        .await;

        session
            .tell(SessionCommand::AgentOutcome(
                crate::agent_loop::AgentOutcome::Failed {
                    agent: id,
                    run_id: 7,
                    error: "duplicate must not win".into(),
                    recoverable: false,
                    terminal: false,
                },
            ))
            .await
            .unwrap();
        let (reply, rx) = oneshot::channel();
        session
            .tell(SessionCommand::Read(ReadCommand::Snapshot {
                reply: ReplyTo::from_sender(reply),
            }))
            .await
            .unwrap();
        assert_eq!(
            rx.await.unwrap().status,
            crate::sessions::spec::SessionStatus::Idle
        );

        let events = crate::sessions::events::session_events(&journal, id).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, SessionDomainEvent::AgentRunOutcomeRecorded { .. }))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            SessionDomainEvent::TurnFailed { error, .. } if error == "duplicate must not win"
        )));
    }

    /// A turn that failed before its loop began banks no boundary in the
    /// agent's journal, so the agent still calls it open and reports it at the
    /// next load. The session was told directly and has already recorded
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
            &f,
            id,
            &[
                SessionDomainEvent::TurnBegan {
                    at_ms: 0,
                    agent: id,
                },
                SessionDomainEvent::TurnFailed {
                    at_ms: 1,
                    agent: id,
                    error: "provider said no".into(),
                },
            ],
        )
        .await;

        session
            .tell(SessionCommand::AgentOutcome(
                crate::agent_loop::AgentOutcome::Interrupted {
                    agent: id,
                    run_id: 0,
                },
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
            crate::sessions::spec::SessionStatus::Failed {
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
                    artifacts: Vec::new(),
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
                    artifacts: Vec::new(),
                })
            })
            .await
            .unwrap()
            .expect("an accepted message")
            .message_id;
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
            if state.status() == crate::sessions::spec::SessionStatus::AwaitingInput {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A subagent completes while the session is AwaitingInput.
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |t| {
            t.sub(sub)
                .is_some_and(|r| r.status == SubAgentStatus::Completed && r.notified)
        })
        .await;
        // Delivered into the agent's queue, but the park holds: a report has no
        // opinion about the question, so it waits rather than overriding it.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.status(),
            crate::sessions::spec::SessionStatus::AwaitingInput,
            "a report must not answer the question for the user"
        );

        // The user's reply is what releases it, and carries the report along.
        send(&session, "the first one").await;
        wait_for_tree(&journal, id, |t| t.sub(sub).is_some_and(|r| r.notified)).await;
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
                    | horsie_agentcore::ContentPart::SubAgentResult(_)
                    | horsie_agentcore::ContentPart::Artifact(_) => {}
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
                    artifacts: Vec::new(),
                })
            })
            .await
            .unwrap()
            .expect("a subagent takes messages like any other agent")
            .message_id;

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
                    artifacts: Vec::new(),
                })
            })
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, UserMessageError::NotFound), "{err:?}");
    }

    /// Say something to the session's main agent.
    async fn send(session: &SessionRef, text: &str) {
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: text.into(),
                    artifacts: Vec::new(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("the session takes messages");
    }

    /// Poll the project's inbox until `pred` holds, then answer with the page.
    ///
    /// Polled because every inbox write is spawned off the session's mailbox:
    /// the row lands shortly after the state it describes, never with it.
    /// Reading once passes on an idle machine and fails under load, which is
    /// the worst of both.
    async fn wait_for_inbox(
        f: &ActorFixture,
        why: &str,
        pred: impl Fn(&crate::user_inbox::InboxPage) -> bool,
    ) -> crate::user_inbox::InboxPage {
        let inbox = f.node.services().await.user_inbox.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let page = inbox
                .list(&crate::user_inbox::InboxFilter::default())
                .await
                .unwrap();
            if pred(&page) {
                return page;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {why}; inbox held {:?}",
                page.messages
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The whole point of the feature: a question that stops an agent has to be
    /// visible somewhere other than inside the session it stopped.
    ///
    /// It also pins the *address*. An inbox row names its agent the way a route
    /// does — `main`, not the session's uuid — and a row carrying the uuid
    /// would still list, still render, and open a page that does not exist.
    #[tokio::test]
    async fn a_parked_question_reaches_the_inbox_addressed_as_a_route_would_be() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(asks("which database?"))])
                .then_repeating_with(|| Ok(concludes(serde_json::json!("done")))),
        );
        let (f, session, id, _journal) = spawn_session_with_provider(provider).await;
        send(&session, "get on with it").await;

        let page = wait_for_inbox(&f, "the question to reach the inbox", |p| {
            !p.messages.is_empty()
        })
        .await;
        let [message] = page.messages.as_slice() else {
            panic!("one question, one message: {:?}", page.messages);
        };
        assert!(message.is_ask());
        assert!(message.is_open(), "the agent is stopped on it");
        assert_eq!(message.body, "which database?");
        assert_eq!(message.session_id, id.to_string());
        assert_eq!(
            message.agent_id, MAIN_AGENT_ID,
            "an inbox row is an address, and `?aid=` spells the main agent `main`"
        );
        assert_eq!(message.tool_call_id.as_deref(), Some(ASK_CALL_ID));
        assert_eq!(page.open_asks, 1, "one agent has stopped");
    }

    /// A person who types a new message instead of answering. The agent is
    /// resumed with a "not answered" result and gets on with it — so the inbox
    /// must stop offering to answer a question that no longer holds anything.
    ///
    /// This is the one resolution path nothing else names: no answer command is
    /// ever sent, so only the projection watching the agent leave
    /// `AwaitingInput` can see it happen.
    #[tokio::test]
    async fn abandoning_a_question_settles_its_inbox_row() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(asks("which database?"))])
                .then_repeating_with(|| Ok(concludes(serde_json::json!("done")))),
        );
        let (f, session, _id, _journal) = spawn_session_with_provider(provider).await;
        send(&session, "get on with it").await;
        wait_for_inbox(&f, "the question to reach the inbox", |p| p.open_asks == 1).await;

        // Not an answer. A plain message abandons the park.
        send(&session, "never mind, do something else").await;

        // Waited on the row rather than on `open_asks`: a page's rows and its
        // counts are three separate reads, so a snapshot taken across a write
        // can hold a row this has not seen settle yet. The row is the fact; the
        // counts are a badge that catches up.
        let page = wait_for_inbox(&f, "the abandoned question to settle", |p| {
            p.messages
                .first()
                .is_some_and(|m| m.state == horsie_models::inbox::InboxState::Closed)
        })
        .await;
        assert_eq!(
            page.messages.len(),
            1,
            "abandoning settles the question, it does not add another"
        );
    }

    /// Answering — from the session page or anywhere else — must read as
    /// *answered*, not merely as a row that stopped being open.
    ///
    /// The two writers race: the projection sees the agent resume and closes,
    /// while the answer handler records what the answer was. This drives them
    /// in the order that used to lose, with the close landing first.
    #[tokio::test]
    async fn answering_marks_the_row_answered_even_after_the_close_lands() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(
            Script::of([Ok(asks("which database?"))])
                .then_repeating_with(|| Ok(concludes(serde_json::json!("done")))),
        );
        let (f, session, id, _journal) = spawn_session_with_provider(provider).await;
        send(&session, "get on with it").await;
        wait_for_inbox(&f, "the question to reach the inbox", |p| p.open_asks == 1).await;

        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::Answer {
                    agent_id: None,
                    answers: vec![answer(ASK_CALL_ID, "postgres")],
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("the parked question is answerable");

        // The projection's close is what the answer has to survive, so wait it
        // out before recording the answer — the ordering the HTTP layer cannot
        // control, and the one that used to leave the row reading "closed".
        wait_for_inbox(&f, "the projection to close the row", |p| {
            p.messages.first().is_some_and(|m| !m.is_open())
        })
        .await;
        let inbox = f.node.services().await.user_inbox.clone();
        inbox
            .settle_agent_asks(
                &id.to_string(),
                MAIN_AGENT_ID,
                &[ASK_CALL_ID.to_string()],
                horsie_models::inbox::InboxState::Answered,
                crate::user_inbox::now_ms_i64(),
            )
            .await
            .unwrap();

        let page = inbox
            .list(&crate::user_inbox::InboxFilter::default())
            .await
            .unwrap();
        assert_eq!(
            page.messages[0].state,
            horsie_models::inbox::InboxState::Answered,
            "a question answered in the session page must not read as merely closed"
        );
    }
}
