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

use crate::agent_loop::component::DispatchedCall;
use crate::agent_loop::prelude::*;
use horsie_actor::{ActorRef, CommandEffect, ReplyTo};
use horsie_agentcore::{StoppedCall, ToolOutcome, Toolbox};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Why the next provider call cannot happen yet.
///
/// A veto, asked of every component that has one. They commute — the order
/// they are asked in cannot matter — which is what makes this a poll rather
/// than another ordered decision.
#[derive(Debug)]
pub(crate) enum Blocked {
    /// Tool calls the model made are still being executed. Naming them makes
    /// a stuck turn diagnosable from a log line.
    ToolCalls(Vec<String>),
    /// The agent is parked on questions and nothing queued may abandon them.
    Parked,
}

impl Components {
    /// Decide what happens next, and start it.
    pub(crate) async fn advance(&mut self, cx: &mut Cx<'_>) -> CommandEffect<AgentDomainEvent> {
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
        // 2. Anyone's veto. The turn's — calls the model made that have no
        //    answer — is also this actor's work order: whichever of them has
        //    no task running yet is dispatched right here, because tool calls
        //    are the actor's to run, not any component's.
        if let Some(blocked) = self.blocked(cx) {
            match blocked {
                Blocked::ToolCalls(open) => self.dispatch_tools(open, cx),
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
        // The actor gathers every toolbox its components vend and hands the
        // lot to provisioning — the components never learn who composed them.
        let vended = self
            .toolboxes(cx.actor.self_ref(), cx.scratch.work);
        self.provision.start(vended, cx);
        true
    }

    /// Run whichever of the model's open calls has no task yet.
    ///
    /// Idempotent like everything the boundary does: dispatched calls are in
    /// `scratch.calls`, stoppers wait in `scratch.stopped`, and asking again
    /// dispatches only what is genuinely new. Every call goes to the composed
    /// toolbox — the actor cannot tell a component's tool from a remote one.
    fn dispatch_tools(&mut self, open: Vec<String>, cx: &mut Cx<'_>) {
        let Some(tctx) = cx.scratch.ctx.clone() else {
            // A crash can land here: an interrupted turn's calls are repaired
            // on load, but a raced report may leave one open before contexts
            // exist. Provisioning first is always safe.
            let _ = self.needs_contexts(cx);
            return;
        };
        for id in open {
            let in_flight = cx.scratch.calls.iter().any(|c| c.id == id)
                || cx.scratch.stopped.iter().any(|c| c.tool_call_id == id);
            if in_flight {
                continue;
            }
            let Some((name, input)) = cx.state.tool_call_named(&id) else {
                tracing::warn!(id, "an open tool call is not in the transcript");
                continue;
            };
            cx.scratch.calls.push(DispatchedCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            });
            spawn_tool_call(
                &tctx.toolbox,
                name,
                input,
                id,
                cx.scratch.work,
                cx.scratch.cancel.clone(),
                cx.actor.self_ref(),
            );
        }
    }

    /// One dispatched call answered. Journal the result; when the batch
    /// settles on a stopper, hand the turn its ending.
    pub(super) async fn tool_returned(
        &mut self,
        work: u64,
        tool_call_id: String,
        outcome: ToolReturn,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        if !cx.scratch.live(work) {
            tracing::warn!(work, tool_call_id, "dropping a superseded tool result");
            return CommandEffect::none();
        }
        let position = cx.scratch.calls.iter().position(|c| c.id == tool_call_id);
        let call = match position {
            Some(position) => cx.scratch.calls.remove(position),
            None => {
                tracing::warn!(tool_call_id, "a tool result answered no dispatched call");
                return CommandEffect::none();
            }
        };
        let mut events = Vec::new();
        match outcome {
            ToolReturn::Result {
                output,
                is_error,
                artifacts,
            } => events.push(AgentDomainEvent::ToolComplete {
                tool_call_id: call.id,
                output,
                is_error,
                artifacts,
                at_ms: horsie_models::now_ms(),
            }),
            // No result yet for a stopper: what it *means* is the turn's to
            // decide, once nothing else is in flight.
            ToolReturn::Stopped => cx.scratch.stopped.push(StoppedCall {
                tool: call.name,
                tool_call_id: call.id,
                input: call.input,
            }),
        }
        if cx.scratch.calls.is_empty() && !cx.scratch.stopped.is_empty() {
            let stopped = std::mem::take(&mut cx.scratch.stopped);
            return self.turn.ended_by_tools(events, stopped, cx).await;
        }
        // Ordinary results only: the persist lands, the advance that follows
        // sees the batch shrink, and the last one clears the veto.
        CommandEffect::persist(events)
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
    pub(crate) async fn cancel(
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

/// Dispatch one tool call on its own task — the actor's, for every kind of
/// tool. Whether the toolbox guards
/// its wire with timeouts is its own business; cancel is the rescue either
/// way, and the fence drops whatever a dead turn's task still says.
pub(super) fn spawn_tool_call(
    toolbox: &Arc<dyn Toolbox>,
    name: String,
    input: Value,
    tool_call_id: String,
    work: u64,
    cancel: CancellationToken,
    self_ref: ActorRef<AgentCommand>,
) {
    let toolbox = toolbox.clone();
    tokio::spawn(async move {
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            result = toolbox.execute(&name, input.clone(), &tool_call_id) => result,
        };
        let outcome = match result {
            // A string result is forwarded verbatim; re-encoding it as JSON
            // would wrap it in quotes and escape every newline.
            Ok(ToolOutcome::Result(v)) => ToolReturn::Result {
                output: v
                    .value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.value.to_string()),
                is_error: false,
                artifacts: v.artifacts,
            },
            Ok(ToolOutcome::StopRun) => ToolReturn::Stopped,
            // An error produced no artifacts by definition.
            Err(e) => ToolReturn::Result {
                output: e.to_string(),
                is_error: true,
                artifacts: Vec::new(),
            },
        };
        let _ = self_ref
            .tell(AgentCommand::Core(CoreCommand::ToolReturned {
                work,
                tool_call_id,
                outcome,
            }))
            .await;
    });
}

