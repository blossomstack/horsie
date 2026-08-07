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
    TurnCommand, TurnEnd,
};
use super::{AnswerError, AskAnswer};
use crate::sessions::UserMessageError;
use crate::sessions::spec::PendingAsk;
use crate::sessions::spec::SessionStatus;
use horsie_actor::ActorContext;
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
                actor.cancel_in_flight(state).await;
                let _ = reply.send(());
                actor.report(SessionStatus::Idle).await;
                // Stop is a turn boundary like any other, so anything the user
                // queued while the cancelled turn ran starts the next one.
                let stopped = vec![SessionDomainEvent::TurnStopped { at_ms: now_ms() }];
                actor.persist_and_advance(state, stopped, ctx).await
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
    /// What the main agent's turn ending means for the session.
    ///
    /// Three of the four are turn boundaries that let the inbox drain; a failure
    /// deliberately is not. Lives here rather than in the actor's routing
    /// because "the turn is over" is this component's fact — the same outcome
    /// means something else entirely to a step or a subagent.
    pub(super) async fn on_main_outcome(
        &mut self,
        state: &SessionState,
        end: TurnEnd,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<SessionDomainEvent> {
        let (events, drains) = match end {
            TurnEnd::Concluded { .. } => {
                self.report(SessionStatus::Idle).await;
                (
                    vec![SessionDomainEvent::TurnEnded { at_ms: now_ms() }],
                    true,
                )
            }
            TurnEnd::Asked { asks } => {
                self.report(SessionStatus::AwaitingInput {
                    asks: asks
                        .iter()
                        .map(|a| PendingAsk {
                            tool_call_id: a.tool_call_id.clone(),
                            question: a.question.clone(),
                        })
                        .collect(),
                })
                .await;
                (
                    asks.into_iter()
                        .map(|a| SessionDomainEvent::AskRecorded {
                            at_ms: now_ms(),
                            tool_call_id: a.tool_call_id,
                            question: a.question,
                        })
                        .collect::<Vec<_>>(),
                    // An ask is a turn boundary too: a message queued while the
                    // agent was working becomes the answer.
                    true,
                )
            }
            // A runtime that a live vendor cannot produce is the one terminal
            // failure: re-provisioning would silently rebuild a workspace the
            // user believes they still have. Everything else — provider errors,
            // tool errors, a vendor that is merely offline — is a failed turn
            // they can retry.
            TurnEnd::Failed {
                error,
                terminal: true,
            } => {
                self.report(SessionStatus::Unrecoverable {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::SessionFailed {
                        at_ms: now_ms(),
                        reason: error,
                    }],
                    false,
                )
            }
            // Deliberately no drain: a stuck cause (expired key, dead vendor)
            // would otherwise turn three queued messages into three back-to-back
            // failures. The next message drains them.
            TurnEnd::Failed {
                error,
                terminal: false,
            } => {
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::TurnFailed {
                        at_ms: now_ms(),
                        error,
                    }],
                    false,
                )
            }
            TurnEnd::Parked => {
                let error = "agent parked; timers are not supported in sessions".to_string();
                self.report(SessionStatus::Failed {
                    reason: error.clone(),
                })
                .await;
                (
                    vec![SessionDomainEvent::TurnFailed {
                        at_ms: now_ms(),
                        error,
                    }],
                    false,
                )
            }
        };
        match drains {
            true => self.persist_and_advance(state, events, ctx).await,
            false => CommandEffect::persist(events),
        }
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
        // An unnamed session is titled from its first message, once. The rule is
        // `SessionCore`'s — a session's name is its own bookkeeping, not the
        // turn's — so this only says when to apply it.
        self.title_from_first_message(&text).await;

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

        // A session whose create failed has no runtime, so the message that the
        // UI invited ("send a message to try again") has to build one rather
        // than start a turn that would ask for it. The message stays queued and
        // the create's own completion drains it, exactly as at session creation.
        if matches!(state.status, SessionStatus::ProvisioningFailed { .. }) {
            let _ = ctx
                .self_ref()
                .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision))
                .await;
            return CommandEffect::persist(vec![queued]);
        }
        self.persist_and_advance(state, vec![queued], ctx).await
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    //! The conversation: what a queued message does, what a turn consumes,
    //! and what answering does and does not accept.
    use super::super::testing::*;
    use super::super::*;
    use super::*;
    use crate::sessions::orchestrator::MERGE_SEPARATOR;

    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn a_fresh_session_is_idle_with_an_empty_inbox() {
        let s = SessionState::default();
        assert_eq!(s.status, SessionStatus::Idle);
        assert!(s.inbox.is_empty());
    }

    #[test]
    fn queued_messages_accumulate_without_changing_status() {
        let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        assert_eq!(s.status, SessionStatus::Idle, "queueing is not running");
        assert_eq!(s.inbox.len(), 2);
    }

    #[test]
    fn a_turn_consumes_exactly_the_messages_it_names() {
        let s = fold(vec![
            queued("m1", "one"),
            queued("m2", "two"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m3", "three"),
        ]);
        assert_eq!(s.status, SessionStatus::Running);
        let ids: Vec<&str> = s.inbox.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["m2", "m3"],
            "a message that arrived after the turn began must still be owed an answer"
        );
    }

    #[test]
    fn a_turn_that_answers_an_ask_clears_it() {
        // `answering` is how turns before multi-ask recorded it; a journal
        // written then must still fold to the same place.
        let s = fold(vec![
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-1".into()),
                question: "which branch?".into(),
            },
            queued("m1", "main"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: Some("call-1".into()),
                answered: Vec::new(),
            },
        ]);
        assert_eq!(s.status, SessionStatus::Running);
        assert!(s.pending_asks.is_empty(), "the ask was answered");
    }

    #[test]
    fn two_asks_in_one_turn_are_both_pending_until_a_turn_begins() {
        let asked = |id: &str, q: &str| SessionDomainEvent::AskRecorded {
            at_ms: 0,
            tool_call_id: Some(id.to_string()),
            question: q.to_string(),
        };
        let s = fold(vec![
            asked("call-1", "which branch?"),
            asked("call-2", "which model?"),
        ]);
        let SessionStatus::AwaitingInput { asks } = &s.status else {
            panic!("expected AwaitingInput, got {:?}", s.status);
        };
        assert_eq!(asks.len(), 2, "the status carries what must be answered");
        assert_eq!(asks[0].question, "which branch?");
        assert_eq!(asks[1].question, "which model?");
        assert_eq!(s.pending_asks.len(), 2);

        // Answered together, or abandoned together — either way the turn that
        // begins is the end of the park.
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: Vec::new(),
                answering: None,
                answered: vec!["call-1".into(), "call-2".into()],
            },
        );
        assert_eq!(s.status, SessionStatus::Running);
        assert!(s.pending_asks.is_empty());
    }

    #[test]
    fn an_ask_survives_a_crash_so_the_answer_is_not_re_asked() {
        // TurnBegan is what clears the ask, and it is journaled with the
        // consumption in one step: a crash before it replays to "still asking".
        let s = fold(vec![
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-1".into()),
                question: "which branch?".into(),
            },
            queued("m1", "main"),
        ]);
        assert!(matches!(s.status, SessionStatus::AwaitingInput { .. }));
        assert_eq!(
            s.pending_asks
                .first()
                .and_then(|a| a.tool_call_id.as_deref()),
            Some("call-1")
        );
        assert_eq!(s.inbox.len(), 1, "the answer is still owed");
    }

    #[test]
    fn stop_and_interrupt_both_land_idle_and_keep_the_inbox() {
        for boundary in [
            SessionDomainEvent::TurnStopped { at_ms: 0 },
            SessionDomainEvent::TurnInterrupted { at_ms: 0 },
        ] {
            let s = fold(vec![
                queued("m1", "one"),
                SessionDomainEvent::TurnBegan {
                    at_ms: 0,
                    consumed: vec!["m1".into()],
                    answering: None,
                    answered: Vec::new(),
                },
                queued("m2", "queued while running"),
                boundary,
            ]);
            assert_eq!(s.status, SessionStatus::Idle);
            assert_eq!(
                s.inbox.len(),
                1,
                "an accepted message is a promise; a stop cancels the turn, not the promise"
            );
        }
    }

    #[test]
    fn a_failed_turn_is_sticky_but_not_terminal() {
        let s = fold(vec![
            queued("m1", "still owed an answer"),
            SessionDomainEvent::TurnFailed {
                at_ms: 0,
                error: "provider exploded".into(),
            },
        ]);
        assert!(matches!(s.status, SessionStatus::Failed { .. }));
        assert_eq!(s.last_error.as_deref(), Some("provider exploded"));
        assert_eq!(
            s.inbox.len(),
            1,
            "a turn that failed answered nothing; the queue is still owed"
        );

        // The next turn moves it straight back to Running.
        let s = SessionActor::apply_event(
            s,
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec![],
                answering: None,
                answered: Vec::new(),
            },
        );
        assert_eq!(s.status, SessionStatus::Running);
        // The detail endpoint reports `last_error`, so a turn that has just
        // started must not still be advertising the previous turn's failure.
        assert_eq!(s.last_error, None);
    }

    #[test]
    fn merging_joins_in_arrival_order_with_a_blank_line() {
        let s = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        let merged = s
            .inbox
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>()
            .join(MERGE_SEPARATOR);
        assert_eq!(merged, "one\n\ntwo");
    }

    #[tokio::test]
    async fn drain_does_nothing_when_the_inbox_is_empty() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        );
        let actions = decisions(&actor, &SessionState::default());
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn drain_does_nothing_while_a_turn_is_already_running() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        );
        let state = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m2", "queued while running"),
        ]);
        let actions = decisions(&actor, &state);
        assert!(
            actions.is_empty(),
            "a run in flight must never be drained into a second one"
        );
    }

    #[tokio::test]
    async fn drain_refuses_once_the_session_is_unrecoverable() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        );
        let state = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::SessionFailed {
                at_ms: 0,
                reason: "runtime gone".into(),
            },
        ]);
        let actions = decisions(&actor, &state);
        assert!(
            actions.is_empty(),
            "a terminal session must never start another turn"
        );
    }

    /// A failed turn is a turn boundary that deliberately does *not* drain. The
    /// cause is usually stuck — an expired key, a dead vendor — and draining
    /// would turn three queued messages into three back-to-back failures the
    /// user never asked for. The next message they send drains them.
    #[tokio::test]
    async fn a_failed_turn_does_not_drain() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let id = Uuid::new_v4();
        let journal: Arc<dyn horsie_actor::Journal> =
            Arc::new(horsie_actor::InMemoryJournal::new());
        // A turn is running, and a message arrived while it was.
        let prior = [
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m2", "queued while running"),
        ];
        let bytes: Vec<Vec<u8>> = prior
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap())
            .collect();
        journal
            .persist(&SessionActor::persistence_id_for(id), &bytes)
            .await
            .unwrap();
        let session = horsie_actor::spawn_root(
            SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps,
                parent,
                crate::sessions::Positions::default(),
            ),
            journal.clone(),
        );
        // Recovery reconciles the interrupted turn first (event 4); wait for
        // that to settle so the failure is the only thing left to observe.
        wait_for_journal_len(&journal, id, 4).await;

        session
            .tell(SessionCommand::AgentOutcome(AgentOutcome::Failed {
                session_id: id,
                error: "provider exploded".into(),
                recoverable: true,
                terminal: false,
            }))
            .await
            .unwrap();

        // The failure lands (event 5) — and nothing follows: no drain into a
        // back-to-back failure.
        wait_for_journal_len(&journal, id, 5).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            session_journal_len(&journal, id).await,
            5,
            "a failed turn records the failure and nothing else"
        );
        // Asked of the actor, which is the only thing that reads this journal.
        let snapshot = session
            .ask(|reply| SessionCommand::Read(ReadCommand::Snapshot { reply }))
            .await
            .unwrap();
        assert!(matches!(
            snapshot.status,
            crate::sessions::spec::SessionStatus::Failed { .. }
        ));
        assert_eq!(snapshot.inbox.len(), 1, "the queued message is still owed");
    }

    /// Stop is a turn boundary like any other: it cancels the turn, not the
    /// promise. Whatever was queued while the cancelled turn ran starts the
    /// next one immediately — which is exactly why the client marks queued
    /// messages as unread, so that next turn does not look self-inflicted.
    #[tokio::test]
    async fn stop_then_a_queued_message_starts_the_next_turn() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        );
        let running = fold(vec![
            queued("m1", "one"),
            SessionDomainEvent::TurnBegan {
                at_ms: 0,
                consumed: vec!["m1".into()],
                answering: None,
                answered: Vec::new(),
            },
            queued("m2", "queued while running"),
        ]);

        let stopped =
            SessionActor::apply_event(running, SessionDomainEvent::TurnStopped { at_ms: 0 });
        assert_eq!(stopped.status, SessionStatus::Idle);
        let actions = decisions(&actor, &stopped);
        assert_eq!(actions.len(), 1, "{actions:?}");
        let AgentAction::StartTurn { consumed, .. } = &actions[0] else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(consumed, &vec!["m2".to_string()]);
    }

    #[tokio::test]
    async fn drain_consumes_the_whole_inbox_and_starts_a_turn() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        );
        let state = fold(vec![queued("m1", "one"), queued("m2", "two")]);
        let actions = decisions(&actor, &state);
        assert_eq!(actions.len(), 1);
        let AgentAction::StartTurn { consumed, .. } = &actions[0] else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(consumed, &vec!["m1".to_string(), "m2".to_string()]);
    }

    #[tokio::test]
    async fn drain_abandons_pending_asks_rather_than_answering_them() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        );
        let state = fold(vec![
            SessionDomainEvent::AskRecorded {
                at_ms: 0,
                tool_call_id: Some("call-1".into()),
                question: "which?".into(),
            },
            queued("m1", "main"),
        ]);
        let actions = decisions(&actor, &state);
        assert_eq!(actions.len(), 1);
        let AgentAction::StartTurn {
            consumed,
            answered,
            input,
            ..
        } = &actions[0]
        else {
            panic!("an interactive session starts turns, not steps");
        };
        assert_eq!(consumed, &vec!["m1".to_string()]);
        assert_eq!(
            input.results.len(),
            1,
            "the parked call still gets a result"
        );
        assert!(input.results[0].is_error);
        assert!(
            answered.is_empty(),
            "a plain message abandons the question rather than answering it — \
             answers come through `Answer`, which requires all of them at once"
        );
    }

    #[tokio::test]
    async fn a_partial_answer_set_is_refused_and_journals_nothing() {
        // Resuming on half the answers would send the provider a `tool_use` with
        // no result, which is exactly the 400 this whole change exists to stop.
        let (mut actor, state) = parked_on_two_asks().await;
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(&state, vec![answer("call-1", "main")], tx)
            .await;

        assert!(
            effect.events().is_empty(),
            "a refused answer set changes nothing"
        );
        match rx.await.unwrap() {
            Err(AnswerError::Incomplete {
                missing,
                unexpected,
            }) => {
                assert_eq!(missing, vec!["call-2".to_string()]);
                assert!(unexpected.is_empty());
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_answer_for_a_call_that_is_not_pending_is_refused() {
        let (mut actor, state) = parked_on_two_asks().await;
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(
                &state,
                vec![
                    answer("call-1", "main"),
                    answer("call-2", "kimi"),
                    answer("call-9", "who asked?"),
                ],
                tx,
            )
            .await;

        assert!(effect.events().is_empty());
        match rx.await.unwrap() {
            Err(AnswerError::Incomplete { unexpected, .. }) => {
                assert_eq!(unexpected, vec!["call-9".to_string()]);
            }
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_complete_answer_set_begins_a_turn_naming_every_ask() {
        let (mut actor, state) = parked_on_two_asks().await;
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(
                &state,
                vec![answer("call-1", "main"), answer("call-2", "kimi")],
                tx,
            )
            .await;

        assert!(rx.await.unwrap().is_ok());
        let events = effect.events();
        assert_eq!(events.len(), 1);
        let SessionDomainEvent::TurnBegan {
            consumed, answered, ..
        } = &events[0]
        else {
            panic!("expected TurnBegan, got {:?}", events[0]);
        };
        assert!(consumed.is_empty(), "an answer consumes no queued message");
        let mut answered = answered.clone();
        answered.sort();
        assert_eq!(answered, vec!["call-1".to_string(), "call-2".to_string()]);

        // And the park is over: folding the event clears every pending ask.
        let next = SessionActor::apply_event(state, events[0].clone());
        assert!(next.pending_asks.is_empty());
        assert_eq!(next.status, SessionStatus::Running);
    }

    #[tokio::test]
    async fn answering_a_session_that_is_not_parked_is_refused() {
        let f = actor_fixture().await;
        let parent = spawn_deaf_supervisor();
        let mut actor = SessionActor::new(
            Uuid::new_v4(),
            actor_spec_fixture(),
            f.deps,
            parent,
            crate::sessions::Positions::default(),
        );
        let (tx, rx) = oneshot::channel();

        let effect = actor
            .on_answer(&SessionState::default(), vec![answer("call-1", "main")], tx)
            .await;

        assert!(effect.events().is_empty());
        assert_eq!(rx.await.unwrap(), Err(AnswerError::NothingPending));
    }

    /// A run works from its definition; there is nobody to send a message to.
    #[tokio::test]
    async fn a_run_refuses_a_user_message() {
        use horsie_agentcore::testkit::{MockProvider, Script};
        let provider = MockProvider::scripted(Script::of([Ok(concludes(
            serde_json::json!({"severity": "p0"}),
        ))]));
        let (_f, session, _id, _journal) = spawn_run_with_provider(provider).await;
        let err = session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    text: "hello".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, UserMessageError::Rejected(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_notification_waits_out_an_awaiting_input_session() {
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
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    text: "start".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        for _ in 0..200 {
            let state = crate::sessions::events::fold_session_state(&journal, id).await;
            if matches!(
                state.status,
                crate::sessions::spec::SessionStatus::AwaitingInput { .. }
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // A subagent completes while the session is AwaitingInput.
        let sub = spawn_sub(&session, "research", "dig").await;
        wait_for_tree(&journal, id, |t| {
            t.node(sub).is_some_and(|r| {
                r.status == crate::sessions::subagents::SubAgentStatus::Completed && !r.notified
            })
        })
        .await;
        // The ask is still pending — the notification must not have answered it.
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert!(matches!(
            state.status,
            crate::sessions::spec::SessionStatus::AwaitingInput { .. }
        ));

        // The user's reply carries the notification along in the same input.
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    text: "the first one".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_tree(&journal, id, |t| t.node(sub).is_some_and(|r| r.notified)).await;
        // A plain message does not answer the question — it abandons it and
        // starts a fresh turn — so the reply and the notification ride in the
        // *user message*, while the abandoned ask gets a result of its own.
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
        // One turn, two kinds of content: the person's words stay the user
        // text, the subagent's report rides alongside as its own part.
        assert!(
            texts.iter().any(|t| t.contains("the first one")),
            "the user's own message must survive the turn: {texts:?}"
        );
        let reports = subagent_texts(&main_history(&session).await);
        assert!(
            reports
                .iter()
                .any(|t| t.contains("[subagent \"research\" completed]")),
            "the notification rides the same turn: {reports:?}"
        );
        assert!(
            results.iter().any(|r| r.contains("not answered")),
            "the abandoned ask still gets a result, so nothing dangles: {results:?}"
        );
    }
}
