//! The session's forks: branching a conversation into a second one that a
//! person can talk to.
//!
//! A fork is not a subagent. It owes nobody a result, it has `ask_user`, and it
//! names itself — so it takes the main agent's toolbox layers and gets a roster
//! of its own rather than a node in the subagent forest. It is not a session
//! either: it shares the one runtime the session owns, under its own agent id,
//! which is the whole reason it is cheap.
//!
//! Persists a create *before* the fork's actor exists, exactly as a subagent
//! spawn does: a crash between the two replays as a fork still `Provisioning`,
//! which [`ForkedAgents::on_load`] re-seeds — strictly better than an untracked
//! agent.

use super::component::{ActionCx, Component};
use super::context::SessionAgentKind;
use super::{
    AgentKey, AgentPlan, AgentStatus, CommandEffect, ForkCommand, SessionActor, SessionCommand,
    SessionDomainEvent, SessionState, TurnEnd,
};
use crate::agent_loop::{AgentCommand, AgentState, Incoming};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::forks::{ForkMode, ForkParent};
use horsie_actor::{ActorContext, ActorRef, ReplyTo};
use horsie_agentcore::{
    ContentPart, EmptyOutcome, FailedOutcome, Message, Role, TextPart, TurnOutcome,
};
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

pub(super) struct ForkedAgents;

impl ForkedAgents {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: ForkCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            ForkCommand::Create {
                parent,
                mode,
                message,
                reply,
            } => {
                // The branch point, read before anything is written: where the
                // source's log stands right now is what this fork carries.
                let Some(source_seq) = actor.source_log_head(state, ctx, parent).await else {
                    let _ =
                        reply.send(Err("the conversation to fork is not available".to_string()));
                    return CommandEffect::none();
                };
                let id = Uuid::new_v4();
                let created = SessionDomainEvent::ForkCreated {
                    at_ms: now_ms(),
                    id,
                    parent,
                    source_seq,
                    mode,
                    message: message.clone(),
                };
                // Persist first, spawn second — see the module doc.
                let (tx, rx) = oneshot::channel();
                let self_ref = actor.me(ctx);
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "fork ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::Fork(ForkCommand::FinishCreate {
                            id,
                            reply,
                            persisted,
                        }))
                        .await;
                });
                CommandEffect::persist(vec![created]).and_ack(ReplyTo::from_sender(tx))
            }
            ForkCommand::FinishCreate {
                id,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist fork: {e}")));
                    return CommandEffect::none();
                }
                if actor.spawn_fork_actor(ctx, state, id).is_none() {
                    let _ = reply.send(Err("could not start the fork".to_string()));
                    return CommandEffect::none();
                }
                // The message is *not* enqueued here. It rides into the same
                // write as the seed, because a fork with a message and no
                // history drains it immediately and answers a conversation it
                // has not been given yet.
                actor.start_seeding(ctx, state, id);
                // The id travels now rather than when the seed lands: the
                // client redirects to a fork that is visibly building itself,
                // which is exactly what a newly created session does.
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            ForkCommand::Summarised { forks, result } => {
                for id in forks {
                    // Dropped rather than reported: a fork deleted while its
                    // summary was being taken is not a failure, it is the user
                    // having changed their mind.
                    if !state.forks.contains(id) {
                        continue;
                    }
                    match &result {
                        Ok(summary) => actor.finish_seeding(ctx, state, id, summary.clone()),
                        Err(error) => {
                            let _ = actor
                                .me(ctx)
                                .tell(SessionCommand::Fork(ForkCommand::SeedFailed {
                                    id,
                                    error: error.clone(),
                                }))
                                .await;
                        }
                    }
                }
                CommandEffect::none()
            }
            ForkCommand::Seeded { id } => {
                if !state.forks.contains(id) {
                    return CommandEffect::none();
                }
                // Through `persist_and_advance` rather than a bare persist: the
                // fork becoming ready is what releases the message queued
                // behind it, and that release is an action.
                actor
                    .persist_and_advance(
                        state,
                        vec![SessionDomainEvent::ForkSeeded {
                            at_ms: now_ms(),
                            id,
                        }],
                        ctx,
                    )
                    .await
            }
            ForkCommand::SeedFailed { id, error } => {
                if !state.forks.contains(id) {
                    return CommandEffect::none();
                }
                tracing::warn!(fork = %id, error, "seeding a fork failed");
                CommandEffect::persist(vec![SessionDomainEvent::ForkStatusChanged {
                    at_ms: now_ms(),
                    id,
                    status: AgentStatus::Failed,
                }])
            }
            ForkCommand::SetTitle { id, title, reply } => {
                let normalized = match crate::sessions::title_tool::normalize_session_title(&title)
                {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = reply.send(Err(e.to_string()));
                        return CommandEffect::none();
                    }
                };
                if !state.forks.contains(id) {
                    let _ = reply.send(Err(format!("no such fork: {id}")));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(normalized.clone()));
                CommandEffect::persist(vec![SessionDomainEvent::ForkTitled {
                    at_ms: now_ms(),
                    id,
                    name: normalized,
                }])
            }
            ForkCommand::Delete { id, reply } => {
                if !state.forks.contains(id) {
                    let _ = reply.send(Err(format!("no such fork: {id}")));
                    return CommandEffect::none();
                }
                actor.retire_fork_actor(id).await;
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionDomainEvent::ForkDeleted {
                    at_ms: now_ms(),
                    id,
                }])
            }
            ForkCommand::ReseedInterrupted => {
                for id in state.forks.seeding() {
                    // Spawning is what a fork needs to be seeded *into*: a
                    // session that reloaded has no resident agents at all.
                    if actor.spawn_fork_actor(ctx, state, id).is_none() {
                        tracing::warn!(fork = %id, "could not restart a fork to re-seed it");
                        continue;
                    }
                    actor.start_seeding(ctx, state, id);
                }
                CommandEffect::none()
            }
        }
    }
}

impl Component for ForkedAgents {
    /// A fork left `Provisioning` by a dead process. Nothing else can finish
    /// one: seeding is session-owned work with no journal of its own, unlike a
    /// turn, which the agent reports as interrupted from its own recovery.
    ///
    /// Safe to re-attempt for the reason [`RuntimeLifecycle`](super::lifecycle::RuntimeLifecycle)
    /// gives about its own case: `Provisioning` is precisely the state in which
    /// no turn has run.
    fn on_load(_cx: &ActionCx<'_>, state: &SessionState) -> Option<SessionCommand> {
        state
            .forks
            .has_seeding()
            .then_some(SessionCommand::Fork(ForkCommand::ReseedInterrupted))
    }

    /// A summariser call is provider time with nothing durable behind it.
    /// Unloading the session mid-seed loses it and leaves a fork that only a
    /// reload repairs.
    fn busy(state: &SessionState) -> bool {
        state.forks.has_seeding()
    }

    // The fallthrough is unreachable by construction: `SessionActor::apply_event`
    // matches every variant explicitly and routes each to exactly one component,
    // so a newly added event fails to compile *there* — which is where it should
    // be classified — rather than silently reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::ForkCreated {
                id,
                parent,
                source_seq,
                mode,
                message,
                at_ms,
            } => state
                .forks
                .apply_created(id, parent, source_seq, mode, message, at_ms),
            SessionDomainEvent::ForkSeeded { id, .. } => state.forks.apply_seeded(id),
            SessionDomainEvent::ForkTitled { id, name, .. } => state.forks.apply_titled(id, name),
            SessionDomainEvent::ForkStatusChanged { at_ms, id, status } => {
                state.forks.apply_status(id, status, at_ms);
            }
            // The status is derived from the outcome, never carried beside it:
            // a conversation that stopped working is idle unless the turn is
            // what broke, and a second field saying so is a second thing that
            // can disagree with the first.
            SessionDomainEvent::ForkTurnEnded { at_ms, id, outcome } => {
                let status = match outcome {
                    TurnOutcome::Failed(_) => AgentStatus::Failed,
                    TurnOutcome::Ended(_)
                    | TurnOutcome::Stopped(_)
                    | TurnOutcome::Interrupted(_) => AgentStatus::Idle,
                };
                state.forks.apply_status(id, status, at_ms);
            }
            SessionDomainEvent::ForkDeleted { id, .. } => state.forks.apply_deleted(id),
            other => unreachable!("ForkedAgents was handed {other:?}"),
        }
    }
}

/// Handlers that belong to this component but act on the actor's own fields —
/// the roster and the spawn helpers. An inherent `impl` in a child module sees
/// them, so moving the code needed no plumbing.
impl SessionActor {
    /// One of this session's forks finished a turn.
    ///
    /// The fork half of [`SessionActor::on_main_outcome`](super::SessionActor::on_main_outcome),
    /// and it answers the same five ends — a fork *is* a conversation, so there
    /// is no end the main agent can reach that a fork cannot. What differs is
    /// only the scope: these move the fork's roster entry and write into the
    /// fork's own log, never the session's status, because a fork working is
    /// not the session working.
    pub(super) async fn on_fork_outcome(
        &mut self,
        state: &SessionState,
        id: Uuid,
        end: TurnEnd,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let outcome = match end {
            TurnEnd::Concluded { .. } => TurnOutcome::Ended(EmptyOutcome {}),
            // Not a boundary: the turn is parked, not over, and the question is
            // journaled into the fork's log by the agent that asked it — the
            // same division the main agent's `AskRecorded` follows.
            TurnEnd::Asked => {
                return self
                    .persist_and_advance(
                        state,
                        vec![SessionDomainEvent::ForkStatusChanged {
                            at_ms: now_ms(),
                            id,
                            status: AgentStatus::AwaitingInput,
                        }],
                        ctx,
                    )
                    .await;
            }
            // Session-wide, exactly as it is for the main agent: forks share the
            // one runtime, so a runtime that cannot be rebuilt takes every
            // conversation in the session with it, not just this one.
            TurnEnd::Failed {
                error,
                terminal: true,
            } => {
                return self
                    .persist_and_advance(
                        state,
                        vec![SessionDomainEvent::SessionFailed {
                            at_ms: now_ms(),
                            reason: error,
                        }],
                        ctx,
                    )
                    .await;
            }
            TurnEnd::Failed {
                error,
                terminal: false,
            } => TurnOutcome::Failed(FailedOutcome { error }),
            TurnEnd::Parked => TurnOutcome::Failed(FailedOutcome {
                error: "agent parked; timers are not supported in sessions".to_string(),
            }),
            // Only from a fork this session still believes is running — the
            // same guard, for the same reason, as the main agent's: a report
            // about anything but a live turn is history already written.
            TurnEnd::Interrupted => {
                let running = state
                    .forks
                    .get(id)
                    .is_some_and(|rec| rec.status == AgentStatus::Running);
                if !running {
                    return CommandEffect::none();
                }
                TurnOutcome::Interrupted(EmptyOutcome {})
            }
        };
        self.persist_and_advance(
            state,
            vec![SessionDomainEvent::ForkTurnEnded {
                at_ms: now_ms(),
                id,
                outcome,
            }],
            ctx,
        )
        .await
    }

    /// Spawn one fork's actor.
    ///
    /// Takes the main agent's plan, because a fork is a conversation: the
    /// session's own settings, no declared output, and no handoff tool — it
    /// ends its turn with plain text like the agent it branched from.
    pub(super) fn spawn_fork_actor(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
    ) -> Option<ActorRef<AgentCommand>> {
        if let Some(resident) = self.agents.as_ref().and_then(|a| a.sub(id)) {
            return Some(resident.actor.clone());
        }
        // A fork runs under the agent session's own settings — forks exist only
        // under an agent session, so a run answers `None` here rather than
        // inventing settings nothing owns.
        let settings = self
            .effective_settings(state, AgentKey::Fork(id))
            .cloned()?;
        // A conversation, like the agent it branched from. `Assembly::fork` is
        // the whole difference, and it names which conversation
        // `set_session_title` renames.
        let equipment = crate::sessions::runners::assemble(
            crate::sessions::runners::RunnerKind::Conversation,
            &crate::sessions::runners::Assembly {
                settings: &settings,
                unattended: self.spec().is_unattended(),
                fork: Some(crate::sessions::runners::RunnerId(id)),
                agent_type: None,
            },
        );
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Fork(id),
                settings,
                equipment,
                agent_type: None,
            },
        )
        .map(|resident| resident.actor)
    }

    /// The agent a fork is being taken from, spawned if it is not resident.
    pub(super) fn fork_source(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        parent: ForkParent,
    ) -> Option<ActorRef<AgentCommand>> {
        match parent {
            ForkParent::Main => self.agent(),
            ForkParent::Fork(id) => self.spawn_fork_actor(ctx, state, id),
        }
    }

    /// Where the source's log stands — a fork's branch point.
    pub(super) async fn source_log_head(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        parent: ForkParent,
    ) -> Option<u64> {
        let agent = self.fork_source(state, ctx, parent)?;
        agent
            .ask(|reply| AgentCommand::LogHead { reply })
            .await
            .ok()
    }

    /// Start whatever this fork's mode needs before it can be seeded.
    ///
    /// A `Copy` has everything already and goes straight to the handover. A
    /// `Summary` needs a provider call over the source's history, and that call
    /// is *the source's own turn*: queued on its inbox, so accepting the command
    /// and the source becoming busy are one event. Nothing can append to the
    /// history between the branch marker and the summary, which is what makes
    /// the two describe the same conversation.
    ///
    /// Running it out of band instead — the first version — left the source
    /// `Idle` and answering, so a reply sent in that window landed after the
    /// marker and inside the summary.
    pub(super) fn start_seeding(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
    ) {
        let Some(rec) = state.forks.get(id) else {
            tracing::warn!(fork = %id, "no record to seed a fork from");
            return;
        };
        match rec.mode {
            ForkMode::Copy => self.copy_into_fork(ctx, state, id),
            ForkMode::Summary => self.ask_source_to_summarise(ctx, state, id, rec.parent),
        }
    }

    /// Queue the summary as a turn on the conversation being forked.
    ///
    /// The item id is derived from the fork's, not generated: a re-seed after a
    /// crash must ask for the same thing rather than queue a second summary.
    fn ask_source_to_summarise(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        parent: ForkParent,
    ) {
        let Some(source) = self.fork_source(state, ctx, parent) else {
            tracing::warn!(fork = %id, "no conversation to summarise for a fork");
            return;
        };
        tokio::spawn(async move {
            let _ = source
                .tell(AgentCommand::Enqueue {
                    item: Incoming::Fork {
                        id: format!("fork-summarise:{id}"),
                        fork: id,
                    },
                    ack: None,
                })
                .await;
        });
    }

    /// Hand a fork the summary its source's turn produced.
    pub(super) fn finish_seeding(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        summary: String,
    ) {
        self.seed_fork_with(ctx, state, id, Some(summary));
    }

    fn copy_into_fork(&mut self, ctx: &ActorContext<SessionInbox>, state: &SessionState, id: Uuid) {
        self.seed_fork_with(ctx, state, id, None);
    }

    /// Build a fork's initial state and hand it over, off the mailbox.
    ///
    /// Detached because a `Copy` seed reads the source's whole history: holding
    /// the session's mailbox for it would stall every other agent in the
    /// session. [`ForkedAgents::busy`] is what keeps the session loaded
    /// meanwhile.
    ///
    /// `summary` present means the history is not copied at all — a summary fork
    /// starts small, which is the entire point of asking for one.
    fn seed_fork_with(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        summary: Option<String>,
    ) {
        // Everything this needs is on the record, and the record is what a
        // re-seed after a crash reads too — so taking it from there is what
        // makes the first attempt and the retry cut the copy at the same
        // place, from the same branch point, with the same message.
        let Some(rec) = state.forks.get(id).cloned() else {
            tracing::warn!(fork = %id, "no record to seed a fork from");
            return;
        };
        let (parent, source_seq, message) = (rec.parent, rec.source_seq, rec.message);
        let (Some(source), Some(fork)) = (
            self.fork_source(state, ctx, parent),
            self.agents
                .as_ref()
                .and_then(|a| a.sub(id))
                .map(|r| r.actor.clone()),
        ) else {
            tracing::warn!(fork = %id, "no agents to seed a fork between");
            return;
        };
        let source_title = self.source_title(state, parent);
        let self_ref = self.me(ctx);
        tokio::spawn(async move {
            let queued = Incoming::User {
                // Derived from the fork's id, not generated: a re-seed after a
                // crash must produce the same item rather than a second one.
                id: format!("fork-message:{id}"),
                text: message,
            };
            let cmd =
                match seed_fork(&source, &fork, summary, source_seq, &source_title, queued).await {
                    Ok(()) => ForkCommand::Seeded { id },
                    Err(error) => ForkCommand::SeedFailed { id, error },
                };
            let _ = self_ref.tell(SessionCommand::Fork(cmd)).await;
        });
    }

    /// What to call the conversation a fork came from, in the fork's own seed.
    /// A fork of a fork names that fork; anything unnamed falls back to a
    /// phrase rather than to an id, which means nothing to a reader.
    fn source_title(&self, state: &SessionState, parent: ForkParent) -> String {
        let named = match parent {
            ForkParent::Main => self.spec().name.clone(),
            ForkParent::Fork(id) => state.forks.get(id).and_then(|rec| rec.title.clone()),
        };
        named.unwrap_or_else(|| "the conversation before this one".to_string())
    }

    /// Stop a fork's actor, if it is resident, and forget it.
    ///
    /// Best effort: a fork that is not resident has nothing to stop, and the
    /// `ForkDeleted` that follows is what makes the removal durable either way.
    pub(super) async fn retire_fork_actor(&mut self, id: Uuid) {
        let Some(agent) = self.agents.as_mut().and_then(|a| a.remove_sub(id)) else {
            return;
        };
        agent.actor.stop().await;
    }
}

/// Build a fork's history from its source and hand it over.
///
/// Both modes end with one synthetic `Role::User` message carrying a `fork:`
/// id — the device compaction already uses for `compaction:{n}`, so
/// `prompt_messages` needs no change and a client special-cases an id prefix it
/// already special-cases.
async fn seed_fork(
    source: &ActorRef<AgentCommand>,
    fork: &ActorRef<AgentCommand>,
    summary: Option<String>,
    source_seq: u64,
    source_title: &str,
    message: Incoming,
) -> Result<(), String> {
    // A summary fork copies nothing: it starts small, which is the entire point
    // of asking for one. Only a copy reads the source, and only at the branch
    // point — the source goes on appending while this runs, and a copy to the
    // log's end would hand the fork its own creation marker.
    let (state, summary) = match summary {
        Some(summary) => (Box::new(AgentState::default()), summary),
        None => {
            let state = source
                .ask(|reply| AgentCommand::ForkSeed {
                    at_seq: source_seq,
                    reply,
                })
                .await
                .map_err(|e| format!("read the conversation to fork: {e}"))?;
            (state, String::new())
        }
    };
    let seed = Message {
        id: format!("fork:{}", Uuid::new_v4()),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: fork_seed_text(source_title, &summary),
        })],
        created_at_ms: now_ms(),
        started_at_ms: None,
    };
    fork.ask(|reply| AgentCommand::SeedFrom {
        state,
        seed: Box::new(seed),
        message,
        reply,
    })
    .await
    .map_err(|e| format!("seed the fork: {e}"))?
}

/// What a fork reads first.
///
/// The title instruction rides here rather than in the system prompt: a prompt
/// section is re-sent every turn and would go on nagging long after the fork
/// was named.
fn fork_seed_text(source_title: &str, summary: &str) -> String {
    let mut out = format!(
        "This conversation was forked from \"{source_title}\". The message that \
         follows sets a new direction — call set_session_title once it is clear."
    );
    if !summary.is_empty() {
        out.push_str("\n\n# Summary of the conversation this was forked from\n\n");
        out.push_str(summary);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::forks::{ForkMode, ForkParent};

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn state_with_fork(status: AgentStatus) -> SessionState {
        let mut state = SessionState::default();
        state.forks.apply_created(
            id(1),
            ForkParent::Main,
            0,
            ForkMode::Summary,
            "go".into(),
            1,
        );
        state.forks.apply_status(id(1), status, 5_000);
        state
    }

    /// A summariser call in flight must not be unloaded out from under itself.
    #[test]
    fn a_fork_mid_seed_keeps_the_session_loaded() {
        assert!(ForkedAgents::busy(&state_with_fork(
            AgentStatus::Provisioning
        )));
        assert!(!ForkedAgents::busy(&state_with_fork(AgentStatus::Idle)));
        assert!(!ForkedAgents::busy(&SessionState::default()));
    }

    /// Seeding is session-owned work with no journal of its own, so nothing
    /// else can finish one a dead process abandoned.
    #[test]
    fn a_fork_left_mid_seed_is_reseeded_at_load() {
        let spec = crate::sessions::spec::SessionSpec::for_vendor("mock");
        let cx = ActionCx {
            id: id(9),
            spec: &spec,
        };
        assert!(matches!(
            ForkedAgents::on_load(&cx, &state_with_fork(AgentStatus::Provisioning)),
            Some(SessionCommand::Fork(ForkCommand::ReseedInterrupted))
        ));
        assert!(
            ForkedAgents::on_load(&cx, &state_with_fork(AgentStatus::Idle)).is_none(),
            "a seeded fork has nothing to repair"
        );
    }

    #[test]
    fn the_fold_tracks_a_fork_through_its_life() {
        let mut state = SessionState::default();
        ForkedAgents::apply(
            &mut state,
            &SessionDomainEvent::ForkCreated {
                at_ms: 1,
                id: id(1),
                parent: ForkParent::Main,
                source_seq: 12,
                mode: ForkMode::Copy,
                message: "go".into(),
            },
        );
        assert_eq!(
            state.forks.get(id(1)).unwrap().status,
            AgentStatus::Provisioning
        );
        ForkedAgents::apply(
            &mut state,
            &SessionDomainEvent::ForkSeeded {
                at_ms: 2,
                id: id(1),
            },
        );
        assert_eq!(state.forks.get(id(1)).unwrap().status, AgentStatus::Idle);
        ForkedAgents::apply(
            &mut state,
            &SessionDomainEvent::ForkTitled {
                at_ms: 3,
                id: id(1),
                name: "Other migration".to_string(),
            },
        );
        assert_eq!(
            state.forks.get(id(1)).unwrap().title.as_deref(),
            Some("Other migration")
        );
        ForkedAgents::apply(
            &mut state,
            &SessionDomainEvent::ForkDeleted {
                at_ms: 4,
                id: id(1),
            },
        );
        assert!(!state.forks.contains(id(1)));
    }

    // ---- integration, over the real actors ----

    use super::super::testing::{
        BlockingProvider, EchoProvider, FailOnNeedleProvider, agent_history, send,
        spawn_session_with_provider, spawn_sub, turn_outcomes, turns_begun, wait_for_state,
    };
    use crate::sessions::addressing::SessionRef;
    use crate::sessions::session_actor::{SessionCommand, TurnCommand};
    use std::sync::Arc;

    /// Type `text` at `agent_id` and hand back what the fork command answered.
    async fn fork_via(
        session: &SessionRef,
        agent_id: Option<String>,
        text: &str,
    ) -> Result<String, crate::sessions::UserMessageError> {
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id,
                    text: text.into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .map(|a| a.forked_agent.expect("a fork command answers with a fork"))
    }

    /// Every text an agent's log holds, joined — enough to ask whether the
    /// conversation came across.
    async fn transcript(session: &SessionRef, agent_id: Option<String>) -> String {
        agent_history(session, agent_id)
            .await
            .entries
            .iter()
            .map(|e| format!("{:?}", e.body))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// How many turns began in one agent's log, and how the ones that ended
    /// ended. A page folds exactly this pair — an unmatched `TurnBegan` is what
    /// reads `RUNNING` for ever.
    ///
    /// Counted rather than merely looked for: a *copy* fork is seeded with its
    /// source's log, boundaries and all, so "the log holds a `TurnEnded`" is
    /// true before the fork has run at all.
    async fn turn_boundaries(
        session: &SessionRef,
        agent_id: Option<String>,
    ) -> (usize, Vec<horsie_agentcore::TurnOutcome>) {
        let page = agent_history(session, agent_id).await;
        (turns_begun(&page), turn_outcomes(&page))
    }

    /// Wait until `agent_id`'s log holds `turns` turns, all of them closed, and
    /// hand back how the last one ended.
    ///
    /// An exact count rather than "at least one has ended": the seeded copy
    /// arrives already closed, so a floor would pass on a fork whose own turn
    /// never ends — which is the whole thing under test.
    async fn wait_for_turn_end(
        session: &SessionRef,
        agent_id: Option<String>,
        turns: usize,
    ) -> horsie_agentcore::TurnOutcome {
        let mut last = (0, Vec::new());
        for _ in 0..300 {
            last = turn_boundaries(session, agent_id.clone()).await;
            if last.0 == turns && last.1.len() == turns {
                return last.1.pop().expect("a closed turn has an outcome");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "wanted {turns} turns, all closed; {} began and {} ended: {}",
            last.0,
            last.1.len(),
            transcript(session, agent_id).await
        )
    }

    /// Wait until any turn in `agent_id`'s log has ended, and hand back how.
    ///
    /// For a fixture where the copied history carries no boundary of its own —
    /// a source held mid-turn — so the first end to appear is the fork's.
    async fn wait_for_any_turn_end(
        session: &SessionRef,
        agent_id: Option<String>,
    ) -> horsie_agentcore::TurnOutcome {
        for _ in 0..300 {
            if let Some(outcome) =
                turn_outcomes(&agent_history(session, agent_id.clone()).await).pop()
            {
                return outcome;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "no turn ever ended in that log: {}",
            transcript(session, agent_id).await
        )
    }

    /// A fork's page folds its own log: `TurnBegan` reads `Running` and only a
    /// `TurnEnded` clears it. Without one the page says `RUNNING` for ever —
    /// through reloads *and* restarts, because the status is derived from the
    /// journal rather than from anything live.
    #[tokio::test]
    async fn a_forks_turn_ends_in_its_own_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // The source's turn has to be *closed* before forking, because this
        // test's premise is that the copy carries a closed turn over. A fork
        // taken between the source's answer and its `TurnEnded` seeds an
        // unmatched `TurnBegan` — a real hazard, and not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            s.forks.is_seeded(fork_id)
        })
        .await;

        // Two: the source's one turn, which the copy carried over already
        // closed, and the fork's own answer to the message that created it.
        assert!(
            matches!(
                wait_for_turn_end(&session, Some(fork.clone()), 2).await,
                horsie_agentcore::TurnOutcome::Ended(_)
            ),
            "a fork's turn ends like any other conversation's: {}",
            transcript(&session, Some(fork)).await
        );
    }

    /// A fork working is not the session working. The two statuses are read off
    /// different things — the roster and `state.status` — and a client shows
    /// them side by side, so a fork's turn must move exactly one of them.
    #[tokio::test]
    async fn a_forks_turn_moves_the_forks_status_and_not_the_sessions() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // Closed before forking: a fork taken between the source's answer and
        // its `TurnEnded` seeds an unmatched `TurnBegan`, which is a real
        // hazard but not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_turn_end(&session, Some(fork.clone()), 2).await;

        let state = wait_for_state(&journal, id, "the fork settles", |s| {
            s.forks
                .get(fork_id)
                .is_some_and(|r| r.status == AgentStatus::Idle && r.last_activity_ms > 0)
        })
        .await;
        assert_eq!(
            state.status,
            crate::sessions::spec::SessionStatus::Idle,
            "the session's own status belongs to its main agent"
        );
    }

    /// The reason a fork's turn failed has one place a reader will look for it:
    /// the fork's own page. It used to be dropped with a warning, so a fork
    /// whose turn broke went on reading `RUNNING` and said nothing about why.
    #[tokio::test]
    async fn a_forks_failed_turn_says_so_in_its_own_log() {
        let provider = FailOnNeedleProvider {
            needle: "the doomed branch".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        send(&session, "the original question").await;
        // Closed before forking: a fork taken between the source's answer and
        // its `TurnEnded` seeds an unmatched `TurnBegan`, which is a real
        // hazard but not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork the doomed branch")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            s.forks.is_seeded(fork_id)
        })
        .await;

        let outcome = wait_for_turn_end(&session, Some(fork.clone()), 2).await;
        let horsie_agentcore::TurnOutcome::Failed(failed) = &outcome else {
            panic!(
                "a fork's failed turn ends as failed, not {outcome:?}: {}",
                transcript(&session, Some(fork)).await
            );
        };
        assert!(failed.error.contains("bad key"), "{:?}", failed.error);
    }

    /// Stop, addressed to a fork.
    ///
    /// It used to be addressed to nothing: the gate read the *session's* status,
    /// which a fork never moves, so pressing Stop on a fork's page returned
    /// `200` having done nothing at all. The fork went on working, and there was
    /// no way to interrupt it.
    #[tokio::test]
    async fn stopping_a_fork_cancels_that_forks_turn() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) =
            spawn_session_with_provider(provider.clone() as Arc<dyn horsie_agentcore::LlmProvider>)
                .await;
        // The source's turn is held open too, so nothing about this test can
        // pass by stopping the main agent instead.
        send(&session, "the original question").await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is working", |s| {
            s.forks
                .get(fork_id)
                .is_some_and(|r| r.status == AgentStatus::Running)
        })
        .await;

        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::Stop {
                    agent_id: fork.clone(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a working fork is stoppable");

        // Any end in this log is the fork's own: the source is deliberately
        // held mid-turn, so the history the copy carried has an *open* turn in
        // it and no boundary of its own.
        let outcome = wait_for_any_turn_end(&session, Some(fork.clone())).await;
        assert!(
            matches!(outcome, horsie_agentcore::TurnOutcome::Stopped(_)),
            "the fork's turn ends as stopped, not {outcome:?}: {}",
            transcript(&session, Some(fork)).await
        );
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.status,
            crate::sessions::spec::SessionStatus::Running,
            "the source's own turn is untouched — it was not what was stopped"
        );
        provider.release();
    }

    /// An id that names no agent here is a `404`, which the session-wide stop
    /// could not say at all. Distinct from an agent that simply is not working:
    /// nothing to stop is `Ok`, so a client racing a turn's own end is not told
    /// it failed for winning the race.
    #[tokio::test]
    async fn stopping_an_unknown_agent_is_refused_but_an_idle_one_is_not() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;

        let stop = |agent_id: String| {
            let session = session.clone();
            async move {
                session
                    .ask(move |reply| SessionCommand::Turn(TurnCommand::Stop { agent_id, reply }))
                    .await
                    .unwrap()
            }
        };
        assert!(
            stop(Uuid::new_v4().to_string()).await.is_err(),
            "an id naming no agent is refused"
        );
        assert!(
            stop("not-even-a-uuid".to_string()).await.is_err(),
            "and so is one that is not an id at all"
        );
        assert!(
            stop(crate::sessions::session_actor::MAIN_AGENT_ID.to_string())
                .await
                .is_ok(),
            "an agent with nothing in flight is not a failure"
        );
    }

    /// The whole of `/fork`: the fork exists, carries what was said before it,
    /// and answers the message that created it.
    #[tokio::test]
    async fn a_fork_carries_the_conversation_and_answers_its_own_message() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // Closed before forking: a fork taken between the source's answer and
        // its `TurnEnded` seeds an unmatched `TurnBegan`, which is a real
        // hazard but not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");

        // Seeded, not merely created: `Idle` is what the seed landing produces,
        // and it is what releases the message waiting in the fork's queue.
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            s.forks.is_seeded(fork_id)
        })
        .await;

        let forked = transcript(&session, Some(fork.clone())).await;
        assert!(
            forked.contains("the original question"),
            "a copy fork carries the conversation it came from: {forked}"
        );
        assert!(
            forked.contains("forked from"),
            "and is told where it came from: {forked}"
        );
        assert!(
            forked.contains("try the other migration"),
            "and holds the message that created it: {forked}"
        );
    }

    /// A summary fork starts small. That is the entire reason to ask for one,
    /// so the source's messages must *not* be in its log.
    #[tokio::test]
    async fn a_summary_fork_does_not_carry_the_source_messages() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "a very long conversation about migrations").await;

        let fork = fork_via(&session, None, "/summary-n-fork now do the other thing")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the summary fork is seeded", |s| {
            s.forks.is_seeded(fork_id)
        })
        .await;

        let forked = transcript(&session, Some(fork.clone())).await;
        assert!(
            !forked.contains("a very long conversation about migrations"),
            "a summary fork discards the history it summarised: {forked}"
        );
        assert!(
            forked.contains("forked from"),
            "but is still told where it came from: {forked}"
        );
    }

    /// The summary is the source's **own turn**, not a detached read of it.
    ///
    /// This is the whole point of the redesign. Run out of band, the summariser
    /// left the source `Idle` and answering, so a reply sent while it ran landed
    /// after the `Forked` marker in the source's transcript and inside the
    /// fork's summary — the two described different conversations. Queued, the
    /// source cannot append while the summary is taken, and the proof that it
    /// is a turn is that the source's own log carries one.
    #[tokio::test]
    async fn summarising_for_a_fork_is_a_turn_on_the_conversation_it_branches() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        let before = main_turns_begun(&session).await;

        let fork = fork_via(&session, None, "/summary-n-fork now do the other thing")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the summary fork is seeded", |s| {
            s.forks
                .get(fork_id)
                .is_some_and(|r| matches!(r.status, AgentStatus::Idle))
        })
        .await;

        assert!(
            main_turns_begun(&session).await > before,
            "the source ran a turn to produce the summary; its log holds only \
             {before} turn(s), which is what an out-of-band summariser leaves \
             behind:\n{}",
            transcript(&session, None).await
        );
    }

    /// How many turns the session's main agent has begun, from its own log.
    async fn main_turns_begun(session: &SessionRef) -> usize {
        turns_begun(&agent_history(session, None).await)
    }

    /// The branch point is visible where it happened, so scrolling the source
    /// shows where each fork left.
    #[tokio::test]
    async fn the_source_transcript_records_where_a_fork_left() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "first").await;
        let fork = fork_via(&session, None, "/fork branch here")
            .await
            .expect("a fork");

        for _ in 0..200 {
            if transcript(&session, None).await.contains(&fork) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "the source never recorded the branch: {}",
            transcript(&session, None).await
        );
    }

    /// A subagent's conversation is delegated work, not a branch to take.
    #[tokio::test]
    async fn only_a_conversation_can_be_forked() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;

        let err = fork_via(&session, Some(sub.to_string()), "/fork off you go")
            .await
            .expect_err("a subagent cannot be forked");
        assert!(
            matches!(err, crate::sessions::UserMessageError::Rejected(ref m)
                if m.contains("only a conversation")),
            "{err:?}"
        );
    }

    /// A fork with nothing to do is a fork nobody comes back to.
    #[tokio::test]
    async fn a_fork_needs_a_message() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let err = fork_via(&session, None, "/fork")
            .await
            .expect_err("a bare fork is refused");
        assert!(
            matches!(err, crate::sessions::UserMessageError::Rejected(ref m)
                if m.contains("needs a message")),
            "{err:?}"
        );
    }

    /// Forks nest: a fork of a fork records the fork it came from, not main.
    #[tokio::test]
    async fn a_fork_of_a_fork_records_the_fork_it_came_from() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "start").await;

        let first = fork_via(&session, None, "/fork one").await.expect("a fork");
        let first_id = Uuid::parse_str(&first).unwrap();
        wait_for_state(&journal, id, "the first fork is seeded", |s| {
            s.forks.is_seeded(first_id)
        })
        .await;

        let second = fork_via(&session, Some(first.clone()), "/fork two")
            .await
            .expect("a fork of a fork");
        let second_id = Uuid::parse_str(&second).unwrap();
        let state = wait_for_state(&journal, id, "the second fork exists", |s| {
            s.forks.contains(second_id)
        })
        .await;
        assert_eq!(
            state.forks.get(second_id).unwrap().parent,
            ForkParent::Fork(first_id),
            "a fork of a fork is rooted on that fork"
        );
    }

    /// Forking a conversation that is parked on a question.
    ///
    /// The copied log carries the `ask_user` `tool_use` with no result. A
    /// dangling call 400s every provider, so what makes this work is the
    /// sanitization every turn start already runs — this is the proof that a
    /// fork's first turn goes through it like any other.
    ///
    /// Note what this does *not* prove: the fork would run even if `asks` were
    /// carried, because its own queued message is a person speaking and that
    /// overrides a park by design. Dropping `asks` is defensive, not what makes
    /// this pass.
    #[tokio::test]
    async fn a_fork_of_a_parked_conversation_runs_rather_than_inheriting_the_question() {
        use horsie_agentcore::{
            StopReason,
            testkit::{MockProvider, Script},
        };
        // The source's first call asks the user and parks. Everything after —
        // including every call the fork makes — answers with plain text.
        let provider = MockProvider::scripted(
            Script::of([Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::ToolCall(
                    horsie_agentcore::ToolCallPart {
                        id: "ask-1".into(),
                        name: "ask_user".into(),
                        input: serde_json::json!({"question": "which migration?"}),
                    },
                )],
                stop_reason: StopReason::ToolUse,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })])
            .then_repeating_with(|| {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "the fork answered".to_string(),
                        },
                    )],
                    stop_reason: StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            }),
        );
        let (_f, session, id, journal) = spawn_session_with_provider(provider).await;

        send(&session, "start").await;
        wait_for_state(&journal, id, "the source parks on its question", |s| {
            matches!(
                s.status,
                crate::sessions::spec::SessionStatus::AwaitingInput
            )
        })
        .await;

        let fork = fork_via(&session, None, "/fork never mind, do the other thing")
            .await
            .expect("a parked conversation can still be forked");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            s.forks.is_seeded(fork_id)
        })
        .await;

        // The question is *visible* in the copied transcript — it happened —
        // but the fork is not waiting on it, so its own turn runs to an answer.
        for _ in 0..200 {
            let t = transcript(&session, Some(fork.clone())).await;
            if t.contains("the fork answered") {
                assert!(
                    t.contains("which migration?"),
                    "the question is still readable in the copied history: {t}"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "the fork never answered: {}",
            transcript(&session, Some(fork)).await
        );
    }

    /// Forking a conversation with a message still queued on it.
    ///
    /// This is the drop that is genuinely load-bearing. A message the source
    /// has accepted but not yet answered belongs to the *source*: it queued
    /// because a turn was in flight, and that turn's boundary is what answers
    /// it. Copied into the fork, both conversations answer it — the person
    /// gets two replies to one message, and the fork's first turn is polluted
    /// by a message that was never meant for it.
    #[tokio::test]
    async fn a_fork_does_not_take_over_a_message_queued_on_the_source() {
        use super::super::testing::BlockingProvider;

        let provider = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(provider.clone()).await;

        // Hold the source inside a turn, so the next message queues rather
        // than draining.
        send(&session, "the turn that is running").await;
        wait_for_state(&journal, id, "the source is running", |s| {
            matches!(s.status, crate::sessions::spec::SessionStatus::Running)
        })
        .await;
        send(&session, "QUEUED-FOR-THE-SOURCE").await;

        let fork = fork_via(&session, None, "/fork the fork's own instruction")
            .await
            .expect("a busy conversation can still be forked");

        // The fork's *queue* must not hold it. The source's log records that
        // the message was queued — that happened, and the copied history says
        // so — but the fork must not be the one to answer it, so it is not an
        // `Incoming` the fork will merge into a turn.
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            s.forks.is_seeded(fork_id)
        })
        .await;
        let forked = transcript(&session, Some(fork.clone())).await;
        assert!(
            !forked.contains("Received") && !forked.contains("QUEUED-FOR-THE-SOURCE\", "),
            "the source's queued message is not the fork's to answer: {forked}"
        );
        // And the copy stops at the branch point: the `Forked` entry recording
        // this very fork is written onto the *source* after the branch, so a
        // copy taken at the log's end would hand the fork a marker pointing at
        // itself.
        assert!(
            !forked.contains("Forked("),
            "a fork must not carry its own creation marker: {forked}"
        );

        provider.release();
    }

    /// The seed always frames where the fork came from; only a summary fork
    /// carries a summary, because only it discarded the history.
    #[test]
    fn the_seed_frames_the_source_and_carries_a_summary_only_when_there_is_one() {
        let copy = fork_seed_text("Migrate the journal", "");
        assert!(copy.contains("forked from \"Migrate the journal\""));
        assert!(copy.contains("set_session_title"));
        assert!(!copy.contains("# Summary"));

        let summarised = fork_seed_text("Migrate the journal", "We chose sqlx::Any.");
        assert!(summarised.contains("# Summary"));
        assert!(summarised.contains("We chose sqlx::Any."));
    }
}
