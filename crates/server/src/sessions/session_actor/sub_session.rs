//! The session's sub sessions: branching a session into a second one that a
//! person can talk to.
//!
//! A sub session is not a subagent. It owes nobody a result, it has
//! `ask_user`, and it names itself — so it takes the main agent's toolbox
//! layers and gets a roster of its own rather than a node in the subagent
//! forest. It is hosted by the root session's actor rather than being a
//! session of its own: it runs under its own agent id, which is the whole
//! reason it is cheap.
//!
//! Persists a create *before* the sub session's actor exists, exactly as a
//! subagent spawn does: a crash between the two replays as a sub session still
//! `Provisioning`, which [`SubSessions::on_load`] re-seeds — strictly better
//! than an untracked agent.

use super::component::Component;
use super::context::SessionAgentKind;
use super::{
    AgentKey, AgentPlan, AgentStatus, CommandEffect, LifecycleCommand, RequestedRuntime,
    SessionActor, SessionCommand, SessionDomainEvent, SessionState, SubSessionCommand, TurnEnd,
};
use crate::agent_loop::{AgentCommand, AgentState, Incoming};
use crate::agent_loop::{
    QueueCommand as AgentQueueCommand, ReadCommand as AgentReadCommand,
    SeedCommand as AgentSeedCommand,
};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::run_forest::{RuntimeChoice, SeedMode};
use horsie_actor::{ActorContext, ActorRef, ReplyTo};
use horsie_agentcore::{
    ContentPart, EmptyOutcome, FailedOutcome, Message, Role, TextPart, TurnOutcome,
};
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

pub(super) struct SubSessions;

impl SubSessions {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: SubSessionCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            SubSessionCommand::Create {
                parent,
                seed,
                message,
                title,
                env,
                reply,
            } => {
                if let Err(why) = branchable(actor.id, state, parent, &message) {
                    let _ = reply.send(Err(why));
                    return CommandEffect::none();
                }
                // The branch point, read before anything is written: where the
                // source's log stands right now is what this sub session
                // carries.
                let Some(source_seq) = actor.source_log_head(state, ctx, parent).await else {
                    let _ = reply.send(Err("the session to branch is not available".to_string()));
                    return CommandEffect::none();
                };
                let id = Uuid::new_v4();
                let created = SessionDomainEvent::SubSessionCreated {
                    at_ms: now_ms(),
                    id,
                    parent,
                    source_seq,
                    seed,
                    message: message.clone(),
                    title,
                    // Which of the three the caller asked for. Never an id:
                    // that is minted by `RuntimeRequested`, which is also what
                    // moves this entry to `On`. Two writers would be two ids.
                    //
                    // `Pending` is what makes a sub session with a machine of
                    // its own wait for it rather than start without one.
                    runtime: match &env {
                        RequestedRuntime::Own(_) => RuntimeChoice::Pending,
                        RequestedRuntime::Without => RuntimeChoice::Without,
                        RequestedRuntime::Inherit => RuntimeChoice::Inherit,
                    },
                };
                // Persist first, spawn second — see the module doc.
                let (tx, rx) = oneshot::channel();
                let self_ref = actor.me(ctx);
                tokio::spawn(async move {
                    let persisted = rx.await.unwrap_or_else(|_| {
                        Err(horsie_actor::JournalError::Backend(
                            "sub session ack channel closed".to_string(),
                        ))
                    });
                    let _ = self_ref
                        .tell(SessionCommand::SubSession(
                            SubSessionCommand::FinishCreate {
                                id,
                                env,
                                reply,
                                persisted,
                            },
                        ))
                        .await;
                });
                CommandEffect::persist(vec![created]).and_ack(ReplyTo::from_sender(tx))
            }
            SubSessionCommand::FinishCreate {
                id,
                env,
                reply,
                persisted,
            } => {
                if let Err(e) = persisted {
                    let _ = reply.send(Err(format!("persist the sub session: {e}")));
                    return CommandEffect::none();
                }
                if actor.spawn_sub_session_actor(ctx, state, id).is_none() {
                    let _ = reply.send(Err("could not start the sub session".to_string()));
                    return CommandEffect::none();
                }
                // A sub session that asked for an environment of its own gets a
                // runtime built for it, owned by it. Sent now rather than at
                // `Create`, so the entry the create points at already exists —
                // and off this mailbox, so the minutes a machine takes to boot
                // do not stop the session answering.
                //
                // Its first turn waits on the result, exactly as a new
                // session's does: the `ready` flag its agent is built with is
                // false until the record says otherwise.
                if let RequestedRuntime::Own(env) = env {
                    let _ = actor
                        .me(ctx)
                        .tell(SessionCommand::Lifecycle(LifecycleCommand::Provision {
                            owner: id,
                            env: Some(env),
                        }))
                        .await;
                }
                // The message is *not* enqueued here. It rides into the same
                // write as the seed, because a sub session with a message and
                // no history drains it immediately and answers a session it
                // has not been given yet.
                actor.start_seeding(ctx, state, id);
                // The id travels now rather than when the seed lands: the
                // client redirects to a sub session that is visibly building
                // itself, which is exactly what a newly created session does.
                let _ = reply.send(Ok(id));
                CommandEffect::none()
            }
            SubSessionCommand::Summarised {
                sub_sessions,
                result,
            } => {
                for id in sub_sessions {
                    // Dropped rather than reported: a sub session deleted
                    // while its summary was being taken is not a failure, it
                    // is the user having changed their mind.
                    if state.forest.sub_session(id).is_none() {
                        continue;
                    }
                    match &result {
                        Ok(summary) => actor.finish_seeding(ctx, state, id, summary.clone()),
                        Err(error) => {
                            let _ = actor
                                .me(ctx)
                                .tell(SessionCommand::SubSession(SubSessionCommand::SeedFailed {
                                    id,
                                    error: error.clone(),
                                }))
                                .await;
                        }
                    }
                }
                CommandEffect::none()
            }
            SubSessionCommand::Seeded { id } => {
                if state.forest.sub_session(id).is_none() {
                    return CommandEffect::none();
                }
                // Through `persist_and_advance` rather than a bare persist:
                // the sub session becoming ready is what releases the message
                // queued behind it, and that release is an action.
                actor
                    .persist_and_advance(
                        state,
                        vec![SessionDomainEvent::SubSessionSeeded {
                            at_ms: now_ms(),
                            id,
                        }],
                        ctx,
                    )
                    .await
            }
            SubSessionCommand::SeedFailed { id, error } => {
                if state.forest.sub_session(id).is_none() {
                    return CommandEffect::none();
                }
                tracing::warn!(sub_session = %id, error, "seeding a sub session failed");
                CommandEffect::persist(vec![SessionDomainEvent::SubSessionStatusChanged {
                    at_ms: now_ms(),
                    id,
                    status: AgentStatus::Failed,
                }])
            }
            SubSessionCommand::ReseedInterrupted => {
                for id in state.forest.seeding_sub_sessions() {
                    // Spawning is what a sub session needs to be seeded
                    // *into*: a session that reloaded has no resident agents
                    // at all.
                    if actor.spawn_sub_session_actor(ctx, state, id).is_none() {
                        tracing::warn!(sub_session = %id, "could not restart a sub session to re-seed it");
                        continue;
                    }
                    actor.start_seeding(ctx, state, id);
                }
                CommandEffect::none()
            }
        }
    }
}

impl Component for SubSessions {
    /// A sub session left `Provisioning` by a dead process. Nothing else can
    /// finish one: seeding is session-owned work with no journal of its own,
    /// unlike a turn, which the agent reports as interrupted from its own
    /// recovery.
    ///
    /// Safe to re-attempt for the reason
    /// [`RuntimeLifecycle`](super::lifecycle::RuntimeLifecycle) gives about
    /// its own case: `Provisioning` is precisely the state in which no turn
    /// has run.
    fn on_load(state: &SessionState) -> Option<SessionCommand> {
        state
            .forest
            .has_seeding_sub_sessions()
            .then_some(SessionCommand::SubSession(
                SubSessionCommand::ReseedInterrupted,
            ))
    }

    /// A summariser call is provider time with nothing durable behind it.
    /// Unloading the session mid-seed loses it and leaves a sub session that
    /// only a reload repairs.
    fn busy(state: &SessionState) -> bool {
        // A summariser call mid-seed, or a sub session's own turn in flight:
        // both are work an unload would lose.
        state.forest.has_seeding_sub_sessions()
            || state
                .forest
                .sub_sessions()
                .any(|(_, f)| f.status == AgentStatus::Running)
    }

    // The fallthrough is unreachable by construction:
    // `SessionActor::apply_event` matches every variant explicitly and routes
    // each to exactly one component, so a newly added event fails to compile
    // *there* — which is where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut SessionState, event: &SessionDomainEvent) {
        match event.clone() {
            SessionDomainEvent::SubSessionCreated {
                id,
                parent,
                source_seq,
                seed,
                message,
                title,
                at_ms,
                runtime,
            } => state.forest.apply_sub_session_created(
                id, parent, source_seq, seed, message, title, at_ms, runtime,
            ),
            SessionDomainEvent::SubSessionSeeded { id, .. } => {
                state.forest.apply_sub_session_seeded(id)
            }
            SessionDomainEvent::SubSessionStatusChanged { at_ms, id, status } => {
                state.forest.apply_sub_session_status(id, status, at_ms);
            }
            // The status is derived from the outcome, never carried beside it:
            // a session that stopped working is idle unless the turn is
            // what broke, and a second field saying so is a second thing that
            // can disagree with the first.
            SessionDomainEvent::SubSessionTurnEnded { at_ms, id, outcome } => {
                let status = match outcome {
                    TurnOutcome::Failed(_) => AgentStatus::Failed,
                    TurnOutcome::Ended(_)
                    | TurnOutcome::Stopped(_)
                    | TurnOutcome::Interrupted(_) => AgentStatus::Idle,
                };
                state.forest.apply_sub_session_status(id, status, at_ms);
            }
            other => unreachable!("SubSessions was handed {other:?}"),
        }
    }
}

/// Handlers that belong to this component but act on the actor's own fields —
/// the roster and the spawn helpers. An inherent `impl` in a child module sees
/// them, so moving the code needed no plumbing.
impl SessionActor {
    /// One of this session's sub sessions finished a turn.
    ///
    /// The sub session half of
    /// [`SessionActor::on_main_outcome`](super::SessionActor::on_main_outcome),
    /// and it answers the same five ends — a sub session *is* a session, so
    /// there is no end the main agent can reach that a sub session cannot.
    /// What differs is only the scope: these move the sub session's roster
    /// entry and write into the sub session's own log, never the session's
    /// status, because a sub session working is not the session working.
    pub(super) async fn on_sub_session_outcome(
        &mut self,
        state: &SessionState,
        id: Uuid,
        end: TurnEnd,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionDomainEvent> {
        let outcome = match end {
            TurnEnd::Concluded { .. } => TurnOutcome::Ended(EmptyOutcome {}),
            // Not a boundary: the turn is parked, not over, and the question
            // is journaled into the sub session's log by the agent that asked
            // it — the same division the main agent's `AskRecorded` follows.
            TurnEnd::Asked => {
                return self
                    .persist_and_advance(
                        state,
                        vec![SessionDomainEvent::SubSessionStatusChanged {
                            at_ms: now_ms(),
                            id,
                            status: AgentStatus::AwaitingInput,
                        }],
                        ctx,
                    )
                    .await;
            }
            // Session-wide, exactly as it is for the main agent: sub sessions
            // share the one runtime, so a runtime that cannot be rebuilt takes
            // every session in the session with it, not just this one.
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
            // Only from a sub session this session still believes is running —
            // the same guard, for the same reason, as the main agent's: a
            // report about anything but a live turn is history already
            // written.
            TurnEnd::Interrupted => {
                let running = state
                    .forest
                    .sub_session(id)
                    .is_some_and(|rec| rec.status == AgentStatus::Running);
                if !running {
                    return CommandEffect::none();
                }
                TurnOutcome::Interrupted(EmptyOutcome {})
            }
        };
        self.persist_and_advance(
            state,
            vec![SessionDomainEvent::SubSessionTurnEnded {
                at_ms: now_ms(),
                id,
                outcome,
            }],
            ctx,
        )
        .await
    }

    /// Spawn one sub session's actor.
    ///
    /// Takes the main agent's plan, because a sub session is a session: the
    /// session's own settings, no declared output, and no handoff tool — it
    /// ends its turn with plain text like the agent it branched from.
    pub(super) fn spawn_sub_session_actor(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
    ) -> Option<ActorRef<AgentCommand>> {
        if let Some(resident) = self.agents.sub(id) {
            return Some(resident.actor.clone());
        }
        // A sub session runs under the agent session's own settings — sub
        // sessions exist only under an agent session, so a run answers `None`
        // here rather than inventing settings nothing owns.
        let settings = self
            .effective_settings(state, AgentKey::SubSession(id))
            .cloned()?;
        self.spawn_agent(
            ctx,
            state,
            AgentPlan {
                kind: SessionAgentKind::SubSession(id),
                settings,
                step_result: Default::default(),
                agent_type: None,
                origin: self.sub_session_origin(state, id),
                runtime_via: None,
            },
        )
        .map(|resident| resident.actor)
    }

    /// The agent a sub session is being taken from — main, or another sub
    /// session — spawned if it is not resident.
    pub(super) fn sub_session_source(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        parent: Uuid,
    ) -> Option<ActorRef<AgentCommand>> {
        match parent == self.id {
            true => self.agent(),
            false => self.spawn_sub_session_actor(ctx, state, parent),
        }
    }

    /// Where the source's log stands — a sub session's branch point.
    pub(super) async fn source_log_head(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        parent: Uuid,
    ) -> Option<u64> {
        let agent = self.sub_session_source(state, ctx, parent)?;
        agent
            .ask(|reply| AgentCommand::Read(AgentReadCommand::LogHead { reply }))
            .await
            .ok()
    }

    /// Start whatever this sub session's mode needs before it can be seeded.
    ///
    /// A `Copy` has everything already and goes straight to the handover, and
    /// so does a `Fresh`, which carries nothing. A `Summary` needs a provider
    /// call over the source's history, and that call is *the source's own
    /// turn*: queued on its inbox, so accepting the command and the source
    /// becoming busy are one event. Nothing can append to the history between
    /// the branch marker and the summary, which is what makes the two describe
    /// the same session.
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
        let Some((entry, rec)) = state
            .forest
            .owner_of_agent(id)
            .and_then(|(rid, e)| state.forest.sub_session(rid.0).map(|f| (e, f)))
        else {
            tracing::warn!(sub_session = %id, "no record to seed a sub session from");
            return;
        };
        let Some(parent) = entry.parent else {
            tracing::warn!(sub_session = %id, "a sub session with no parent cannot be seeded");
            return;
        };
        match rec.seed {
            SeedMode::Copy => self.seed_sub_session_with(ctx, state, id, Seed::Copy),
            SeedMode::Fresh => self.seed_sub_session_with(ctx, state, id, Seed::Fresh),
            SeedMode::Summary => self.ask_source_to_summarise(ctx, state, id, parent),
        }
    }

    /// Queue the summary as a turn on the session being branched.
    ///
    /// The item id is derived from the sub session's, not generated: a re-seed
    /// after a crash must ask for the same thing rather than queue a second
    /// summary.
    fn ask_source_to_summarise(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        parent: Uuid,
    ) {
        let Some(source) = self.sub_session_source(state, ctx, parent) else {
            tracing::warn!(sub_session = %id, "no session to summarise for a sub session");
            return;
        };
        tokio::spawn(async move {
            let _ = source
                .tell(AgentCommand::Queue(AgentQueueCommand::Enqueue {
                    item: Incoming::SubSession {
                        id: format!("sub_session-summarise:{id}"),
                        sub_session: id,
                    },
                    ack: None,
                }))
                .await;
        });
    }

    /// Hand a sub session the summary its source's turn produced.
    pub(super) fn finish_seeding(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        summary: String,
    ) {
        self.seed_sub_session_with(ctx, state, id, Seed::Summary(summary));
    }

    /// Build a sub session's initial state and hand it over, off the mailbox.
    ///
    /// Detached because a `Copy` seed reads the source's whole history: holding
    /// the session's mailbox for it would stall every other agent in the
    /// session. [`SubSessions::busy`] is what keeps the session loaded
    /// meanwhile.
    fn seed_sub_session_with(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: Uuid,
        seed: Seed,
    ) {
        // Everything this needs is on the record, and the record is what a
        // re-seed after a crash reads too — so taking it from there is what
        // makes the first attempt and the retry cut the copy at the same
        // place, from the same branch point, with the same message.
        let Some((parent, rec)) = state
            .forest
            .owner_of_agent(id)
            .and_then(|(_, e)| e.parent)
            .zip(state.forest.sub_session(id).cloned())
        else {
            tracing::warn!(sub_session = %id, "no record to seed a sub session from");
            return;
        };
        let (source_seq, message) = (rec.source_seq, rec.message);
        let (Some(source), Some(sub_session)) = (
            self.sub_session_source(state, ctx, parent),
            self.agents.sub(id).map(|r| r.actor.clone()),
        ) else {
            tracing::warn!(sub_session = %id, "no agents to seed a sub session between");
            return;
        };
        let self_ref = self.me(ctx);
        tokio::spawn(async move {
            let queued = Incoming::User {
                // Derived from the sub session's id, not generated: a re-seed
                // after a crash must produce the same item rather than a
                // second one.
                id: format!("sub_session-message:{id}"),
                text: message,
                // A branch carries the command that made it, which is text.
                artifacts: Vec::new(),
            };
            let cmd = match seed_sub_session(&source, &sub_session, seed, source_seq, queued).await
            {
                Ok(()) => SubSessionCommand::Seeded { id },
                Err(error) => SubSessionCommand::SeedFailed { id, error },
            };
            let _ = self_ref.tell(SessionCommand::SubSession(cmd)).await;
        });
    }

    /// Where a sub session came from, for the `# Sub session` section of its
    /// system prompt.
    ///
    /// Read from the record rather than passed in, because this runs on every
    /// spawn — including the one recovery does for a sub session created by a
    /// process that is gone.
    fn sub_session_origin(
        &self,
        state: &SessionState,
        id: Uuid,
    ) -> Option<crate::sessions::session_actor::context::SubSessionOrigin> {
        let parent = state
            .forest
            .owner_of_agent(id)
            .and_then(|(_, entry)| entry.parent)?;
        Some(crate::sessions::session_actor::context::SubSessionOrigin {
            mode: state.forest.sub_session(id)?.seed,
            source_title: self.source_title(state, parent),
        })
    }

    /// What to call the session a sub session came from. A sub session of a
    /// sub session names that sub session; anything unnamed falls back to a
    /// phrase rather than to an id, which means nothing to a reader.
    fn source_title(&self, state: &SessionState, parent: Uuid) -> String {
        let named = match parent == self.id {
            true => state.forest.main_title().map(str::to_string),
            false => state
                .forest
                .sub_session(parent)
                .map(|rec| rec.title.clone()),
        };
        named.unwrap_or_else(|| "the session before this one".to_string())
    }

    /// Stop a sub session's actor, if it is resident, and forget it.
    ///
    /// Best effort: a sub session that is not resident has nothing to stop,
    /// and the `ForkDeleted` that follows is what makes the removal durable
    /// either way.
    pub(super) async fn retire_agent_actor(&mut self, id: Uuid) {
        let Some(agent) = self.agents.remove(id) else {
            return;
        };
        agent.actor.stop().await;
    }
}

/// What every sub session must be true of, whoever asked for one.
///
/// Here rather than at the two callers — the composer's `/fork` and the
/// `spawn_subsession` tool — because this is the one place a sub session is
/// written, and an invariant checked anywhere else is one a third caller will
/// miss.
fn branchable(main: Uuid, state: &SessionState, parent: Uuid, message: &str) -> Result<(), String> {
    if message.trim().is_empty() {
        return Err("a sub session needs a message saying what it should do".to_string());
    }
    // A run has no session to branch: its steps are chosen by the
    // definition, and nobody talks to one.
    if state.forest.root_is_workflow() {
        return Err("a workflow run cannot be branched".to_string());
    }
    // Only a session sub sessions. A subagent's is delegated work and a step's
    // belongs to the run, so neither has a branch to take.
    if parent != main && state.forest.sub_session(parent).is_none() {
        return Err("only a session can be branched".to_string());
    }
    Ok(())
}

/// What a sub session's history is built from — [`ForkMode`] with the summary
/// in hand.
///
/// A separate enum because the mode is what the journal records and this is
/// what one seeding run needs: the summary exists only between the source's
/// turn producing it and the sub session's write, and has no business on the
/// record.
enum Seed {
    /// The source's log, copied to the branch point.
    Copy,
    /// A summary of it, produced by the source's own turn.
    Summary(String),
    /// Nothing. The brief queued behind this seed is the whole history.
    Fresh,
}

/// Build a sub session's history from its source and hand it over.
///
/// Only a `Summary` ends with a synthetic `Role::User` message — the summary
/// itself, which is content the model needs and has nowhere else to be. It
/// carries a `sub_session:` id, the device compaction already uses for
/// `compaction:{n}`, so `prompt_messages` needs no change and a client
/// special-cases an id prefix it already special-cases.
///
/// The other two seed no message at all. What a sub session *is* — what it
/// branched from and what history that left it with — is standing context
/// rather than a turn, so it is a section of the system prompt
/// (`sub_session_prompt`). Written here instead, it read as a message the
/// person had typed, and said the same thing the prompt was already saying.
async fn seed_sub_session(
    source: &ActorRef<AgentCommand>,
    sub_session: &ActorRef<AgentCommand>,
    seed: Seed,
    source_seq: u64,
    message: Incoming,
) -> Result<(), String> {
    // Only a copy reads the source, and only at the branch point — the source
    // goes on appending while this runs, and a copy to the log's end would
    // hand the sub session its own creation marker. The other two start on an
    // empty state, which is the entire point of asking for either.
    let state = match seed {
        Seed::Summary(_) | Seed::Fresh => Box::new(AgentState::default()),
        Seed::Copy => source
            .ask(|reply| {
                AgentCommand::Seed(AgentSeedCommand::Snapshot {
                    at_seq: source_seq,
                    reply,
                })
            })
            .await
            .map_err(|e| format!("read the session to branch: {e}"))?,
    };
    let seed = summary_message(&seed);
    sub_session
        .ask(|reply| {
            AgentCommand::Seed(AgentSeedCommand::SeedFrom {
                state,
                seed: seed.map(Box::new),
                message,
                reply,
            })
        })
        .await
        .map_err(|e| format!("seed the sub_session: {e}"))?
}

/// The one seeded message: a `Summary`'s summary, and nothing for the other
/// two modes.
///
/// A `Copy` has the conversation itself and a `Fresh` has the brief queued
/// behind it, so in both cases a message here would be a second telling of
/// something the sub session already holds.
fn summary_message(seed: &Seed) -> Option<Message> {
    let Seed::Summary(summary) = seed else {
        return None;
    };
    Some(Message {
        id: format!("sub_session:{}", Uuid::new_v4()),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            text: format!("# Summary of the session this was branched from\n\n{summary}"),
        })],
        created_at_ms: now_ms(),
        started_at_ms: None,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::run_forest::{RuntimeChoice, SeedMode};

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn state_with_sub_session(status: AgentStatus) -> SessionState {
        let mut state = SessionState::default();
        let session = id(9);
        state.forest.apply_root_agent(
            session,
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        state.forest.apply_sub_session_created(
            id(1),
            session,
            0,
            SeedMode::Summary,
            "go".into(),
            "a branch".into(),
            1,
            RuntimeChoice::Inherit,
        );
        state.forest.apply_sub_session_status(id(1), status, 5_000);
        state
    }

    /// The invariants live at the write, so the composer's `/fork` and the
    /// `spawn_subsession` tool cannot disagree about what a sub session may be.
    #[test]
    fn a_sub_session_needs_a_message_saying_what_to_do() {
        let mut state = SessionState::default();
        state.forest.apply_root_agent(
            id(9),
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        assert!(branchable(id(9), &state, id(9), "go").is_ok());
        assert_eq!(
            branchable(id(9), &state, id(9), "   ").unwrap_err(),
            "a sub session needs a message saying what it should do"
        );
    }

    /// Nobody talks to a run, so there is no session in one to branch.
    #[test]
    fn a_workflow_run_cannot_be_branched() {
        let mut state = SessionState::default();
        state.forest.apply_root_workflow(
            id(9),
            "nightly".into(),
            std::sync::Arc::new(crate::sessions::workflow::WorkflowRunSpec {
                workflow: "nightly".into(),
                start: "first".into(),
                steps: Vec::new(),
                input: String::new(),
                max_steps: 1,
            }),
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        assert_eq!(
            branchable(id(9), &state, id(9), "go").unwrap_err(),
            "a workflow run cannot be branched"
        );
    }

    /// A subagent's history is delegated work: it has no branch to take, and
    /// its id is not a session the session knows how to seed from.
    #[test]
    fn branchable_rejects_a_parent_that_is_not_a_session() {
        let state = state_with_sub_session(AgentStatus::Idle);
        assert!(
            branchable(id(9), &state, id(1), "go").is_ok(),
            "a sub_session sub_sessions"
        );
        assert_eq!(
            branchable(id(9), &state, id(7), "go").unwrap_err(),
            "only a session can be branched"
        );
    }

    /// A summariser call in flight must not be unloaded out from under itself.
    #[test]
    fn a_sub_session_mid_seed_keeps_the_session_loaded() {
        assert!(SubSessions::busy(&state_with_sub_session(
            AgentStatus::Provisioning
        )));
        assert!(!SubSessions::busy(&state_with_sub_session(
            AgentStatus::Idle
        )));
        assert!(!SubSessions::busy(&SessionState::default()));
    }

    /// Seeding is session-owned work with no journal of its own, so nothing
    /// else can finish one a dead process abandoned.
    #[test]
    fn a_sub_session_left_mid_seed_is_reseeded_at_load() {
        assert!(matches!(
            SubSessions::on_load(&state_with_sub_session(AgentStatus::Provisioning)),
            Some(SessionCommand::SubSession(
                SubSessionCommand::ReseedInterrupted
            ))
        ));
        assert!(
            SubSessions::on_load(&state_with_sub_session(AgentStatus::Idle)).is_none(),
            "a seeded sub_session has nothing to repair"
        );
    }

    #[test]
    fn the_fold_tracks_a_sub_session_through_its_life() {
        let mut state = SessionState::default();
        state.forest.apply_root_agent(
            id(9),
            0,
            crate::sessions::run_forest::RuntimeChoice::Pending,
        );
        SubSessions::apply(
            &mut state,
            &SessionDomainEvent::SubSessionCreated {
                at_ms: 1,
                id: id(1),
                parent: id(9),
                source_seq: 12,
                seed: SeedMode::Copy,
                message: "go".into(),
                title: "Other migration".into(),
                runtime: crate::sessions::run_forest::RuntimeChoice::Inherit,
            },
        );
        assert_eq!(
            state.forest.sub_session(id(1)).unwrap().status,
            AgentStatus::Provisioning
        );
        SubSessions::apply(
            &mut state,
            &SessionDomainEvent::SubSessionSeeded {
                at_ms: 2,
                id: id(1),
            },
        );
        assert_eq!(
            state.forest.sub_session(id(1)).unwrap().status,
            AgentStatus::Idle
        );
        assert_eq!(
            state.forest.sub_session(id(1)).unwrap().title,
            "Other migration"
        );
        // Removal is `SessionCore`'s: one event for both kinds of agent, and
        // the fold takes the whole subtree, which can span both.
        crate::sessions::session_actor::core::SessionCore::apply(
            &mut state,
            &SessionDomainEvent::AgentDeleted {
                at_ms: 4,
                id: id(1),
            },
        );
        assert!(state.forest.sub_session(id(1)).is_none());
    }

    // ---- integration, over the real actors ----

    use super::super::testing::{
        BlockingProvider, EchoProvider, FailOnNeedleProvider, agent_history, send,
        spawn_session_with_provider, spawn_sub, turn_outcomes, turns_begun, wait_for_state,
    };
    use crate::sessions::addressing::SessionRef;
    use crate::sessions::session_actor::{SessionCommand, TurnCommand};
    use std::sync::Arc;

    /// Type `text` at `agent_id` and hand back what the sub session command
    /// answered.
    async fn branch_via(
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
                    artifacts: Vec::new(),
                })
            })
            .await
            .unwrap()
            .map(|a| {
                a.sub_session
                    .expect("a sub session command answers with a sub session")
            })
    }

    /// Every text an agent's log holds, joined — enough to ask whether the
    /// session came across.
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
    /// Counted rather than merely looked for: a *copy* sub session is seeded
    /// with its source's log, boundaries and all, so "the log holds a
    /// `TurnEnded`" is true before the sub session has run at all.
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
    /// arrives already closed, so a floor would pass on a sub session whose
    /// own turn never ends — which is the whole thing under test.
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
    /// a source held mid-turn — so the first end to appear is the sub
    /// session's.
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

    /// A sub session's page folds its own log: `TurnBegan` reads `Running` and
    /// only a `TurnEnded` clears it. Without one the page says `RUNNING` for
    /// ever — through reloads *and* restarts, because the status is derived
    /// from the journal rather than from anything live.
    #[tokio::test]
    async fn a_sub_sessions_turn_ends_in_its_own_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // The source's turn has to be *closed* before branching, because this
        // test's premise is that the copy carries a closed turn over. A sub
        // session taken between the source's answer and its `TurnEnded` seeds
        // an unmatched `TurnBegan` — a real hazard, and not the one under
        // test.
        wait_for_turn_end(&session, None, 1).await;

        let sub_session = branch_via(&session, None, "/fork try the other migration")
            .await
            .expect("a sub session");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;

        // Two: the source's one turn, which the copy carried over already
        // closed, and the sub session's own answer to the message that created
        // it.
        assert!(
            matches!(
                wait_for_turn_end(&session, Some(sub_session.clone()), 2).await,
                horsie_agentcore::TurnOutcome::Ended(_)
            ),
            "a sub_session's turn ends like any other session's: {}",
            transcript(&session, Some(sub_session)).await
        );
    }

    /// A sub session working is not the session working. The two statuses are
    /// read off different things — the roster and `state.status` — and a
    /// client shows them side by side, so a sub session's turn must move
    /// exactly one of them.
    #[tokio::test]
    async fn a_sub_sessions_turn_moves_the_sub_sessions_status_and_not_the_sessions() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // Closed before branching: a sub session taken between the source's
        // answer and its `TurnEnded` seeds an unmatched `TurnBegan`, which is
        // a real hazard but not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let sub_session = branch_via(&session, None, "/fork try the other migration")
            .await
            .expect("a sub session");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_turn_end(&session, Some(sub_session.clone()), 2).await;

        let state = wait_for_state(&journal, id, "the sub_session settles", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|r| r.status == AgentStatus::Idle && r.last_activity_ms > 0)
        })
        .await;
        assert_eq!(
            state.status(),
            crate::sessions::spec::SessionStatus::Idle,
            "the session's own status belongs to its main agent"
        );
    }

    /// The reason a sub session's turn failed has one place a reader will look
    /// for it: the sub session's own page. It used to be dropped with a
    /// warning, so a sub session whose turn broke went on reading `RUNNING`
    /// and said nothing about why.
    #[tokio::test]
    async fn a_sub_sessions_failed_turn_says_so_in_its_own_log() {
        let provider = FailOnNeedleProvider {
            needle: "the doomed branch".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        send(&session, "the original question").await;
        // Closed before branching: a sub session taken between the source's
        // answer and its `TurnEnded` seeds an unmatched `TurnBegan`, which is
        // a real hazard but not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let sub_session = branch_via(&session, None, "/fork the doomed branch")
            .await
            .expect("a sub session");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;

        let outcome = wait_for_turn_end(&session, Some(sub_session.clone()), 2).await;
        let horsie_agentcore::TurnOutcome::Failed(failed) = &outcome else {
            panic!(
                "a sub_session's failed turn ends as failed, not {outcome:?}: {}",
                transcript(&session, Some(sub_session)).await
            );
        };
        assert!(failed.error.contains("bad key"), "{:?}", failed.error);
    }

    /// Stop, addressed to a sub session.
    ///
    /// It used to be addressed to nothing: the gate read the *session's*
    /// status, which a sub session never moves, so pressing Stop on a sub
    /// session's page returned `200` having done nothing at all. The sub
    /// session went on working, and there was no way to interrupt it.
    #[tokio::test]
    async fn stopping_a_sub_session_cancels_that_sub_sessions_turn() {
        let provider = BlockingProvider::new();
        let (_f, session, id, journal) =
            spawn_session_with_provider(provider.clone() as Arc<dyn horsie_agentcore::LlmProvider>)
                .await;
        // The source's turn is held open too, so nothing about this test can
        // pass by stopping the main agent instead.
        send(&session, "the original question").await;

        let sub_session = branch_via(&session, None, "/fork try the other migration")
            .await
            .expect("a sub session");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the sub_session is working", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|r| r.status == AgentStatus::Running)
        })
        .await;

        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::Stop {
                    agent_id: sub_session.clone(),
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("a working sub session is stoppable");

        // Any end in this log is the sub session's own: the source is
        // deliberately held mid-turn, so the history the copy carried has an
        // *open* turn in it and no boundary of its own.
        let outcome = wait_for_any_turn_end(&session, Some(sub_session.clone())).await;
        assert!(
            matches!(outcome, horsie_agentcore::TurnOutcome::Stopped(_)),
            "the sub_session's turn ends as stopped, not {outcome:?}: {}",
            transcript(&session, Some(sub_session)).await
        );
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.status(),
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

    /// The whole of `/fork`: the sub session exists, carries what was said
    /// before it, and answers the message that created it.
    #[tokio::test]
    async fn a_sub_session_carries_the_history_and_answers_its_own_message() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // Closed before branching: a sub session taken between the source's
        // answer and its `TurnEnded` seeds an unmatched `TurnBegan`, which is
        // a real hazard but not the one under test.
        wait_for_turn_end(&session, None, 1).await;

        let sub_session = branch_via(&session, None, "/fork try the other migration")
            .await
            .expect("a sub session");

        // Seeded, not merely created: `Idle` is what the seed landing
        // produces, and it is what releases the message waiting in the sub
        // session's queue.
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;

        let branched = transcript(&session, Some(sub_session.clone())).await;
        assert!(
            branched.contains("the original question"),
            "a copy sub_session carries the session it came from: {branched}"
        );
        assert!(
            branched.contains("try the other migration"),
            "and holds the message that created it: {branched}"
        );
        // Where it came from is its system prompt's to say, not its
        // transcript's: a copy seeds no message at all, so nothing stands
        // between the copied history and the brief.
        assert!(
            !branched.contains("set_session_title"),
            "a copy sub_session is seeded no orientation message: {branched}"
        );
    }

    /// A summary sub session starts small. That is the entire reason to ask
    /// for one, so the source's messages must *not* be in its log.
    #[tokio::test]
    async fn a_summary_sub_session_does_not_carry_the_source_messages() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "a very long session about migrations").await;

        let sub_session = branch_via(&session, None, "/summary-n-fork now do the other thing")
            .await
            .expect("a sub session");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the summary sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;

        let branched = transcript(&session, Some(sub_session.clone())).await;
        assert!(
            !branched.contains("a very long session about migrations"),
            "a summary sub_session discards the history it summarised: {branched}"
        );
        assert!(
            branched.contains("# Summary of the session this was branched from"),
            "and carries the summary it was seeded with instead: {branched}"
        );
    }

    /// The summary is the source's **own turn**, not a detached read of it.
    ///
    /// This is the whole point of the redesign. Run out of band, the
    /// summariser left the source `Idle` and answering, so a reply sent while
    /// it ran landed after the `Branched` marker in the source's transcript
    /// and inside the sub session's summary — the two described different
    /// sessions. Queued, the source cannot append while the summary is taken,
    /// and the proof that it is a turn is that the source's own log carries
    /// one.
    #[tokio::test]
    async fn summarising_for_a_sub_session_is_a_turn_on_the_session_it_branches() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        let before = main_turns_begun(&session).await;

        let sub_session = branch_via(&session, None, "/summary-n-fork now do the other thing")
            .await
            .expect("a sub session");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the summary sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
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
    /// shows where each sub session left.
    #[tokio::test]
    async fn the_source_transcript_records_where_a_sub_session_left() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "first").await;
        let sub_session = branch_via(&session, None, "/fork branch here")
            .await
            .expect("a sub session");

        for _ in 0..200 {
            if transcript(&session, None).await.contains(&sub_session) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "the source never recorded the branch: {}",
            transcript(&session, None).await
        );
    }

    /// A subagent's session is delegated work, not a branch to take.
    #[tokio::test]
    async fn only_a_session_can_be_branched() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, "research", "dig").await;

        let err = branch_via(&session, Some(sub.to_string()), "/fork off you go")
            .await
            .expect_err("a subagent cannot be branched");
        assert!(
            matches!(err, crate::sessions::UserMessageError::Rejected(ref m)
                if m.contains("only a session")),
            "{err:?}"
        );
    }

    /// A sub session with nothing to do is a sub session nobody comes back to.
    #[tokio::test]
    async fn a_sub_session_needs_a_message() {
        let (_f, session, _id, _journal) =
            spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let err = branch_via(&session, None, "/fork")
            .await
            .expect_err("a bare sub_session is refused");
        assert!(
            matches!(err, crate::sessions::UserMessageError::Rejected(ref m)
                if m.contains("needs a message")),
            "{err:?}"
        );
    }

    /// Sub sessions nest: a sub session of a sub session records the sub
    /// session it came from, not main.
    #[tokio::test]
    async fn a_sub_session_of_a_sub_session_records_the_sub_session_it_came_from() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "start").await;

        let first = branch_via(&session, None, "/fork one")
            .await
            .expect("a sub session");
        let first_id = Uuid::parse_str(&first).unwrap();
        wait_for_state(&journal, id, "the first sub_session is seeded", |s| {
            s.forest
                .sub_session(first_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;

        let second = branch_via(&session, Some(first.clone()), "/fork two")
            .await
            .expect("a sub session of a sub session");
        let second_id = Uuid::parse_str(&second).unwrap();
        let state = wait_for_state(&journal, id, "the second sub_session exists", |s| {
            s.forest.sub_session(second_id).is_some()
        })
        .await;
        assert_eq!(
            state.forest.owner_of_agent(second_id).unwrap().1.parent,
            Some(first_id),
            "a sub_session of a sub_session is rooted on that sub_session"
        );
    }

    /// The model hands work to a second session with `spawn_subsession`.
    ///
    /// Mid-turn, which the composer's `/fork` never is — and the reason the
    /// tool's sub session carries no history: a copy cut here would end on the
    /// assistant message holding the very call that asked for it, unanswered.
    /// A `Fresh` sub session starts on the brief instead, and there is nothing
    /// to cut.
    #[tokio::test]
    async fn the_model_can_hand_work_to_a_sub_session() {
        use horsie_agentcore::{
            StopReason,
            testkit::{MockProvider, Script},
        };
        // The main agent's first call sub sessions. Everything after —
        // including the sub session's own turn — answers with plain text.
        let provider = MockProvider::scripted(
            Script::of([Ok(horsie_agentcore::CompletionResponse {
                parts: vec![horsie_agentcore::ContentPart::ToolCall(
                    horsie_agentcore::ToolCallPart {
                        id: "sub_session-1".into(),
                        name: crate::sessions::sub_session_tool::SPAWN_SUBSESSION_TOOL.into(),
                        input: serde_json::json!({
                            "title": "the materialised view",
                            "task": "try the materialised view"
                        }),
                    },
                )],
                stop_reason: StopReason::ToolUse,
                usage: horsie_agentcore::Usage::without_cache(1, 1),
            })])
            .then_repeating_with(|| {
                Ok(horsie_agentcore::CompletionResponse {
                    parts: vec![horsie_agentcore::ContentPart::Text(
                        horsie_agentcore::TextPart {
                            text: "answered".to_string(),
                        },
                    )],
                    stop_reason: StopReason::EndTurn,
                    usage: horsie_agentcore::Usage::without_cache(1, 1),
                })
            }),
        );
        let (_f, session, id, journal) = spawn_session_with_provider(provider).await;

        send(&session, "start").await;
        let state = wait_for_state(&journal, id, "the model's sub_session is seeded", |s| {
            s.forest.sub_session_ids().iter().any(|f| {
                s.forest
                    .sub_session(*f)
                    .is_some_and(|r| r.status != AgentStatus::Provisioning)
            })
        })
        .await;
        let sub_session_id = state.forest.sub_session_ids()[0];
        let rec = state.forest.sub_session(sub_session_id).unwrap();
        assert_eq!(rec.seed, SeedMode::Fresh);
        assert_eq!(rec.message, "try the materialised view");
        assert_eq!(
            state
                .forest
                .owner_of_agent(sub_session_id)
                .unwrap()
                .1
                .parent,
            Some(id),
            "a sub_session the main agent asked for is rooted on the main agent"
        );

        // The sub session runs on the brief it was handed, and on nothing
        // else: the source said "start" and the sub session's transcript never
        // mentions it.
        for _ in 0..200 {
            let t = transcript(&session, Some(sub_session_id.to_string())).await;
            if t.contains("answered") {
                assert!(
                    t.contains("try the materialised view"),
                    "the sub_session was given its brief: {t}"
                );
                assert!(
                    !t.contains("\"start\""),
                    "a fresh sub_session carries none of the source's history: {t}"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "the sub_session never ran: {}",
            transcript(&session, Some(sub_session_id.to_string())).await
        );
    }

    /// A sub session's turn runs with the `# Sub session` section, naming the
    /// session it branched from and what that mode left it holding.
    ///
    /// Asserted on the prompt the session actually handed the provider, not on
    /// `sub_session_prompt`: the section is built from an origin snapshotted at
    /// spawn, and a spawn that forgot to fill it would compile, pass every unit
    /// test, and run every sub session with no section at all.
    #[tokio::test]
    async fn a_sub_sessions_turn_carries_the_sub_session_section() {
        let provider =
            Arc::new(crate::sessions::session_actor::testing::PromptRecordingProvider::default());
        let (_f, session, id, journal) = spawn_session_with_provider(provider.clone()).await;
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;

        let sub_session = branch_via(&session, None, "/fork try the other migration")
            .await
            .expect("a sub session");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;

        for _ in 0..200 {
            if let Some(prompt) = provider
                .prompts()
                .into_iter()
                .find(|p| p.contains("# Sub session"))
            {
                assert!(
                    prompt.contains("carry \"untitled session\"'s history")
                        || prompt.contains("'s history up to the moment you were branched"),
                    "a copy is told what it carries: {prompt}"
                );
                assert!(prompt.contains("share one workspace"), "{prompt}");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "no turn ran with the sub session section: {:?}",
            provider.prompts()
        );
    }

    /// Branching a session that is parked on a question.
    ///
    /// The copied log carries the `ask_user` `tool_use` with no result. A
    /// dangling call 400s every provider, so what makes this work is the
    /// sanitization every turn start already runs — this is the proof that a
    /// sub session's first turn goes through it like any other.
    ///
    /// Note what this does *not* prove: the sub session would run even if
    /// `asks` were carried, because its own queued message is a person
    /// speaking and that overrides a park by design. Dropping `asks` is
    /// defensive, not what makes this pass.
    #[tokio::test]
    async fn a_sub_session_of_a_parked_session_runs_rather_than_inheriting_the_question() {
        use horsie_agentcore::{
            StopReason,
            testkit::{MockProvider, Script},
        };
        // The source's first call asks the user and parks. Everything after —
        // including every call the sub session makes — answers with plain text.
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
                            text: "the sub_session answered".to_string(),
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
                s.status(),
                crate::sessions::spec::SessionStatus::AwaitingInput
            )
        })
        .await;

        let sub_session = branch_via(&session, None, "/fork never mind, do the other thing")
            .await
            .expect("a parked session can still be branched");
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;

        // The question is *visible* in the copied transcript — it happened —
        // but the sub session is not waiting on it, so its own turn runs to an
        // answer.
        for _ in 0..200 {
            let t = transcript(&session, Some(sub_session.clone())).await;
            if t.contains("the sub_session answered") {
                assert!(
                    t.contains("which migration?"),
                    "the question is still readable in the copied history: {t}"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "the sub_session never answered: {}",
            transcript(&session, Some(sub_session)).await
        );
    }

    /// Branching a session with a message still queued on it.
    ///
    /// This is the drop that is genuinely load-bearing. A message the source
    /// has accepted but not yet answered belongs to the *source*: it queued
    /// because a turn was in flight, and that turn's boundary is what answers
    /// it. Copied into the sub session, both sessions answer it — the person
    /// gets two replies to one message, and the sub session's first turn is
    /// polluted by a message that was never meant for it.
    #[tokio::test]
    async fn a_sub_session_does_not_take_over_a_message_queued_on_the_source() {
        use super::super::testing::BlockingProvider;

        let provider = BlockingProvider::new();
        let (_f, session, id, journal) = spawn_session_with_provider(provider.clone()).await;

        // Hold the source inside a turn, so the next message queues rather
        // than draining.
        send(&session, "the turn that is running").await;
        wait_for_state(&journal, id, "the source is running", |s| {
            matches!(s.status(), crate::sessions::spec::SessionStatus::Running)
        })
        .await;
        send(&session, "QUEUED-FOR-THE-SOURCE").await;

        let sub_session = branch_via(&session, None, "/fork the sub_session's own instruction")
            .await
            .expect("a busy session can still be branched");

        // The sub session's *queue* must not hold it. The source's log records
        // that the message was queued — that happened, and the copied history
        // says so — but the sub session must not be the one to answer it, so
        // it is not an `Incoming` the sub session will merge into a turn.
        let sub_session_id = Uuid::parse_str(&sub_session).unwrap();
        wait_for_state(&journal, id, "the sub_session is seeded", |s| {
            s.forest
                .sub_session(sub_session_id)
                .is_some_and(|f| f.status != AgentStatus::Provisioning)
        })
        .await;
        let branched = transcript(&session, Some(sub_session.clone())).await;
        assert!(
            !branched.contains("Received") && !branched.contains("QUEUED-FOR-THE-SOURCE\", "),
            "the source's queued message is not the sub_session's to answer: {branched}"
        );
        // And the copy stops at the branch point: the `Branched` entry
        // recording this very sub session is written onto the *source* after
        // the branch, so a copy taken at the log's end would hand the sub
        // session a marker pointing at itself.
        assert!(
            !branched.contains("Forked("),
            "a sub_session must not carry its own creation marker: {branched}"
        );

        provider.release();
    }

    /// Only a summary seeds a message, and that message is the summary.
    ///
    /// The other two modes seed nothing: a copy's history is the context and a
    /// fresh sub session's brief is queued behind the seed, so a message here
    /// would be a second telling of something already in hand — and it read as
    /// one the person had typed.
    #[test]
    fn only_a_summary_seeds_a_message() {
        assert!(summary_message(&Seed::Copy).is_none());
        assert!(summary_message(&Seed::Fresh).is_none());

        let summarised = summary_message(&Seed::Summary("We chose sqlx::Any.".to_string()))
            .expect("a summary seeds its summary");
        let ContentPart::Text(text) = &summarised.parts[0] else {
            panic!("the summary is text");
        };
        assert!(
            text.text
                .contains("# Summary of the session this was branched from")
        );
        assert!(text.text.contains("We chose sqlx::Any."));
        // Nothing about being a sub session, and nothing about naming itself:
        // both are the system prompt's, and saying either here is what made
        // this message read as orientation rather than as content.
        assert!(!text.text.contains("set_session_title"));
        assert!(!text.text.contains("branched from \""));
    }
}
