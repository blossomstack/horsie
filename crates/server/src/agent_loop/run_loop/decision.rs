//! The one ordered decision about what this agent does next.
//!
//! It is evaluated after every durable append and every live transition that
//! writes nothing. Handlers report facts; this file alone orders them.
//!
//! 1. Wait while foreground work is running or the runtime is unavailable.
//! 2. Initialize or reconnect live clients when work needs them.
//! 3. Finish unresolved tools, or remain parked on an unanswered question.
//! 4. Prepare and consume the next history-derived input.
//! 5. Run a Stop hook for a settled provider step.
//! 6. Compact when required, then start exactly one provider step.
//!
//! Because input is consumed only here, anything received during provider or
//! tool execution remains pending until that step and all its tool results are
//! durable.

use super::{CompactionStep, ContextStep, ProviderStep, SeedStep};
use crate::agent_loop::prelude::*;
use crate::agent_loop::step_run::DispatchedCall;
use horsie_actor::{ActorRef, CommandEffect, ReplyTo};
use horsie_agentcore::{StoppedCall, ToolOutcome, Toolbox};
use serde_json::Value;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const MAX_STOP_CONTINUATIONS: usize = 3;

/// Why the next provider call cannot happen yet.
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

impl RunLoop {
    pub(crate) async fn handle_incoming(
        &mut self,
        cmd: IncomingCommand,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            IncomingCommand::Receive { item, ack } => {
                let effect = CommandEffect::persist(vec![AgentDomainEvent::Received {
                    item,
                    at_ms: horsie_models::now_ms(),
                }]);
                match ack {
                    Some(ack) => effect.and_ack(ack),
                    None => effect,
                }
            }
            IncomingCommand::Answer { answers, reply } => {
                let asks = cx.state.pending_asks();
                let already_answered = cx
                    .state
                    .pending_incoming()
                    .iter()
                    .any(|item| matches!(item, crate::agent_loop::Incoming::Answers { .. }));
                if already_answered {
                    let _ = reply.send(Err(crate::agent_loop::AnswerError::NothingPending));
                    return CommandEffect::none();
                }
                if let Err(error) = crate::agent_loop::validate_answers(&asks, &answers) {
                    let _ = reply.send(Err(error));
                    return CommandEffect::none();
                }
                let (durable, persisted) =
                    tokio::sync::oneshot::channel::<Result<(), horsie_actor::JournalError>>();
                tokio::spawn(async move {
                    let result = match persisted.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(error)) => Err(crate::agent_loop::AnswerError::Unavailable(
                            error.to_string(),
                        )),
                        Err(_) => Err(crate::agent_loop::AnswerError::Unavailable(
                            "the journal did not acknowledge the write".to_string(),
                        )),
                    };
                    let _ = reply.send(result);
                });
                CommandEffect::persist(vec![AgentDomainEvent::Received {
                    item: crate::agent_loop::Incoming::Answers {
                        id: uuid::Uuid::new_v4().to_string(),
                        answers,
                    },
                    at_ms: horsie_models::now_ms(),
                }])
                .and_ack(ReplyTo::from_sender(durable))
            }
            IncomingCommand::InputPrepared(prepared) => {
                let PreparedInput {
                    marker_seq,
                    input,
                    records,
                    rewritten,
                    rejection,
                } = *prepared;
                let marker_is_open = cx.state.open_step().is_some_and(|(seq, kind)| {
                    seq == marker_seq && *kind == StepKind::PrepareInput
                });
                if !marker_is_open || !cx.step_run.finish_start_hooks(marker_seq) {
                    return CommandEffect::none();
                }
                CommandEffect::persist(Self::input_events(
                    input, records, rewritten, rejection, cx.state,
                ))
            }
        }
    }

    fn input_events(
        input: crate::agent_loop::TurnInput,
        records: Vec<horsie_models::hooks::HookRecord>,
        rewritten: Option<String>,
        rejection: Option<RejectedInput>,
        state: &AgentState,
    ) -> Vec<AgentDomainEvent> {
        let at_ms = horsie_models::now_ms();
        let mut events: Vec<_> = (state.hook_entry_count()..)
            .zip(records)
            .map(|(seq, record)| AgentDomainEvent::HookRan { record, seq, at_ms })
            .collect();
        events.push(AgentDomainEvent::TurnBegan {
            consumed: input.consumed,
            abandoned: input.abandoned,
            rewritten,
            at_ms,
        });
        events.push(AgentDomainEvent::StepStarted {
            kind: StepKind::Provider,
        });
        if let Some(rejection) = rejection {
            let (error, recoverable, terminal) = match rejection {
                RejectedInput::Blocked(reason) => (reason, false, false),
                RejectedInput::Failed(error) => (error.message, true, error.terminal),
            };
            events.extend([
                AgentDomainEvent::StepFailed {
                    reason: StepFailure::Provider(error.clone()),
                },
                AgentDomainEvent::TurnCancelled { at_ms },
                AgentDomainEvent::RunEnded {
                    reason: RunEnd::Failed {
                        error,
                        recoverable,
                        terminal,
                    },
                    at_ms,
                },
            ]);
        }
        events
    }

    fn prepare_input(
        &mut self,
        input: crate::agent_loop::TurnInput,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let start = crate::agent_loop::StartTurn {
            start_source: (!cx.step_run.start_hooks_ran()).then_some(match cx.state.has_run() {
                false => horsie_models::runtime::SessionStartSource::Startup,
                true => horsie_models::runtime::SessionStartSource::Resume,
            }),
            prompt: input.message.clone(),
        };
        let nothing_due = start.start_source.is_none() && start.prompt.is_none();
        if nothing_due || !cx.runtime.context_provider.has_start_hooks() {
            return CommandEffect::persist(Self::input_events(
                input,
                Vec::new(),
                None,
                None,
                cx.state,
            ));
        }

        let marker = cx
            .state
            .open_step()
            .filter(|(_, kind)| **kind == StepKind::PrepareInput)
            .map(|(seq, _)| seq);
        let Some(marker_seq) = marker.filter(|_| !cx.state.open_step_has_response()) else {
            return CommandEffect::persist(vec![AgentDomainEvent::StepStarted {
                kind: StepKind::PrepareInput,
            }]);
        };
        let cancel = cx.step_run.begin_start_hooks(marker_seq);
        cx.step_run.mark_start_hooks_ran();
        let provider = cx.runtime.context_provider.clone();
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let outcome = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                outcome = provider.start_hooks(start) => outcome,
            };
            let (records, rewritten, rejection) = match outcome {
                Ok(preparation) => {
                    let rejection = crate::agent_loop::start_blocked(&preparation.records)
                        .map(RejectedInput::Blocked);
                    (preparation.records, preparation.message, rejection)
                }
                Err(error) => (Vec::new(), None, Some(RejectedInput::Failed(error))),
            };
            let _ = self_ref
                .tell(AgentCommand::Incoming(IncomingCommand::InputPrepared(
                    Box::new(PreparedInput {
                        marker_seq,
                        input,
                        records,
                        rewritten,
                        rejection,
                    }),
                )))
                .await;
        });
        CommandEffect::none()
    }

    /// Decide what happens next, and start it.
    pub(crate) async fn advance(
        &mut self,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        // 1. One thing at a time. Whatever is running reports back, and the
        //    handler that takes the report advances again.
        if cx.step_run.is_running() {
            return CommandEffect::none();
        }
        // No runtime, no work. The `Runtime` lifecycle record the owner sends
        // is what moves this, and it advances again when it does.
        if !cx.step_run.runtime_ready {
            return CommandEffect::none();
        }
        // 2. Live clients are needed only when durable work is waiting.
        let work_due = cx.state.turn_in_flight() || cx.state.next_input().is_some();
        if work_due && let Some(effect) = self.ensure_contexts(cx) {
            return effect;
        }
        // 3. Resolve open tool calls, or hold an unanswered question. A call
        //    with no live task is dispatched here from durable history.
        if let Some(blocked) = self.blocked(cx) {
            match blocked {
                Blocked::ToolCalls(open) => self.dispatch_tools(open, cx),
                Blocked::Parked => tracing::trace!("holding: parked on a question"),
            }
            return CommandEffect::none();
        }
        // 4. The next history-derived input, in its defined precedence.
        match cx.state.next_input() {
            Some(crate::agent_loop::PendingInput::Summary {
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
                SeedStep::take_summary(marker_seq, consumed, sub_sessions, cx);
                return CommandEffect::none();
            }
            Some(crate::agent_loop::PendingInput::Compact {
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
                CompactionStep::start(
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
            Some(crate::agent_loop::PendingInput::Input(turn)) => {
                return self.prepare_input(*turn, cx);
            }
            None => {}
        }
        // A settled provider step with no continuation input proposes an end
        // through a distinct Stop-hook step. Incoming records that arrived
        // before this point were offered above and win automatically.
        if !cx.state.turn_in_flight() {
            match cx.state.open_step() {
                Some((_, StepKind::Provider)) if cx.state.open_step_has_response() => {
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
        if CompactionStep::due(cx) {
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
                    CompactionStep::start(
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
            .is_some_and(|(_, kind)| *kind == StepKind::Provider)
            && !cx.state.open_step_has_response();
        if !marker_is_ready {
            return CommandEffect::persist(vec![AgentDomainEvent::StepStarted {
                kind: StepKind::Provider,
            }]);
        }
        ProviderStep::run_step(cx).await
    }

    /// Open or run initialization/connection before foreground work. The
    /// marker's history sequence is the callback fence.
    fn ensure_contexts(
        &mut self,
        cx: &mut CommandContext<'_>,
    ) -> Option<CommandEffect<AgentDomainEvent>> {
        if !cx.step_run.reconnect_required && cx.step_run.execution.is_some() {
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
        let toolboxes = self.toolboxes(cx.actor.self_ref());
        if initializing {
            ContextStep::initialize(marker_seq, toolboxes, cx);
        } else {
            ContextStep::reconnect(marker_seq, toolboxes, cx);
        }
        Some(CommandEffect::none())
    }

    fn start_stop_hook(&mut self, marker_seq: u64, cx: &mut CommandContext<'_>) {
        let provider = cx.runtime.context_provider.clone();
        let request = crate::agent_loop::StopHookRequest {
            last_assistant_message: cx.state.last_assistant_text(),
            active: cx.state.stop_continuations() > 0,
        };
        let cancel = cx.step_run.begin_stop_hook(marker_seq);
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
                .tell(AgentCommand::Core(CoreCommand::StopHookReturned {
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
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let marker_is_open = cx
            .state
            .open_step()
            .is_some_and(|(seq, kind)| seq == marker_seq && *kind == StepKind::StopHook);
        if !marker_is_open {
            return CommandEffect::none();
        }
        if !cx.step_run.finish_stop_hook(marker_seq) {
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
        let pending = cx.state.next_input().is_some();
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
    fn dispatch_tools(&mut self, open: Vec<String>, cx: &mut CommandContext<'_>) {
        let Some(execution) = cx.step_run.execution.clone() else {
            return;
        };
        let Some((marker_seq, StepKind::Provider)) = cx.state.open_step() else {
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
                &execution.toolbox,
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
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        if !cx.step_run.tools_are_running(marker_seq)
            || !cx
                .state
                .open_step()
                .is_some_and(|(seq, kind)| seq == marker_seq && *kind == StepKind::Provider)
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
            return ProviderStep::ended_by_tools(events, stopped, cx).await;
        }
        CommandEffect::persist(events)
    }

    fn blocked(&self, cx: &CommandContext<'_>) -> Option<Blocked> {
        if cx.state.turn_in_flight() {
            let open = cx.state.open_tool_calls();
            if !open.is_empty() {
                return Some(Blocked::ToolCalls(open));
            }
        }
        let asks = cx.state.pending_asks();
        (!asks.is_empty() && cx.state.next_input().is_none()).then_some(Blocked::Parked)
    }

    /// Stop current foreground work. Acknowledgement follows the durable
    /// cancellation boundary; unconsumed incoming history stays pending.
    pub(crate) async fn cancel(
        &mut self,
        ack: Option<ReplyTo<()>>,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        cx.step_run.stop();
        let effect = if cx.state.turn_in_flight() {
            ProviderStep::cancelled(cx).await
        } else if let Some((_, kind)) = cx.state.open_step() {
            let at_ms = horsie_models::now_ms();
            let mut events = Vec::new();
            match kind {
                StepKind::StopHook => events.push(AgentDomainEvent::StopHookCompleted {
                    outcome: StopHookOutcome::Interrupted,
                }),
                StepKind::PrepareInput | StepKind::Compaction | StepKind::SeedSummary { .. } => {
                    events.push(AgentDomainEvent::StepFailed {
                        reason: StepFailure::Interrupted,
                    });
                }
                StepKind::Initialize | StepKind::Connect | StepKind::Provider => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_marker_follows_its_exact_input() {
        let events = RunLoop::input_events(
            crate::agent_loop::TurnInput {
                consumed: vec!["input".into()],
                message: Some("hello".into()),
                ..Default::default()
            },
            Vec::new(),
            None,
            None,
            &AgentState::default(),
        );

        assert!(matches!(events[0], AgentDomainEvent::TurnBegan { .. }));
        assert!(matches!(
            events[1],
            AgentDomainEvent::StepStarted {
                kind: StepKind::Provider
            }
        ));
    }
}
