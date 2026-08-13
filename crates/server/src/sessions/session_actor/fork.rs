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
    AgentPlan, AgentStatus, CommandEffect, ForkCommand, SessionActor, SessionCommand,
    SessionDomainEvent, SessionState,
};
use crate::agent_loop::{AgentCommand, AgentState, Incoming};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::forks::{ForkMode, ForkParent};
use horsie_actor::{ActorContext, ActorRef, ReplyTo};
use horsie_agentcore::{ContentPart, Message, Role, TextPart};
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
                    let _ = reply.send(Err("the conversation to fork is not available".to_string()));
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
                            parent,
                            mode,
                            message,
                            reply,
                            persisted,
                        }))
                        .await;
                });
                CommandEffect::persist(vec![created]).and_ack(ReplyTo::from_sender(tx))
            }
            ForkCommand::FinishCreate {
                id,
                parent,
                mode,
                message,
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
                actor.start_seeding(ctx, state, id, parent, mode, message);
                // The id travels now rather than when the seed lands: the
                // client redirects to a fork that is visibly building itself,
                // which is exactly what a newly created session does.
                let _ = reply.send(Ok(id));
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
                let normalized = match crate::sessions::title_tool::normalize_session_title(&title) {
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
                    let Some((parent, mode, message)) = state
                        .forks
                        .get(id)
                        .map(|rec| (rec.parent, rec.mode, rec.message.clone()))
                    else {
                        continue;
                    };
                    // Spawning is what a fork needs to be seeded *into*: a
                    // session that reloaded has no resident agents at all.
                    if actor.spawn_fork_actor(ctx, state, id).is_none() {
                        tracing::warn!(fork = %id, "could not restart a fork to re-seed it");
                        continue;
                    }
                    actor.start_seeding(ctx, state, id, parent, mode, message);
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
            SessionDomainEvent::ForkStatusChanged { id, status, .. } => {
                state.forks.apply_status(id, status);
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
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::Fork(id),
                settings: self.spec().agent.clone(),
                step_output_schema: None,
                agent_type: None,
                handoff_tool: None,
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

    /// Build a fork's initial state and hand it over, off the mailbox.
    ///
    /// Detached because a `Summary` seed is a provider call: holding the
    /// session's mailbox for it would stall every other agent in the session.
    /// [`ForkedAgents::busy`] is what keeps the session loaded meanwhile.
    pub(super) fn start_seeding(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        parent: ForkParent,
        mode: ForkMode,
        message: String,
    ) {
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
            let cmd = match seed_fork(&source, &fork, mode, &source_title, queued).await {
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
    mode: ForkMode,
    source_title: &str,
    message: Incoming,
) -> Result<(), String> {
    let (state, summary) = match mode {
        ForkMode::Copy => {
            let state = source
                .ask(|reply| AgentCommand::ForkSeed { reply })
                .await
                .map_err(|e| format!("read the conversation to fork: {e}"))?;
            (state, String::new())
        }
        // Nothing is read from the source but the summary: a summary fork
        // starts small, which is the entire point of asking for one.
        ForkMode::Summary => {
            let summary = source
                .ask(|reply| AgentCommand::SummariseAll { reply })
                .await
                .map_err(|e| format!("summarise the conversation to fork: {e}"))?
                .map_err(|e| format!("summarise the conversation to fork: {e}"))?;
            (Box::new(AgentState::default()), summary)
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
        state
            .forks
            .apply_created(id(1), ForkParent::Main, 0, ForkMode::Summary, "go".into(), 1);
        state.forks.apply_status(id(1), status);
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
        EchoProvider, agent_history, send, spawn_session_with_provider, spawn_sub, wait_for_state,
    };
    use crate::sessions::session_actor::{SessionCommand, TurnCommand};
    use crate::sessions::addressing::SessionRef;
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

    /// The whole of `/fork`: the fork exists, carries what was said before it,
    /// and answers the message that created it.
    #[tokio::test]
    async fn a_fork_carries_the_conversation_and_answers_its_own_message() {
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");

        // Seeded, not merely created: `Idle` is what the seed landing produces,
        // and it is what releases the message waiting in the fork's queue.
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            s.forks
                .get(fork_id)
                .is_some_and(|r| matches!(r.status, AgentStatus::Idle))
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
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "a very long conversation about migrations").await;

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
        let (_f, session, id, journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "start").await;

        let first = fork_via(&session, None, "/fork one").await.expect("a fork");
        let first_id = Uuid::parse_str(&first).unwrap();
        wait_for_state(&journal, id, "the first fork is seeded", |s| {
            s.forks
                .get(first_id)
                .is_some_and(|r| matches!(r.status, AgentStatus::Idle))
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
