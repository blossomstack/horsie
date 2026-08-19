//! The conversation surface: what a person sends, stops and answers.
//!
//! The session does not hold the message and does not decide the turn. A
//! message is addressed to an *agent* — once it can name a subagent or a
//! workflow step, a session-level queue has nowhere to put it — so this
//! resolves the agent, forwards the message, and lets the agent's own durable
//! write be what the caller waits on.
//!
//! What is left here is the session's own half: an unnamed session takes its
//! title from its first message, a terminal session refuses one, and a stop is
//! journaled in whatever vocabulary the owning runner answers with.

use super::runner::state::RunnerState;
use super::runner::{Runner, RunnerBehavior};
use super::{
    AgentId, AnswerError, AskAnswer, CommandEffect, ForkCommand, LifecycleCommand, MAIN_AGENT_ID,
    MessageAccepted, SessionActor, SessionCommand, SessionEvent, SessionState, TurnCommand,
};
use crate::agent_loop::{AgentCommand, Incoming};
use crate::sessions::UserMessageError;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::forks::ForkMode;
use crate::sessions::spec::SessionKind;
use horsie_actor::{ActorContext, ReplyTo};
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

/// A recognised fork command: which of the two it was, and what the new
/// conversation is for.
struct ForkRequest {
    mode: ForkMode,
    /// The builtin's name, for a refusal that quotes what was typed.
    name: &'static str,
    message: String,
}

impl SessionActor {
    pub(super) async fn handle_turn(
        &mut self,
        state: &SessionState,
        cmd: TurnCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            TurnCommand::UserMessage {
                agent_id,
                text,
                reply,
            } => {
                self.on_user_message(state, agent_id, text, reply, ctx)
                    .await
            }
            TurnCommand::Stop { agent_id, reply } => {
                self.on_stop(state, &agent_id, reply, ctx).await
            }
            TurnCommand::Answer {
                agent_id,
                answers,
                reply,
            } => self.on_answer(state, agent_id, answers, reply, ctx).await,
        }
    }

    /// Cancel one agent's turn, and journal the boundary its runner uses.
    ///
    /// Resolution never spawns: waking a cold agent in order to stop it is
    /// work to achieve nothing. Whether the agent is doing anything is the
    /// owning runner's [`RunnerBehavior::stop_event`], which answers with the
    /// event to journal, so the gate and the record cannot disagree about what
    /// was stopped.
    async fn on_stop(
        &mut self,
        state: &SessionState,
        agent_id: &str,
        reply: ReplyTo<Result<(), String>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let Some(agent) = self.resolve_selector(state, Some(agent_id)) else {
            let _ = reply.send(Err(format!("no such agent: {agent_id}")));
            return CommandEffect::none();
        };
        let stopped = Runner::owner_of(agent, state)
            .and_then(|runner| runner.stop_event(state, agent, now_ms()));
        let Some(stopped) = stopped else {
            // Not working is not a failure. A client that pressed Stop as the
            // turn ended on its own has got what it asked for.
            let _ = reply.send(Ok(()));
            return CommandEffect::none();
        };
        self.cancel_agent(agent).await;
        let _ = reply.send(Ok(()));
        self.persist_and_advance(state, vec![stopped], ctx).await
    }

    /// Route a set of answers to the agent that asked.
    ///
    /// Pure routing, and the agent replies to the caller directly: it owns the
    /// questions, so it is the only thing that can tell a complete answer set
    /// from a partial one.
    async fn on_answer(
        &mut self,
        state: &SessionState,
        agent_id: Option<String>,
        answers: Vec<AskAnswer>,
        reply: ReplyTo<Result<(), AnswerError>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
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

    /// What a fork command asked for, if the text is one.
    fn fork_request(text: &str) -> Option<ForkRequest> {
        let (builtin, args) = horsie_support::plugin::commands::parse_invocation(text, '/')
            .and_then(|(name, args)| {
                horsie_support::plugin::builtins::builtin(name).map(|b| (b, args))
            })?;
        let mode = match builtin.name {
            "fork" => ForkMode::Copy,
            "summary-n-fork" => ForkMode::Summary,
            _ => return None,
        };
        Some(ForkRequest {
            mode,
            name: builtin.name,
            message: args.trim().to_string(),
        })
    }

    /// Hand a fork command to the fork machinery.
    ///
    /// Recognising one is this surface's job — it is where every built-in is
    /// caught, before the text can be treated as a prompt. *Creating* one is
    /// not: the fork runner's state belongs to the fork machinery, so this
    /// decides what was typed and forwards a command.
    fn start_fork(
        &mut self,
        state: &SessionState,
        agent: AgentId,
        req: ForkRequest,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        let ForkRequest {
            mode,
            name,
            message,
        } = req;
        if message.is_empty() {
            let _ = reply.send(Err(UserMessageError::Rejected(format!(
                "/{name} needs a message saying what the new conversation should do"
            ))));
            return CommandEffect::none();
        }
        // A workflow session has no conversation to branch: its steps are
        // chosen by the definition, and nobody talks to one. (It also has no
        // session settings a fork could run under.)
        if matches!(self.spec().kind, SessionKind::Workflow { .. }) {
            let _ = reply.send(Err(UserMessageError::Rejected(
                "a workflow run cannot be forked".to_string(),
            )));
            return CommandEffect::none();
        }
        // Only a conversation forks. A subagent's is delegated work and a
        // step's belongs to the run, so neither has a branch to take.
        let forkable = agent == self.self_agent()
            || matches!(
                state
                    .record(super::RunnerId::of_agent(agent))
                    .map(|r| &r.state),
                Some(RunnerState::Fork(_))
            );
        if !forkable {
            let _ = reply.send(Err(UserMessageError::Rejected(
                "only a conversation can be forked".to_string(),
            )));
            return CommandEffect::none();
        }
        // Off the mailbox: `Create` reads the source agent's log head and then
        // waits on its own write, and holding this mailbox across both would
        // stall every other agent in the session.
        let self_ref = self.me(ctx);
        tokio::spawn(async move {
            let created = self_ref
                .ask(|r| {
                    SessionCommand::Fork(ForkCommand::Create {
                        parent: agent,
                        mode,
                        message,
                        reply: r,
                    })
                })
                .await;
            let answer = match created {
                Ok(Ok(id)) => Ok(MessageAccepted {
                    message_id: id.to_string(),
                    forked_agent: Some(id.to_string()),
                }),
                Ok(Err(why)) => Err(UserMessageError::Rejected(why)),
                Err(e) => Err(UserMessageError::Rejected(format!("fork: {e}"))),
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
    async fn on_user_message(
        &mut self,
        state: &SessionState,
        agent_id: Option<String>,
        text: String,
        reply: ReplyTo<Result<MessageAccepted, UserMessageError>>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        if let Some(reason) = &state.fatal {
            let _ = reply.send(Err(UserMessageError::Unrecoverable(reason.clone())));
            return CommandEffect::none();
        }
        // A run works from its definition and has no main agent, so an
        // unaddressed message has nobody to reach. Naming a step is fine —
        // that agent exists and can be spoken to like any other.
        if self.spec().workflow_run().is_some()
            && agent_id.as_deref().unwrap_or(MAIN_AGENT_ID) == MAIN_AGENT_ID
        {
            let _ = reply.send(Err(UserMessageError::Rejected(
                "this session is a workflow run; name a step agent to message it".to_string(),
            )));
            return CommandEffect::none();
        }
        let Some((agent_key, agent)) = self.resolve_agent(state, ctx, agent_id.as_deref()) else {
            let _ = reply.send(Err(UserMessageError::NotFound));
            return CommandEffect::none();
        };
        // Resolved before the session is titled, because a fork command is not
        // a thing to name a session after: it says what the *new* conversation
        // should do.
        if let Some(req) = Self::fork_request(text.trim()) {
            return self.start_fork(state, agent_key, req, reply, ctx);
        }
        // An unnamed session is titled from its first message, once.
        self.title_from_first_message(&text).await;

        let id = Uuid::new_v4().to_string();
        // A built-in is resolved here, before anything treats the text as a
        // prompt: `/compact` asks the server to do something and must never
        // reach `expand_invocation`, a template, or the model.
        let item = match horsie_support::plugin::commands::parse_invocation(text.trim(), '/')
            .and_then(|(name, args)| {
                horsie_support::plugin::builtins::builtin(name).map(|b| (b, args))
            }) {
            Some((builtin, args)) if builtin.name == "compact" => Incoming::Compact {
                id: id.clone(),
                instructions: (!args.trim().is_empty()).then(|| args.trim().to_string()),
            },
            // Every other built-in, present and future. Reaching here means
            // the table names something this match does not handle, which is a
            // bug rather than a message.
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
                Ok(Ok(())) => Ok(MessageAccepted::queued(accepted)),
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

        // A session whose create failed has no runtime, so the message that
        // the UI invited ("send a message to try again") has to build one
        // rather than start a turn that would ask for it. The message waits in
        // the agent's queue and the create's own completion releases it.
        if matches!(
            state.provisioning.phase,
            super::runner::state::ProvisionPhase::Failed { .. }
        ) {
            let _ = self
                .me(ctx)
                .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
                .await;
        }
        // A person acting is the boundary that flushes results owed to
        // subagent parents. Those strand once every node is terminal — no
        // further subagent outcome will arrive to trigger the flush — so the
        // next thing the user does has to be what delivers them.
        self.persist_and_advance(state, Vec::new(), ctx).await
    }
}
