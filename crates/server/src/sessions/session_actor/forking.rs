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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::super::testing::{
        BlockingProvider, EchoProvider, FailOnNeedleProvider, agent_history, send,
        spawn_session_with_provider, spawn_sub, turn_outcomes, turns_begun, wait_for_state,
    };
    use super::super::{AgentStatus, MAIN_AGENT_ID, SessionCommand, TurnCommand};
    use super::*;
    use crate::sessions::addressing::SessionRef;
    use std::sync::Arc;
    use uuid::Uuid;

    /// This fork's projected status, off the folded state.
    fn fork_status(state: &SessionState, id: Uuid) -> Option<AgentStatus> {
        fork_of(state, AgentId(id)).map(|(f, _)| f.agent_status())
    }

    /// Whether this fork's history has landed and it may run.
    fn fork_seeded(state: &SessionState, id: Uuid) -> bool {
        fork_status(state, id).is_some_and(|s| s != AgentStatus::Provisioning)
    }

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
    /// ended. A page folds exactly this pair — an unmatched `TurnBegan` is
    /// what reads `RUNNING` for ever.
    async fn turn_boundaries(
        session: &SessionRef,
        agent_id: Option<String>,
    ) -> (usize, Vec<horsie_agentcore::TurnOutcome>) {
        let page = agent_history(session, agent_id).await;
        (turns_begun(&page), turn_outcomes(&page))
    }

    /// Wait until `agent_id`'s log holds `turns` turns, all of them closed, and
    /// hand back how the last one ended.
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
    /// `TurnEnded` clears it.
    #[tokio::test]
    async fn a_forks_turn_ends_in_its_own_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        // The source's turn has to be *closed* before forking: the copy
        // carries a closed turn over.
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            fork_seeded(s, fork_id)
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

    /// A fork working is not the session working: a fork's turn must move
    /// exactly one of the two statuses a client shows side by side.
    #[tokio::test]
    async fn a_forks_turn_moves_the_forks_status_and_not_the_sessions() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_turn_end(&session, Some(fork.clone()), 2).await;

        let state = wait_for_state(&journal, id, "the fork settles", |s| {
            fork_status(s, fork_id) == Some(AgentStatus::Idle)
                && fork_of(s, AgentId(fork_id)).is_some_and(|(f, _)| f.last_activity_ms > 0)
        })
        .await;
        assert_eq!(
            state.status(),
            crate::sessions::spec::SessionStatus::Idle,
            "the session's own status belongs to its main agent"
        );
    }

    /// The reason a fork's turn failed has one place a reader will look for
    /// it: the fork's own page.
    #[tokio::test]
    async fn a_forks_failed_turn_says_so_in_its_own_log() {
        let provider = FailOnNeedleProvider {
            needle: "the doomed branch".to_string(),
        };
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(provider)).await;
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork the doomed branch")
            .await
            .expect("a fork");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            fork_seeded(s, fork_id)
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

    /// Stop, addressed to a fork. It used to be addressed to nothing: the gate
    /// read the *session's* status, which a fork never moves.
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
            fork_status(s, fork_id) == Some(AgentStatus::Running)
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
        // held mid-turn, so the copied history has an *open* turn in it and no
        // boundary of its own.
        let outcome = wait_for_any_turn_end(&session, Some(fork.clone())).await;
        assert!(
            matches!(outcome, horsie_agentcore::TurnOutcome::Stopped(_)),
            "the fork's turn ends as stopped, not {outcome:?}: {}",
            transcript(&session, Some(fork)).await
        );
        let state = crate::sessions::events::fold_session_state(&journal, id).await;
        assert_eq!(
            state.status(),
            crate::sessions::spec::SessionStatus::Running,
            "the source's own turn is untouched — it was not what was stopped"
        );
        provider.release();
    }

    /// An id that names no agent here is a refusal; an agent that simply is
    /// not working is `Ok`, so a client racing a turn's own end is not told it
    /// failed for winning the race.
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
            stop(MAIN_AGENT_ID.to_string()).await.is_ok(),
            "an agent with nothing in flight is not a failure"
        );
    }

    /// The whole of `/fork`: the fork exists, carries what was said before it,
    /// and answers the message that created it.
    #[tokio::test]
    async fn a_fork_carries_the_conversation_and_answers_its_own_message() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        send(&session, "the original question").await;
        wait_for_turn_end(&session, None, 1).await;

        let fork = fork_via(&session, None, "/fork try the other migration")
            .await
            .expect("a fork");

        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            fork_seeded(s, fork_id)
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
            fork_seeded(s, fork_id)
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

    /// The summary is the source's **own turn**, not a detached read of it:
    /// queued, the source cannot append while the summary is taken, and the
    /// proof that it is a turn is that the source's own log carries one.
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
            fork_status(s, fork_id) == Some(AgentStatus::Idle)
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
        let (_f, session, id, _journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        let sub = spawn_sub(&session, id, "research", "dig").await;

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
            fork_seeded(s, first_id)
        })
        .await;

        let second = fork_via(&session, Some(first.clone()), "/fork two")
            .await
            .expect("a fork of a fork");
        let second_id = Uuid::parse_str(&second).unwrap();
        let state = wait_for_state(&journal, id, "the second fork exists", |s| {
            fork_of(s, AgentId(second_id)).is_some()
        })
        .await;
        assert_eq!(
            fork_of(&state, AgentId(second_id)).unwrap().1,
            Some(AgentId(first_id)),
            "a fork of a fork is rooted on that fork"
        );
    }

    /// Forking a conversation that is parked on a question: the fork's own
    /// queued message is a person speaking, which overrides a park by design.
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
                s.status(),
                crate::sessions::spec::SessionStatus::AwaitingInput
            )
        })
        .await;

        let fork = fork_via(&session, None, "/fork never mind, do the other thing")
            .await
            .expect("a parked conversation can still be forked");
        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            fork_seeded(s, fork_id)
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

    /// Forking a conversation with a message still queued on it: the queued
    /// message belongs to the *source*, and the fork must not be the one to
    /// answer it.
    #[tokio::test]
    async fn a_fork_does_not_take_over_a_message_queued_on_the_source() {
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

        let fork = fork_via(&session, None, "/fork the fork's own instruction")
            .await
            .expect("a busy conversation can still be forked");

        let fork_id = Uuid::parse_str(&fork).unwrap();
        wait_for_state(&journal, id, "the fork is seeded", |s| {
            fork_seeded(s, fork_id)
        })
        .await;
        let forked = transcript(&session, Some(fork.clone())).await;
        assert!(
            !forked.contains("Received") && !forked.contains("QUEUED-FOR-THE-SOURCE\", "),
            "the source's queued message is not the fork's to answer: {forked}"
        );
        // And the copy stops at the branch point: the `Forked` entry recording
        // this very fork is written onto the *source* after the branch.
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
