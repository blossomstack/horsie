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
//! the state and the step_run as they stand and does whatever they now call
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

const MAX_STOP_CONTINUATIONS: usize = 3;

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

fn cap_stop_records(records: &mut [horsie_models::hooks::HookRecord]) {
    use horsie_models::hooks::{HookAction, StopOutcome, SubagentStopOutcome};
    for record in records {
        match &mut record.action {
            HookAction::Stop(stop) => {
                if let StopOutcome::Blocked(blocked) = &stop.outcome {
                    stop.outcome = StopOutcome::CapReached(blocked.clone());
                }
            }
            HookAction::SubagentStop(stop) => {
                if let SubagentStopOutcome::Blocked(blocked) = &stop.outcome {
                    stop.outcome = SubagentStopOutcome::CapReached(blocked.clone());
                }
            }
            HookAction::PreToolUse(_)
            | HookAction::PostToolUse(_)
            | HookAction::PostToolUseFailure(_)
            | HookAction::PostToolBatch(_)
            | HookAction::SessionStart(_)
            | HookAction::SessionEnd(_)
            | HookAction::UserPromptSubmit(_)
            | HookAction::UserPromptExpansion(_)
            | HookAction::StopFailure(_)
            | HookAction::SubagentStart(_)
            | HookAction::TaskCreated(_)
            | HookAction::TaskCompleted(_)
            | HookAction::Notification(_)
            | HookAction::PreCompact(_)
            | HookAction::PostCompact(_)
            | HookAction::CwdChanged(_) => {}
        }
    }
}

impl AgentLoop {
    /// Decide what happens next, and start it.
    pub(crate) async fn advance(&mut self, cx: &mut Cx<'_>) -> CommandEffect<AgentDomainEvent> {
        // 1. One thing at a time. Whatever is running reports back, and the
        //    handler that takes the report advances again.
        if cx.step_run.is_running() {
            return CommandEffect::none();
        }
        // No runtime, no work. The `Runtime` lifecycle record the owner sends
        // is what moves this, and it advances again when it does.
        if !cx.step_run.ready {
            return CommandEffect::none();
        }
        let work_due = cx.state.turn_in_flight() || cx.state.queued_offer().is_some();
        if work_due && let Some(effect) = self.ensure_contexts(cx) {
            return effect;
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
        match cx.state.queued_offer() {
            Some(crate::agent_loop::Offer::Summary {
                consumed,
                sub_sessions,
            }) => {
                let request_id = consumed.join(":");
                let expected = StepKind::SeedSummary { request_id };
                let marker = cx
                    .state
                    .open_step()
                    .filter(|(_, kind)| **kind == expected)
                    .map(|(seq, _)| seq);
                let Some(marker_seq) = marker else {
                    return CommandEffect::persist(vec![AgentDomainEvent::StepStarted {
                        kind: expected,
                    }]);
                };
                self.seed
                    .take_summary(marker_seq, consumed, sub_sessions, cx);
                return CommandEffect::none();
            }
            Some(crate::agent_loop::Offer::Compact {
                consumed,
                instructions,
            }) => {
                let marker = cx
                    .state
                    .open_step()
                    .filter(|(_, kind)| **kind == StepKind::Compaction)
                    .map(|(seq, _)| seq);
                let Some(marker_seq) = marker else {
                    return CommandEffect::persist(vec![AgentDomainEvent::StepStarted {
                        kind: StepKind::Compaction,
                    }]);
                };
                self.compaction.start(
                    marker_seq,
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
        // A settled provider step with no continuation input proposes an end
        // through a distinct Stop-hook step. Incoming records that arrived
        // before this point were offered above and win automatically.
        if !cx.state.turn_in_flight() {
            match cx.state.open_step() {
                Some((_, StepKind::Agent)) if cx.state.open_step_has_response() => {
                    return CommandEffect::persist(vec![AgentDomainEvent::StepStarted {
                        kind: StepKind::StopHook,
                    }]);
                }
                Some((marker_seq, StepKind::StopHook)) if !cx.state.open_step_has_response() => {
                    self.start_stop_hook(marker_seq, cx);
                    return CommandEffect::none();
                }
                _ => return CommandEffect::none(),
            }
        }
        // Between calls is the only safe place to fold history away — every
        // tool call is answered, so nothing can be cut across — and it is also
        // the last chance before the call that would overflow the window. The
        // turn is never told: its next call simply reads a shorter history.
        if self.compaction.due(cx) {
            let marker = cx
                .state
                .open_step()
                .filter(|(_, kind)| **kind == StepKind::Compaction)
                .map(|(seq, _)| seq);
            match marker {
                None => {
                    return CommandEffect::persist(vec![AgentDomainEvent::StepStarted {
                        kind: StepKind::Compaction,
                    }]);
                }
                Some(_) if cx.state.open_step_has_response() => {
                    // Recovery recorded an interrupted compaction. Keep the
                    // unchanged history and continue; never replay the call.
                }
                Some(marker_seq) => {
                    self.compaction.start(
                        marker_seq,
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
            }
        }
        let marker_is_ready = cx
            .state
            .open_step()
            .is_some_and(|(_, kind)| *kind == StepKind::Agent)
            && !cx.state.open_step_has_response();
        if !marker_is_ready {
            return CommandEffect::persist(vec![AgentDomainEvent::StepStarted {
                kind: StepKind::Agent,
            }]);
        }
        self.turn.run_step(cx).await
    }

    /// Open or run initialization/connection before foreground work. The
    /// marker's history sequence is the callback fence.
    fn ensure_contexts(&mut self, cx: &mut Cx<'_>) -> Option<CommandEffect<AgentDomainEvent>> {
        if !cx.step_run.ctx_stale && cx.step_run.ctx.is_some() {
            return None;
        }
        let initializing = !cx.state.initialized();
        let kind = if initializing {
            StepKind::Initialize
        } else {
            StepKind::Connect
        };
        let marker = cx
            .state
            .open_step()
            .filter(|(_, open)| **open == kind)
            .map(|(seq, _)| seq);
        let Some(marker_seq) = marker else {
            return Some(CommandEffect::persist(vec![
                AgentDomainEvent::StepStarted { kind },
            ]));
        };
        let vended = self.toolboxes(cx.actor.self_ref());
        self.provision.start(marker_seq, initializing, vended, cx);
        Some(CommandEffect::none())
    }

    fn start_stop_hook(&mut self, marker_seq: u64, cx: &mut Cx<'_>) {
        let provider = cx.runtime.context_provider.clone();
        let request = crate::agent_loop::StopHookRequest {
            last_assistant_message: cx.state.last_assistant_text(),
            active: cx.state.stop_continuations() > 0,
        };
        let cancel = cx.step_run.begin(StepPhase::StopHook, marker_seq);
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                result = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    provider.stop_hook(request),
                ) => match result {
                    Ok(result) => result,
                    Err(_) => crate::agent_loop::StopHookResult {
                        records: Vec::new(),
                        outcome: StopHookOutcome::TimedOut,
                    },
                },
            };
            let _ = self_ref
                .tell(AgentCommand::Run(RunCommand::StopHookDone {
                    marker_seq,
                    result,
                }))
                .await;
        });
    }

    pub(crate) fn stop_hook_returned(
        &mut self,
        marker_seq: u64,
        result: crate::agent_loop::StopHookResult,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let marker_is_open = cx
            .state
            .open_step()
            .is_some_and(|(seq, kind)| seq == marker_seq && *kind == StepKind::StopHook);
        if !marker_is_open {
            return CommandEffect::none();
        }
        if !cx.step_run.finished(marker_seq) {
            return CommandEffect::none();
        }
        let at_ms = horsie_models::now_ms();
        let crate::agent_loop::StopHookResult {
            mut records,
            mut outcome,
        } = result;
        if matches!(outcome, StopHookOutcome::Continue { .. })
            && cx.state.stop_continuations() >= MAX_STOP_CONTINUATIONS
        {
            cap_stop_records(&mut records);
            outcome = StopHookOutcome::Allow;
        }
        let mut events: Vec<_> = (cx.state.hook_entry_count()..)
            .zip(records)
            .map(|(seq, record)| AgentDomainEvent::HookRan { record, seq, at_ms })
            .collect();
        let pending = cx.state.queued_offer().is_some();
        let outcome = match outcome {
            StopHookOutcome::Continue { message }
                if cx.state.stop_continuations() < MAX_STOP_CONTINUATIONS =>
            {
                events.push(AgentDomainEvent::StopHookCompleted {
                    outcome: StopHookOutcome::Continue {
                        message: message.clone(),
                    },
                });
                events.push(AgentDomainEvent::Received {
                    item: crate::agent_loop::Incoming::Continue {
                        id: format!("stop-hook:{marker_seq}"),
                        reason: message,
                    },
                    at_ms,
                });
                return CommandEffect::persist(events);
            }
            StopHookOutcome::Continue { .. } | StopHookOutcome::Allow if pending => {
                StopHookOutcome::Allow
            }
            StopHookOutcome::Continue { .. } | StopHookOutcome::Allow => StopHookOutcome::Allow,
            other @ (StopHookOutcome::Failed { .. }
            | StopHookOutcome::Interrupted
            | StopHookOutcome::TimedOut) => other,
        };
        events.push(AgentDomainEvent::StopHookCompleted {
            outcome: outcome.clone(),
        });
        let reason = match outcome {
            StopHookOutcome::Allow => RunEnd::Complete {
                output: cx.state.stop_candidate(),
            },
            StopHookOutcome::Failed { reason } => RunEnd::Failed {
                error: reason,
                recoverable: false,
                terminal: false,
            },
            StopHookOutcome::Interrupted => RunEnd::Interrupted,
            StopHookOutcome::TimedOut => RunEnd::Failed {
                error: "stop hook timed out".to_string(),
                recoverable: true,
                terminal: false,
            },
            StopHookOutcome::Continue { .. } => return CommandEffect::persist(events),
        };
        if !pending {
            events.push(AgentDomainEvent::RunEnded { reason, at_ms });
        }
        CommandEffect::persist(events)
    }

    /// Run whichever of the model's open calls has no task yet.
    ///
    /// Entering the `Tools` phase dispatches the whole unresolved batch once.
    /// Recovery never enters it: open calls left by a dead process receive
    /// interrupted results instead. Every live call uses the composed toolbox.
    fn dispatch_tools(&mut self, open: Vec<String>, cx: &mut Cx<'_>) {
        let Some(tctx) = cx.step_run.ctx.clone() else {
            return;
        };
        let Some((marker_seq, StepKind::Agent)) = cx.state.open_step() else {
            return;
        };
        let calls: Vec<DispatchedCall> = open
            .into_iter()
            .filter_map(|id| match cx.state.tool_call_named(&id) {
                Some((name, input)) => Some(DispatchedCall { id, name, input }),
                None => {
                    tracing::warn!(id, "an open tool call is not in the transcript");
                    None
                }
            })
            .collect();
        if calls.is_empty() {
            return;
        }
        let cancel = cx.step_run.begin_tools(marker_seq, calls.clone());
        for call in calls {
            spawn_tool_call(
                &tctx.toolbox,
                call.name,
                call.input,
                call.id,
                marker_seq,
                cancel.clone(),
                cx.actor.self_ref(),
            );
        }
    }

    /// One dispatched call answered. Journal the result; when the batch
    /// settles on a stopper, hand the turn its ending.
    pub(super) async fn tool_returned(
        &mut self,
        marker_seq: u64,
        tool_call_id: String,
        outcome: ToolReturn,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        if !cx.step_run.live(marker_seq)
            || !cx
                .state
                .open_step()
                .is_some_and(|(seq, kind)| seq == marker_seq && *kind == StepKind::Agent)
        {
            tracing::warn!(
                marker_seq,
                tool_call_id,
                "dropping a callback for a closed tool step"
            );
            return CommandEffect::none();
        }
        let Some(call) = cx.step_run.take_tool(marker_seq, &tool_call_id) else {
            tracing::warn!(tool_call_id, "a tool result answered no dispatched call");
            return CommandEffect::none();
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
            ToolReturn::Stopped => cx.step_run.push_stopped(StoppedCall {
                tool: call.name,
                tool_call_id: call.id,
                input: call.input,
            }),
        }
        let settled = cx.step_run.settle_tools(marker_seq);
        if let Some(stopped) = settled
            && !stopped.is_empty()
        {
            return self.turn.ended_by_tools(events, stopped, cx).await;
        }
        CommandEffect::persist(events)
    }

    fn blocked(&self, cx: &Cx<'_>) -> Option<Blocked> {
        if cx.state.turn_in_flight() {
            let open = cx.state.open_tool_calls();
            if !open.is_empty() {
                return Some(Blocked::ToolCalls(open));
            }
        }
        let asks = cx.state.pending_asks();
        (!asks.is_empty() && cx.state.queued_offer().is_none()).then_some(Blocked::Parked)
    }

    /// Stop current foreground work. Acknowledgement follows the durable
    /// cancellation boundary; unconsumed incoming history stays pending.
    pub(crate) async fn cancel(
        &mut self,
        ack: Option<ReplyTo<()>>,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        cx.step_run.stop();
        let effect = if cx.state.turn_in_flight() {
            self.turn.cancelled(cx).await
        } else if let Some((_, kind)) = cx.state.open_step() {
            let at_ms = horsie_models::now_ms();
            let mut events = Vec::new();
            match kind {
                StepKind::StopHook => events.push(AgentDomainEvent::StopHookCompleted {
                    outcome: StopHookOutcome::Interrupted,
                }),
                StepKind::Compaction | StepKind::SeedSummary { .. } => {
                    events.push(AgentDomainEvent::StepFailed {
                        reason: StepFailure::Interrupted,
                    });
                }
                StepKind::Initialize | StepKind::Connect | StepKind::Agent => {}
            }
            events.push(AgentDomainEvent::RunEnded {
                reason: RunEnd::Cancelled,
                at_ms,
            });
            CommandEffect::persist(events)
        } else {
            if let Some(ack) = ack {
                let _ = ack.send(());
            }
            return CommandEffect::none();
        };
        let Some(ack) = ack else {
            return effect;
        };
        let (durable, persisted) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            if matches!(persisted.await, Ok(Ok(()))) {
                let _ = ack.send(());
            }
        });
        effect.and_ack(ReplyTo::from_sender(durable))
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
    marker_seq: u64,
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
                marker_seq,
                tool_call_id,
                outcome,
            }))
            .await;
    });
}
