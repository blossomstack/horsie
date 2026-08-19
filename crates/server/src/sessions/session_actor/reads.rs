//! Answering questions without waking anything.
//!
//! Every read is served from the resident actor's own memory, or forwarded to
//! the agent that owns the transcript. None of them touches the journal, so
//! opening a session to look at it costs no sandbox — which is what lets a
//! browser poll a session that is otherwise idle.
//!
//! No events and no state: this only ever answers.
//!
//! # Why there is almost nothing here
//!
//! The projections themselves live in [`crate::sessions::runners::reads`],
//! where they are pure functions of `SessionState` and are tested against
//! hand-built state with no actor, no runtime and no journal. What is left here
//! is the part that genuinely needs the actor: resolving a selector to an agent
//! and asking that agent for its own log.
//!
//! The seven hundred lines this replaces were that same projection written
//! against four kinds of agent, with a `match` per fact.

use super::{
    AgentDetail, CommandEffect, ReadCommand, SessionActor, SessionEvent, SessionSnapshot,
    SessionState,
};
use crate::agent_loop::AgentCommand;
use crate::sessions::addressing::SessionInbox;
use crate::sessions::runners::reads;
use horsie_actor::ActorContext;

/// Reads.
pub(super) struct Reads;

impl Reads {
    pub(super) async fn handle(
        actor: &mut SessionActor,
        state: &SessionState,
        cmd: ReadCommand,
        ctx: &ActorContext<SessionInbox>,
    ) -> CommandEffect<SessionEvent> {
        match cmd {
            ReadCommand::ReadLog {
                agent_id,
                after,
                reply,
            } => {
                let out = match Self::mailbox(actor, state, ctx, agent_id.as_deref()) {
                    Some(agent) => agent
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
                let page = match Self::mailbox(actor, state, ctx, agent_id.as_deref()) {
                    Some(agent) => agent
                        .ask(|reply| AgentCommand::PageLog { before, max, reply })
                        .await
                        .ok(),
                    None => None,
                };
                let _ = reply.send(page);
                CommandEffect::none()
            }
            ReadCommand::Agent { agent_id, reply } => {
                let detail = Self::detail(actor, state, ctx, agent_id.as_deref()).await;
                let _ = reply.send(detail);
                CommandEffect::none()
            }
            ReadCommand::Snapshot { reply } => {
                let usage = reads::usage_stats(state);
                let _ = reply.send(SessionSnapshot {
                    status: reads::session_status(state),
                    // Banked totals only, so this asks no agent anything. The
                    // live context size is per-agent and never summed, and it
                    // is on the agent document rather than here.
                    usage_total: usage.session_total,
                    agents: reads::agent_roster(state),
                });
                CommandEffect::none()
            }
            ReadCommand::UsageStats { reply } => {
                let _ = reply.send(reads::usage_stats(state));
                CommandEffect::none()
            }
            ReadCommand::RunState { reply } => {
                let _ = reply.send(reads::run_state(state));
                CommandEffect::none()
            }
        }
    }

    /// The mailbox a selector names, spawning a cold agent if need be.
    ///
    /// Two steps, and the split is the point: [`reads::resolve`] is a pure
    /// lookup — `"main"` is a *name* for the root runner's agent, not an id —
    /// and `reach` is the part that needs the actor.
    fn mailbox(
        actor: &mut SessionActor,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        agent_id: Option<&str>,
    ) -> Option<horsie_actor::ActorRef<AgentCommand>> {
        let agent = reads::resolve(state, agent_id)?;
        actor.reach(agent, state, ctx)
    }

    /// One agent's document: what it is, what became of it, and its live values.
    ///
    /// The first three come from the session's fold; the last is the agent's
    /// own, because a task list and a context size are things only it knows.
    async fn detail(
        actor: &mut SessionActor,
        state: &SessionState,
        ctx: &ActorContext<SessionInbox>,
        agent_id: Option<&str>,
    ) -> Option<AgentDetail> {
        let agent = reads::resolve(state, agent_id)?;
        let entry = reads::agent_entry(state, agent)?;
        let settings = reads::settings_of(state, agent)?.clone();
        let (task, output) = reads::task_and_output(state, agent);
        let mailbox = actor.reach(agent, state, ctx)?;
        let view = mailbox
            .ask(|reply| AgentCommand::GetState { reply })
            .await
            .ok()?;
        Some(AgentDetail {
            entry,
            settings,
            task,
            output,
            state: view,
        })
    }
}
