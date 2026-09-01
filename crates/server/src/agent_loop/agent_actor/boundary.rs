//! What this agent should be doing, decided in one place.
//!
//! Every component reports only about itself; nothing tells anything else what
//! to do. This is where those reports become decisions — the one piece of code
//! that knows what components exist, and therefore the only place the order
//! between them is written down.
//!
//! The decision is re-taken at every moment it could have changed, which is
//! after every durable write ([`AgentActor::on_events_persisted`]) and at the
//! few points where something moved without one. It is *idempotent*: it reads
//! the state and the scratch as they stand and does whatever they now call
//! for, so asking twice for one change costs a check and nothing else.
//!
//! The order below is the whole design:
//!
//! 1. something is already running, or there is no runtime — nothing to do
//! 2. a component vetoes the next step — wait for whatever it named
//! 3. the queue is offering something — take it, in the queue's own order
//! 4. the turn owes the provider a call — contexts, compaction, then the call
//!
//! Rule 3 sitting *above* rule 4 is what lets a message that arrives mid-turn
//! reach the model on the next call rather than after the turn: every tool
//! call is answered by the time this runs, so the queue may join in.

use super::*;
use horsie_actor::{CommandEffect, ReplyTo};

/// Why the next provider call cannot happen yet.
///
/// A veto, asked of every component that has one. They commute — the order
/// they are asked in cannot matter — which is what makes this a poll rather
/// than another ordered decision.
#[derive(Debug)]
pub(super) enum Blocked {
    /// Tool calls the model made are still being executed. Naming them makes
    /// a stuck turn diagnosable from a log line.
    ToolCalls(Vec<String>),
    /// The agent is parked on questions and nothing queued may abandon them.
    Parked,
}

impl Components {
    /// Decide what happens next, and start it.
    pub(super) async fn advance(&mut self, cx: &mut Cx<'_>) -> CommandEffect<AgentDomainEvent> {
        // 1. One thing at a time. Whatever is running reports back, and the
        //    handler that takes the report advances again.
        if cx.scratch.running.is_some() {
            return CommandEffect::none();
        }
        // No runtime, no work. The `Runtime` lifecycle record the owner sends
        // is what moves this, and it advances again when it does.
        if !cx.scratch.ready {
            return CommandEffect::none();
        }
        // 2. Anyone's veto.
        if let Some(blocked) = self.blocked(cx) {
            match blocked {
                Blocked::ToolCalls(open) => {
                    tracing::trace!(calls = ?open, "holding: tool calls are still running");
                }
                Blocked::Parked => tracing::trace!("holding: parked on a question"),
            }
            return CommandEffect::none();
        }
        // 3. Whatever the queue is offering, in its order of precedence.
        match crate::agent_loop::queued_offer(cx.state.inbox(), cx.state.asks()) {
            Some(crate::agent_loop::Offer::Summary {
                consumed,
                sub_sessions,
            }) => {
                if self.needs_contexts(cx) {
                    return CommandEffect::none();
                }
                self.seed.take_summary(consumed, sub_sessions, cx);
                return CommandEffect::none();
            }
            Some(crate::agent_loop::Offer::Compact {
                consumed,
                instructions,
            }) => {
                if self.needs_contexts(cx) {
                    return CommandEffect::none();
                }
                self.compaction.start(
                    CompactJob {
                        consumed,
                        manual: true,
                        instructions,
                        tokens_before: cx.state.context_tokens(),
                    },
                    cx,
                );
                return CommandEffect::none();
            }
            // Journaled, not run: taking the input is a write, and the write
            // is what makes this agent owe the provider a call. The advance
            // that follows the persist is what runs it.
            Some(crate::agent_loop::Offer::Input(turn)) => {
                return self.queue.take(*turn, cx).await;
            }
            None => {}
        }
        // 4. Nothing queued. Either a turn is part-way through — the model
        //    called tools, they answered, and it is owed another call — or the
        //    agent is idle and this costs one branch.
        if !cx.state.turn_in_flight() {
            return CommandEffect::none();
        }
        if self.needs_contexts(cx) {
            return CommandEffect::none();
        }
        // Between calls is the only safe place to fold history away — every
        // tool call is answered, so nothing can be cut across — and it is also
        // the last chance before the call that would overflow the window. The
        // turn is never told: its next call simply reads a shorter history.
        if self.compaction.due(cx) {
            self.compaction.start(
                CompactJob {
                    consumed: Vec::new(),
                    manual: false,
                    instructions: None,
                    tokens_before: cx.state.context_tokens(),
                },
                cx,
            );
            return CommandEffect::none();
        }
        self.turn.run_step(cx).await
    }

    /// Whether the contexts must be built before anything else can run —
    /// starting the setup if so.
    ///
    /// Every kind of work needs the same one and none of them knows it
    /// happened: provisioning claims the slot, and the advance that follows it
    /// arrives back here with the contexts published.
    fn needs_contexts(&mut self, cx: &mut Cx<'_>) -> bool {
        if !cx.scratch.ctx_stale && cx.scratch.ctx.is_some() {
            return false;
        }
        self.provision.start(cx);
        true
    }

    /// Every part's veto, asked of each in turn.
    ///
    /// A poll, not a rule: this code does not know what any of them will say,
    /// or how many there are. A component added later gets a say by
    /// implementing one method on its own state.
    fn blocked(&self, cx: &Cx<'_>) -> Option<Blocked> {
        cx.state.vetoes().next()
    }

    /// Stop whatever is in flight and answer the canceller.
    ///
    /// The ack fires before anything else: a canceller is likely blocking its
    /// own mailbox on it, and the deliveries a concluded turn makes `tell`
    /// into that same mailbox. The generation fence is what makes answering
    /// early honest — a bumped generation cannot be written against.
    pub(super) async fn cancel(
        &mut self,
        ack: Option<ReplyTo<()>>,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        cx.scratch.stop();
        if let Some(ack) = ack {
            let _ = ack.send(());
        }
        // A turn that was owed a call is over, and its dangling calls are
        // repaired where they belong — under the message that made them.
        // Anything else in flight simply stops: a cancelled compaction leaves
        // the history it was going to fold exactly as it was.
        if !cx.state.turn_in_flight() {
            return CommandEffect::none();
        }
        self.turn.cancelled(cx).await
    }
}
