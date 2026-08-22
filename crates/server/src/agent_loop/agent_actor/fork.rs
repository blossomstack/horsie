//! Being a fork, and being forked from.
//!
//! Forking changes nothing about the conversation being forked: the branch
//! point is a read, taken before anything is written so the number names the
//! moment the fork was *asked for* rather than the moment its seed happened to
//! be built.
//!
//! Adopting a history is the other half, and it is one write. The message rides
//! along with the state rather than being enqueued separately for two reasons,
//! both learned the hard way: enqueued first, the fork drains and answers it
//! before it has a history; enqueued after, a crash in between leaves a seeded
//! fork with nothing to do.

use super::*;
use horsie_actor::{ActorContext, CommandEffect, ReplyTo};
use horsie_agentcore::AgentLogBody;
use horsie_models::now_ms;

impl AgentState {
    /// Append `body` at the next sequence number.
    ///
    /// The single place a `seq` is handed out, so the fold cannot produce a gap
    /// or a duplicate by accident.
    /// This conversation as a fork's starting point.
    ///
    /// Everything that is *about the conversation* carries; everything that is
    /// in flight, or is a bill, does not. A fork that inherited an ask would
    /// park on a question nobody put to it; one that inherited `turn_in_flight`
    /// would be reported interrupted before it had ever run; one that inherited
    /// `usage_total` would make the session's aggregate count the same tokens
    /// twice, once under each conversation.
    ///
    /// Cut at `at_seq` — the branch point, read when the fork was asked for.
    /// Not at the log's current end: journaling the fork writes a `Forked`
    /// entry onto this very log, and a source that is mid-turn goes on
    /// appending while the seed is being built. Copying to the end handed the
    /// fork its own creation marker and whatever else had landed since.
    ///
    /// `next_seq` becomes `at_seq` for the same reason: the fork's own entries
    /// number on from where the copied ones stop, so every cursor into the
    /// copied log still resolves and nothing collides.
    #[must_use]
    pub fn scrub_for_fork(&self, at_seq: u64) -> Self {
        Self {
            log: self
                .log
                .iter()
                .filter(|e| e.seq < at_seq)
                .cloned()
                .collect(),
            next_seq: at_seq,
            context_tokens: self.context_tokens,
            task_list: self.task_list.clone(),
            inbox: Vec::new(),
            asks: Vec::new(),
            nudges: 0,
            timers: Vec::new(),
            parked: false,
            turn_in_flight: false,
            usage_total: UsageTotal::default(),
            last_turn_usage: None,
        }
    }
}

/// Being a fork, and being forked from.
pub(super) struct Forks;

impl Forks {
    pub(super) async fn handle(
        _actor: &mut AgentActor,
        state: &AgentState,
        cmd: ForkCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            ForkCommand::ForkSeed { at_seq, reply } => {
                let _ = reply.send(Box::new(state.scrub_for_fork(at_seq)));
                CommandEffect::none()
            }
            ForkCommand::SeedFrom {
                state: seeded,
                seed,
                message,
                reply,
            } => {
                // Already seeded. Not an error: a process that died between
                // this write and the session journaling `ForkSeeded` comes back
                // and re-seeds, and the honest answer is that the work is done.
                // Saying otherwise would fail a fork that is perfectly fine.
                if !state.log.is_empty() {
                    let _ = reply.send(Ok(()));
                    let _ = ctx
                        .self_ref()
                        .tell(AgentCommand::Queue(QueueCommand::Drain))
                        .await;
                    return CommandEffect::none();
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let answer = match rx.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(format!("persist the fork's history: {e}")),
                        Err(_) => Err("the fork's history was never written".to_string()),
                    };
                    let _ = reply.send(answer);
                });
                // Decided after the write, exactly as `Enqueue` does: the queue
                // a turn drains has to be the durable one.
                let _ = ctx
                    .self_ref()
                    .tell(AgentCommand::Queue(QueueCommand::Drain))
                    .await;
                CommandEffect::persist(vec![
                    AgentDomainEvent::Seeded {
                        state: seeded,
                        seed,
                    },
                    AgentDomainEvent::Received {
                        item: message,
                        at_ms: now_ms(),
                    },
                ])
                .and_ack(ReplyTo::from_sender(tx))
                // A whole conversation in one event is exactly the case a
                // snapshot exists for: without one, every later recovery
                // replays it.
                .and_snapshot()
            }
        }
    }
}

impl Component for Forks {
    /// The history this agent adopted, and the seed appended after it.
    // The fallthrough is unreachable by construction: `AgentActor::apply_event`
    // routes every variant to exactly one module, so an event added later fails
    // to compile *there* — where it should be classified — rather than silently
    // reaching the wrong fold here.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::Seeded {
                state: seeded,
                seed,
            } => {
                // Wholesale, because this is the agent's first event: anything
                // already here would be a bug rather than a history to merge.
                *state = *seeded;
                let at_ms = seed.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(*seed));
            }
            _ => {}
        }
    }
}
