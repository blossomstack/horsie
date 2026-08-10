//! Answering questions without waking anything.
//!
//! Every read is served from the resident actor's own memory, or forwarded to
//! the agent that owns the transcript. None of them touches the journal, so
//! opening a session to look at it costs no sandbox — which is what lets a
//! browser poll a session that is otherwise idle.
//!
//! No events and no state: this component only ever answers.

use super::{
    AgentUsageEntry, CommandEffect, MAIN_AGENT_ID, ReadCommand, SessionActor, SessionDomainEvent,
    SessionSnapshot, SessionState, SessionUsageStats,
};
use crate::agent_loop::AgentCommand;
use crate::agent_loop::AgentUsageSnapshot;
use crate::agent_loop::UsageTotal;
use crate::sessions::session_actor::SessionCommand;
use horsie_actor::ActorContext;

/// Reads.
pub(super) struct Reads;

impl Reads {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: ReadCommand,
        ctx: &ActorContext<SessionCommand>,
    ) -> CommandEffect<SessionDomainEvent> {
        match cmd {
            ReadCommand::ReadLog {
                agent_id,
                after,
                reply,
            } => {
                // Read from the resident actor's in-memory state. No journal
                // access, no runtime — opening a session to read it stays free
                // of sandbox cost.
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
                let out = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::ReadLog { after, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(out);
                CommandEffect::none()
            }
            ReadCommand::PageLog {
                agent_id,
                before,
                max,
                reply,
            } => {
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
                let page = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::PageLog { before, max, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(page);
                CommandEffect::none()
            }
            ReadCommand::AgentState { agent_id, reply } => {
                let agent = actor.resolve_agent(state, ctx, agent_id.as_deref());
                let view = match agent {
                    Some((_, agent)) => agent
                        .ask(|reply| AgentCommand::GetState { reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(view);
                CommandEffect::none()
            }
            ReadCommand::Snapshot { reply } => {
                let _ = reply.send(SessionSnapshot {
                    status: state.status.clone(),
                });
                CommandEffect::none()
            }
            ReadCommand::UsageStats { reply } => {
                let stats = actor.read_usage(state).await;
                let _ = reply.send(stats);
                CommandEffect::none()
            }
        }
    }
}

/// Handlers that belong to this component but act on the actor's own
/// fields — the roster, the supervisor link, the spawn helpers. An inherent
/// `impl` in a child module sees them, so moving the code needed no plumbing.
impl SessionActor {
    /// Aggregated usage. Totals come from this session's own durable record;
    /// only the live context size is asked of the agent.
    pub(super) async fn read_usage(&self, state: &SessionState) -> SessionUsageStats {
        let snapshot = match self.agent() {
            Some(agent) => agent
                .ask(|reply| AgentCommand::GetUsage { reply })
                .await
                .unwrap_or_default(),
            None => AgentUsageSnapshot::default(),
        };
        let main_usage_total = state
            .agent_usage
            .get(MAIN_AGENT_ID)
            .copied()
            .unwrap_or_default();
        let session_total = state
            .agent_usage
            .values()
            .fold(UsageTotal::default(), |acc, u| acc.combine(u));
        SessionUsageStats {
            session_total,
            agents: state.agent_usage.clone(),
            main_agent: AgentUsageEntry {
                model: self.spec.agent.model.clone(),
                snapshot: AgentUsageSnapshot {
                    usage_total: main_usage_total,
                    last_turn_usage: snapshot.last_turn_usage,
                    context_tokens: snapshot.context_tokens,
                },
            },
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
    //! That a read answers from memory and never from the journal.
    use super::super::testing::*;
    use super::super::*;
    use super::*;

    use horsie_agentcore::LlmProvider;

    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn usage_is_recorded_per_agent() {
        let s = fold(vec![SessionDomainEvent::UsageRecorded {
            at_ms: 0,
            agent_id: MAIN_AGENT_ID.to_string(),
            usage_total: UsageTotal {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_tokens: None,
                cache_read_tokens: None,
            },
        }]);
        assert_eq!(s.agent_usage.get(MAIN_AGENT_ID).unwrap().input_tokens, 10);
    }

    /// The invariant the old two-vocabulary design could not even state, now
    /// nearly a tautology: reading forward and paging backwards return the same
    /// entries, in the same order, because there is one log and one writer.
    ///
    /// Worth keeping precisely because it used to be hard. Two projections of
    /// one append-only log could disagree when one of them was a broadcast a
    /// subscriber might have joined late; neither of these can.
    #[tokio::test]
    async fn reading_forward_and_paging_back_agree_on_the_log() {
        let (_f, session, id, journal) = spawn_session_with_provider(Arc::new(EchoProvider)).await;
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "go".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_journal_len(&journal, id, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let streamed: Vec<u64> = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::ReadLog {
                    agent_id: None,
                    after: None,
                    reply,
                })
            })
            .await
            .unwrap()
            .expect("main agent log")
            .entries
            .iter()
            .map(|e| e.seq)
            .collect();
        let stored: Vec<u64> = main_history(&session)
            .await
            .entries
            .iter()
            .map(|e| e.seq)
            .collect();
        assert!(!streamed.is_empty(), "the turn must produce entries");
        assert_eq!(streamed, stored);
        assert_eq!(
            streamed,
            (0..streamed.len() as u64).collect::<Vec<_>>(),
            "no gaps and no reordering"
        );
    }

    /// Reads and streams are served from actor state. The journal is touched
    /// only while an actor recovers — never to answer a query.
    #[tokio::test]
    async fn serving_reads_never_touches_the_journal() {
        let f = actor_fixture().await;
        let id = Uuid::new_v4();
        f.deps
            .runtimes
            .create(&id.to_string(), "mock", &actor_spec_fixture())
            .await
            .expect("create");
        f.deps.provider_registry.write().unwrap().insert(
            "mock".to_string(),
            Arc::new(EchoProvider) as Arc<dyn LlmProvider>,
        );
        let counting = Arc::new(CountingJournal::new());
        let journal: Arc<dyn horsie_actor::Journal> = counting.clone();
        let session =
            horsie_actor::ActorSystem::new(journal.clone()).spawn_persistent(SessionActor::new(
                id,
                actor_spec_fixture(),
                f.deps.clone(),
                spawn_deaf_supervisor(),
                crate::sessions::Positions::default(),
            ));

        // Drive one turn so both actors are loaded and have history.
        session
            .ask(|reply| {
                SessionCommand::Turn(TurnCommand::UserMessage {
                    agent_id: None,
                    text: "go".into(),
                    reply,
                })
            })
            .await
            .unwrap()
            .unwrap();
        wait_for_journal_len(&journal, id, 2).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Recovery is allowed to replay; everything after it is not.
        let after_recovery = counting.replays();
        assert!(
            after_recovery > 0,
            "the counter must actually observe recovery, or this test proves nothing"
        );

        let _ = main_history(&session).await;
        let _ = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::ReadLog {
                    agent_id: None,
                    after: Some(crate::agent_loop::Cursor {
                        entry_seq: 0,
                        delta_seq: 0,
                    }),
                    reply,
                })
            })
            .await
            .unwrap();
        let _ = session
            .ask(|reply| {
                SessionCommand::Read(ReadCommand::AgentState {
                    agent_id: None,
                    reply,
                })
            })
            .await
            .unwrap();

        assert_eq!(
            counting.replays(),
            after_recovery,
            "history and agent state must both be served from memory"
        );
    }
}
