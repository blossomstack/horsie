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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    //! The session's half of a conversation: what it refuses, what a message
    //! reaches, and what a person acting releases.
    //!
    //! The queue's own rules — what merges, what waits out a park, what an
    //! answer must cover — belong to the agent that holds the queue and are
    //! tested in `crate::agent_loop::inbox`. What is left here is what the
    //! *session* still owns.
    use super::super::testing::*;
    use super::super::*;
    use crate::sessions::UserMessageError;
    use crate::sessions::session_actor::testing::seed_session;
    use crate::sessions::spec::SessionStatus;
    use horsie_actor::ReplyTo;
    use tokio::sync::oneshot;

    use std::sync::Arc;
    use uuid::Uuid;

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
            &[SessionEvent::SessionFailed {
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
                    })
                })
                .await
                .unwrap()
                .unwrap_err()
        };

        // Refuse once to settle the log, then refuse again and assert that
        // second one added nothing.
        let _ = refuse().await;
        let settled = session_journal_len(&journal, id).await;

        let err = refuse().await;
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
        events: &[SessionEvent],
    ) -> (crate::sessions::addressing::SessionRef, Arc<dyn horsie_actor::Journal>) {
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
            &[SessionEvent::TurnBegan {
                at_ms: 0,
                agent: AgentId(id),
            }],
        )
        .await;

        session
            .tell(SessionCommand::AgentOutcome(
                crate::agent_loop::AgentOutcome::Interrupted { agent: id },
            ))
            .await
            .unwrap();

        wait_for_state(&journal, id, "the interrupted turn to be recorded", |s| {
            s.status() == SessionStatus::Idle
        })
        .await;
    }

    /// A turn that failed before its loop began banks no boundary in the
    /// agent's journal, so the agent still calls it open and reports it at the
    /// next load. The runner owns the merged phase, so the runner decides: a
    /// report about anything but a live turn is history already written.
    #[tokio::test]
    async fn a_reported_interruption_leaves_a_failed_turn_alone() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        let (session, journal) = load_from(
            &f,
            id,
            &[
                SessionEvent::TurnBegan {
                    at_ms: 0,
                    agent: AgentId(id),
                },
                SessionEvent::TurnEnded {
                    at_ms: 1,
                    agent: AgentId(id),
                    end: RecordedEnd::Failed {
                        error: "provider said no".into(),
                    },
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

        // Nothing to wait *for*, so wait on something the same mailbox
        // answers after it: a read replies only once the outcome ahead of it
        // is done.
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
            !events.iter().any(|e| matches!(
                e,
                SessionEvent::TurnEnded {
                    end: RecordedEnd::Interrupted,
                    ..
                }
            )),
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
            .expect("an accepted message")
            .message_id;
        assert!(!accepted.is_empty(), "the caller is given the message id");
        // The id names an entry in the agent's own log, which is where the
        // queue lives.
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
        wait_for_state(&journal, id, "the session parks", |s| {
            s.status() == SessionStatus::AwaitingInput
        })
        .await;

        // A subagent completes while the session is AwaitingInput.
        let sub = spawn_sub(&session, id, "research", "dig").await;
        wait_for_tree(&journal, id, |s| sub_notified(s, sub)).await;
        // Delivered into the agent's queue, but the park holds: a report has
        // no opinion about the question, so it waits rather than overriding
        // it.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.status(),
            SessionStatus::AwaitingInput,
            "a report must not answer the question for the user"
        );

        // The user's reply is what releases it, and carries the report along.
        send(&session, "the first one").await;
        let _ = wait_for_subagent_text(&session, |texts| {
            texts
                .iter()
                .any(|t| t.contains("[subagent \"research\" completed]"))
        })
        .await;
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

    /// A message names an *agent*, and a subagent is an agent. It lands in
    /// that agent's queue and its log — never in the main agent's.
    #[tokio::test]
    async fn a_message_can_name_a_subagent() {
        let (_f, session, id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, id, "research", "dig").await;

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
                })
            })
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, UserMessageError::NotFound), "{err:?}");
    }
}
