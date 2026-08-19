//! Branching a conversation: fork creation, seeding, titles and deletion.
//!
//! Persist-then-spawn, exactly as a subagent spawn: the fork's `Created` event
//! is durable before its actor exists, so a crash between the two replays as a
//! fork still seeding, which recovery re-seeds. Strictly better than an
//! untracked agent.

use super::runner::event::{RunnerArgs, RunnerEvent};
use super::runner::state::{ForkState, RunnerState, SeedPhase};
use super::{
    AgentId, CommandEffect, ForkCommand, RunnerId, SessionActor, SessionCommand, SessionEvent,
    SessionState,
};
use crate::agent_loop::{AgentCommand, AgentState, Incoming};
use crate::sessions::addressing::SessionInbox;
use crate::sessions::forks::ForkMode;
use horsie_actor::{ActorContext, ActorRef, ReplyTo};
use horsie_agentcore::{ContentPart, Message, Role, TextPart};
use horsie_models::now_ms;
use tokio::sync::oneshot;
use uuid::Uuid;

impl SessionActor {
    pub(super) async fn handle_fork(
        &mut self,
        state: &SessionState,
        cmd: ForkCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            ForkCommand::Create {
                parent,
                mode,
                message,
                reply,
            } => {
                // The branch point, read before anything is written: where the
                // source's log stands right now is what this fork carries.
                let Some(source_seq) = self.source_log_head(state, ctx, parent).await else {
                    let _ =
                        reply.send(Err("the conversation to fork is not available".to_string()));
                    return CommandEffect::none();
                };
                let id = AgentId(Uuid::new_v4());
                let created = SessionEvent::Runner {
                    id: RunnerId::of_agent(id),
                    at_ms: now_ms(),
                    event: RunnerEvent::Created {
                        parent: Some(parent),
                        args: Box::new(RunnerArgs::Fork {
                            source_seq,
                            mode,
                            message,
                        }),
                    },
                };
                // Persist first, spawn second — see the module doc.
                let (tx, rx) = oneshot::channel();
                let self_ref = self.me(ctx);
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
                if self.reach(id, state, ctx).is_none() {
                    let _ = reply.send(Err("could not start the fork".to_string()));
                    return CommandEffect::none();
                }
                // The message is *not* enqueued here. It rides into the same
                // write as the seed, because a fork with a message and no
                // history drains it immediately and answers a conversation it
                // has not been given yet.
                self.start_seeding(ctx, state, id);
                // The id travels now rather than when the seed lands: the
                // client redirects to a fork that is visibly building itself,
                // which is exactly what a newly created session does.
                let _ = reply.send(Ok(id.0));
                CommandEffect::none()
            }
            ForkCommand::Seeded { id } => {
                if fork_of(state, id).is_none() {
                    return CommandEffect::none();
                }
                // Through `persist_and_advance` rather than a bare persist:
                // the fork becoming ready is what releases the message queued
                // behind it, and that release is an action.
                self.persist_and_advance(
                    state,
                    vec![SessionEvent::Runner {
                        id: RunnerId::of_agent(id),
                        at_ms: now_ms(),
                        event: RunnerEvent::ForkSeeded,
                    }],
                    ctx,
                )
                .await
            }
            ForkCommand::SeedFailed { id, error } => {
                if fork_of(state, id).is_none() {
                    return CommandEffect::none();
                }
                tracing::warn!(fork = %id, error, "seeding a fork failed");
                CommandEffect::persist(vec![SessionEvent::Runner {
                    id: RunnerId::of_agent(id),
                    at_ms: now_ms(),
                    event: RunnerEvent::ForkSeedFailed { error },
                }])
            }
            ForkCommand::Summarised { forks, result } => {
                for id in forks {
                    let id = AgentId(id);
                    // Dropped rather than reported: a fork deleted while its
                    // summary was being taken is not a failure, it is the user
                    // having changed their mind.
                    if fork_of(state, id).is_none() {
                        continue;
                    }
                    match &result {
                        Ok(summary) => self.finish_seeding(ctx, state, id, summary.clone()),
                        Err(error) => {
                            let _ = self
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
            ForkCommand::SetTitle { id, title, reply } => {
                let normalized = match crate::sessions::title_tool::normalize_session_title(&title)
                {
                    Ok(t) => t,
                    Err(e) => {
                        let _ = reply.send(Err(e.to_string()));
                        return CommandEffect::none();
                    }
                };
                if fork_of(state, id).is_none() {
                    let _ = reply.send(Err(format!("no such fork: {id}")));
                    return CommandEffect::none();
                }
                let _ = reply.send(Ok(normalized.clone()));
                CommandEffect::persist(vec![SessionEvent::Runner {
                    id: RunnerId::of_agent(id),
                    at_ms: now_ms(),
                    event: RunnerEvent::ForkTitled { name: normalized },
                }])
            }
            ForkCommand::Delete { id, reply } => {
                if fork_of(state, id).is_none() {
                    let _ = reply.send(Err(format!("no such fork: {id}")));
                    return CommandEffect::none();
                }
                self.retire_fork_actor(id).await;
                let _ = reply.send(Ok(()));
                CommandEffect::persist(vec![SessionEvent::Runner {
                    id: RunnerId::of_agent(id),
                    at_ms: now_ms(),
                    event: RunnerEvent::ForkDeleted,
                }])
            }
            ForkCommand::ReseedInterrupted => {
                let seeding: Vec<AgentId> = state
                    .runners
                    .iter()
                    .filter_map(|(id, record)| match &record.state {
                        RunnerState::Fork(f) if matches!(f.seed, SeedPhase::Seeding) => {
                            Some(AgentId(id.0))
                        }
                        RunnerState::Fork(_)
                        | RunnerState::Main(_)
                        | RunnerState::Sub(_)
                        | RunnerState::Workflow(_) => None,
                    })
                    .collect();
                for id in seeding {
                    // Spawning is what a fork needs to be seeded *into*: a
                    // session that reloaded has no resident agents at all.
                    if self.reach(id, state, ctx).is_none() {
                        tracing::warn!(fork = %id, "could not restart a fork to re-seed it");
                        continue;
                    }
                    self.start_seeding(ctx, state, id);
                }
                CommandEffect::none()
            }
        }
    }

    /// One of this session's summarised forks got its summary. Delegated from
    /// the outcome routing: the summary is not the source's turn ending.
    pub(super) async fn on_summarised(
        &mut self,
        state: &SessionState,
        forks: Vec<Uuid>,
        result: Result<String, String>,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        self.handle_fork(state, ForkCommand::Summarised { forks, result }, ctx)
            .await
    }

    /// Where the source's log stands — a fork's branch point.
    async fn source_log_head(
        &mut self,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        parent: AgentId,
    ) -> Option<u64> {
        let agent = self.reach(parent, state, ctx)?;
        agent
            .ask(|reply| AgentCommand::LogHead { reply })
            .await
            .ok()
    }

    /// Start whatever this fork's mode needs before it can be seeded.
    ///
    /// A `Copy` has everything already and goes straight to the handover. A
    /// `Summary` needs a provider call over the source's history, and that
    /// call is *the source's own turn*: queued on its inbox, so accepting the
    /// command and the source becoming busy are one event. Nothing can append
    /// to the history between the branch marker and the summary, which is what
    /// makes the two describe the same conversation.
    pub(super) fn start_seeding(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: AgentId,
    ) {
        let Some((fork, parent)) = fork_of(state, id) else {
            tracing::warn!(fork = %id, "no record to seed a fork from");
            return;
        };
        match fork.mode {
            ForkMode::Copy => self.seed_fork_with(ctx, state, id, None),
            ForkMode::Summary => self.ask_source_to_summarise(ctx, state, id, parent),
        }
    }

    /// Queue the summary as a turn on the conversation being forked.
    ///
    /// The item id is derived from the fork's, not generated: a re-seed after
    /// a crash must ask for the same thing rather than queue a second summary.
    fn ask_source_to_summarise(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: AgentId,
        parent: Option<AgentId>,
    ) {
        let Some(source) = parent.and_then(|p| self.reach(p, state, ctx)) else {
            tracing::warn!(fork = %id, "no conversation to summarise for a fork");
            return;
        };
        let fork = id.0;
        tokio::spawn(async move {
            let _ = source
                .tell(AgentCommand::Enqueue {
                    item: Incoming::Fork {
                        id: format!("fork-summarise:{fork}"),
                        fork,
                    },
                    ack: None,
                })
                .await;
        });
    }

    /// Hand a fork the summary its source's turn produced.
    fn finish_seeding(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: AgentId,
        summary: String,
    ) {
        self.seed_fork_with(ctx, state, id, Some(summary));
    }

    /// Build a fork's initial state and hand it over, off the mailbox.
    ///
    /// Detached because a `Copy` seed reads the source's whole history:
    /// holding the session's mailbox for it would stall every other agent in
    /// the session. The fork runner's `busy` is what keeps the session loaded
    /// meanwhile.
    ///
    /// `summary` present means the history is not copied at all — a summary
    /// fork starts small, which is the entire point of asking for one.
    fn seed_fork_with(
        &mut self,
        ctx: &ActorContext<SessionInbox>,
        state: &SessionState,
        id: AgentId,
        summary: Option<String>,
    ) {
        // Everything this needs is on the record, and the record is what a
        // re-seed after a crash reads too — so taking it from there is what
        // makes the first attempt and the retry cut the copy at the same
        // place, from the same branch point, with the same message.
        let Some((fork_state, parent)) = fork_of(state, id) else {
            tracing::warn!(fork = %id, "no record to seed a fork from");
            return;
        };
        let (source_seq, message) = (fork_state.source_seq, fork_state.message.clone());
        let (Some(source), Some(fork)) = (
            parent.and_then(|p| self.reach(p, state, ctx)),
            self.reach(id, state, ctx),
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
    fn source_title(&self, state: &SessionState, parent: Option<AgentId>) -> String {
        let named = match parent {
            Some(p) if p == self.self_agent() => self.spec().name.clone(),
            Some(p) => fork_of(state, p).and_then(|(f, _)| f.title.clone()),
            None => None,
        };
        named.unwrap_or_else(|| "the conversation before this one".to_string())
    }

    /// Stop a fork's actor, if it is resident, and forget it.
    ///
    /// Best effort: a fork that is not resident has nothing to stop, and the
    /// `ForkDeleted` that follows is what makes the removal durable either
    /// way.
    async fn retire_fork_actor(&mut self, id: AgentId) {
        let Some(agent) = self.agents.as_mut().and_then(|a| a.remove(id)) else {
            return;
        };
        agent.actor.stop().await;
    }
}

/// The fork behind `id`, and the agent it branched from.
fn fork_of(state: &SessionState, id: AgentId) -> Option<(&ForkState, Option<AgentId>)> {
    let record = state.record(RunnerId::of_agent(id))?;
    match &record.state {
        RunnerState::Fork(f) => Some((f, record.parent)),
        RunnerState::Main(_) | RunnerState::Sub(_) | RunnerState::Workflow(_) => None,
    }
}

/// Build a fork's history from its source and hand it over.
///
/// Both modes end with one synthetic `Role::User` message carrying a `fork:`
/// id — the device compaction already uses for `compaction:{n}`, so
/// `prompt_messages` needs no change and a client special-cases an id prefix
/// it already special-cases.
async fn seed_fork(
    source: &ActorRef<AgentCommand>,
    fork: &ActorRef<AgentCommand>,
    summary: Option<String>,
    source_seq: u64,
    source_title: &str,
    message: Incoming,
) -> Result<(), String> {
    // A summary fork copies nothing: it starts small, which is the entire
    // point of asking for one. Only a copy reads the source, and only at the
    // branch point — the source goes on appending while this runs, and a copy
    // to the log's end would hand the fork its own creation marker.
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
