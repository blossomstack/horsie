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
use horsie_actor::ActorContext;
use horsie_workflow::AgentCommand;
use horsie_workflow::AgentUsageSnapshot;
use horsie_workflow::UsageTotal;

/// Reads.
pub(super) struct Reads;

impl Reads {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: ReadCommand,
        ctx: &ActorContext<SessionActor>,
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
                    inbox: state.inbox.clone(),
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
