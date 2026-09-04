//! One normal provider step and the meaning of its result.
//!
//! This module never decides when to run. [`RunLoop::advance`] opens the
//! durable marker and calls one provider request. The callback is accepted only
//! by the matching live provider variant, then its message and usage are
//! journaled together.
//!
//! Tool calls are dispatched by the run loop after that write. Once every call
//! has a durable result, this module interprets terminal tools, asks, parks,
//! cancellation, and result-required nudges. Iteration and repeated-tool
//! detection are derived from current-turn history rather than retained in a
//! second transient structure.

mod conclusion;

use crate::agent_loop::context::AskedQuestion;
use crate::agent_loop::prelude::*;
use crate::sessions::ask_tool::ASK_USER_TOOL;
use crate::sessions::workflow::SUBMIT_RESULT_TOOL;
use async_trait::async_trait;
use horsie_actor::{ActorRef, CommandEffect};
use horsie_agentcore::{
    AgentEvent, EventSink, EventSinkError, LlmError, Message, StepError, StepRequest, StoppedCall,
    extract_text, extract_tool_calls, tool_fingerprint,
};
use horsie_models::now_ms;
use serde_json::Value;
use std::time::Duration;

/// A high safety ceiling for long autonomous and workflow runs.
const MAX_ITERATIONS: u32 = 10_000;
const STUCK_THRESHOLD: usize = 5;
const NUDGE_THRESHOLD: usize = 3;

/// Interprets provider-step results. All durable and process-local progress is
/// owned by history and [`StepRun`], respectively.
#[derive(Default)]
pub(crate) struct ProviderStep;

/// Result of a turn, interpreted by [`ProviderStep::conclude`].
pub struct RunReport {
    pub(crate) outcome: RunOutcome,
}

#[derive(Debug)]
pub(crate) enum RunOutcome {
    /// Agent ended its turn with plain text. Whether that is a park or a
    /// mistake is decided here, where what would wake the agent is known.
    Completed {
        text: String,
    },
    /// A tool ended the run. One call per stopper the model issued.
    Stopped {
        calls: Vec<StoppedCall>,
    },
    Cancelled,
    Failed {
        error: String,
        recoverable: bool,
    },
}

/// Forwards streamed text chunks to the mailbox, tagged with the turn so a
/// cancelled turn's stragglers are dropped. Everything else a provider emits
/// is ignored: coarse events are journaled from the call's response, not from
/// the stream.
struct DeltaSink {
    actor: ActorRef<AgentCommand>,
    marker_seq: u64,
}

#[async_trait]
impl EventSink for DeltaSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let AgentEvent::TextChunk(chunk) = &event {
            let _ = self
                .actor
                .tell(AgentCommand::Provider(ProviderCommand::StreamDelta {
                    marker_seq: self.marker_seq,
                    text: chunk.text.clone(),
                }))
                .await;
        }
        Ok(())
    }
}

fn open_agent_marker(state: &AgentState, marker_seq: u64) -> bool {
    state
        .open_step()
        .is_some_and(|(seq, kind)| seq == marker_seq && *kind == StepKind::Provider)
}

fn current_turn_start(state: &AgentState) -> usize {
    state
        .history()
        .iter()
        .rposition(|entry| matches!(&entry.record, AgentDomainEvent::TurnBegan { .. }))
        .map_or(0, |position| position + 1)
}

fn completed_provider_steps(state: &AgentState) -> u32 {
    state.history()[current_turn_start(state)..]
        .iter()
        .filter(|entry| matches!(&entry.record, AgentDomainEvent::MessageComplete { .. }))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn recent_tool_fingerprints(state: &AgentState) -> Vec<String> {
    let mut fingerprints: Vec<_> = state.history()[current_turn_start(state)..]
        .iter()
        .filter_map(|entry| {
            let AgentDomainEvent::MessageComplete { message, .. } = &entry.record else {
                return None;
            };
            let calls = extract_tool_calls(&message.parts);
            (!calls.is_empty()).then(|| tool_fingerprint(&calls))
        })
        .rev()
        .take(STUCK_THRESHOLD)
        .collect();
    fingerprints.reverse();
    fingerprints
}

fn provider_tool_choice(state: &AgentState, requires_result: bool) -> horsie_agentcore::ToolChoice {
    if requires_result && state.nudges() >= MAX_RESULT_NUDGES {
        horsie_agentcore::ToolChoice::Required(SUBMIT_RESULT_TOOL.to_string())
    } else {
        horsie_agentcore::ToolChoice::Auto
    }
}

impl ProviderStep {
    /// Make this turn's next provider call — or fail the turn when its
    /// iteration budget is spent.
    ///
    /// Called by the boundary, which has already established that the agent
    /// owes a call, that nothing else is running, and that the contexts are
    /// fresh. Iteration and stuck-loop progress are reconstructed from durable
    /// history rather than retained beside [`StepRun`].
    pub(crate) async fn run_step(cx: &mut CommandContext<'_>) -> CommandEffect<AgentDomainEvent> {
        let max_iterations = cx.params.max_iterations.unwrap_or(MAX_ITERATIONS);
        if completed_provider_steps(cx.state) >= max_iterations {
            let state = cx.state.clone();
            return Self::fail_turn(
                Vec::new(),
                &state,
                format!("max iterations exceeded (max={max_iterations})"),
                false,
                cx,
            )
            .await;
        }
        Self::spawn_call(cx.state, cx, 0, None);
        CommandEffect::none()
    }

    /// The message a cancelled run was part-way through writing, if it had
    /// written anything worth keeping.
    ///
    /// Reads the deltas, which are the only copy: a streamed message becomes
    /// durable when the provider finishes it, and a cancelled call never
    /// reaches that point.
    pub(crate) fn aborted_message(cx: &CommandContext<'_>) -> Option<Message> {
        let text = cx.step_run.streamed_text.concat();
        (!text.trim().is_empty()).then(|| Message::assistant_text(new_message_id(), text, now_ms()))
    }

    /// Merge `events` with whatever concluding the turn adds, in one effect.
    async fn finish(
        mut events: Vec<AgentDomainEvent>,
        report: RunReport,
        folded: &AgentState,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let tail = Self::conclude(report, folded, cx).await;
        let snapshot = tail.snapshots();
        events.extend(tail.events().iter().cloned());
        let effect = CommandEffect::persist(events);
        match snapshot {
            true => effect.and_snapshot(),
            false => effect,
        }
    }

    /// End the turn as a failure: bank what it spent, then report it.
    async fn fail_turn(
        mut events: Vec<AgentDomainEvent>,
        state: &AgentState,
        error: String,
        recoverable: bool,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        events.push(Self::turn_aborted());
        let folded = RunLoop::apply_all(state, &events);
        Self::finish(
            events,
            RunReport {
                outcome: RunOutcome::Failed { error, recoverable },
            },
            &folded,
            cx,
        )
        .await
    }

    /// Mark a normal run boundary. Completed provider steps already banked
    /// their own usage with their assistant messages.
    fn turn_completed(state: &AgentState) -> AgentDomainEvent {
        AgentDomainEvent::TurnCompleted {
            iterations: completed_provider_steps(state),
            at_ms: now_ms(),
        }
    }

    /// Mark a failed or cancelled run boundary. There is no bill here: any
    /// completed provider step was persisted and banked when it returned.
    fn turn_aborted() -> AgentDomainEvent {
        AgentDomainEvent::TurnAborted { at_ms: now_ms() }
    }

    /// Spawn one provider call over the state's prompt.
    fn spawn_call(
        folded: &AgentState,
        cx: &mut CommandContext<'_>,
        attempt: u32,
        delay: Option<Duration>,
    ) {
        let Some(execution) = cx.step_run.execution.clone() else {
            return;
        };
        let tool_choice = provider_tool_choice(folded, cx.params.requires_result);
        // The wire-shape guard: every `tool_use` must have a `tool_result`.
        // Repairs are journaled at the moments a call becomes unanswerable, so
        // this should find nothing; it stays as the last line of defence.
        let history = repair_unanswered_tool_calls(folded.prompt_messages());
        let req = StepRequest {
            provider: execution.provider.clone(),
            conversation_id: execution.conversation_id.clone(),
            system_prompt: execution.system_prompt.clone(),
            specs: execution.specs.clone(),
            tool_choice,
            max_tokens: None,
            thinking_effort: execution.thinking_effort,
            artifact_source: cx.runtime.artifacts.clone(),
        };
        let Some((marker_seq, StepKind::Provider)) = folded.open_step() else {
            tracing::warn!("refusing a provider call without an open Agent marker");
            return;
        };
        let cancel = cx.step_run.begin_provider(marker_seq, attempt);
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            if let Some(delay) = delay {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
            }
            let sink = DeltaSink {
                actor: self_ref.clone(),
                marker_seq,
            };
            let cmd = match horsie_agentcore::run_step(&req, &history, &sink, &cancel).await {
                Ok(response) => ProviderCommand::StepDone {
                    marker_seq,
                    response: Box::new(response),
                },
                Err(StepError::Cancelled) => return,
                Err(StepError::Provider(error)) => {
                    ProviderCommand::StepFailed { marker_seq, error }
                }
            };
            let _ = self_ref.tell(AgentCommand::Provider(cmd)).await;
        });
    }

    /// One provider call answered: journal it, then read what it asks for.
    async fn handle_responded(
        response: horsie_agentcore::StepResponse,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let tool_calls = extract_tool_calls(&response.message.parts);
        let mut events = vec![AgentDomainEvent::MessageComplete {
            message: response.message.clone(),
            usage: response.usage.clone(),
        }];
        let folded = RunLoop::apply_all(cx.state, &events);

        // A truncated turn is not a finished turn. Tool calls are exempt: a
        // backend may report `length` alongside a complete tool call, and the
        // turn can still execute it and continue.
        if response.stop_reason == horsie_agentcore::StopReason::MaxTokens && tool_calls.is_empty()
        {
            return Self::fail_turn(
                events,
                &folded,
                "response truncated at the max_tokens limit".to_string(),
                false,
                cx,
            )
            .await;
        }

        if tool_calls.is_empty() {
            events.push(Self::turn_completed(&folded));
            let folded = RunLoop::apply_all(cx.state, &events);
            return Self::finish(
                events,
                RunReport {
                    outcome: RunOutcome::Completed {
                        text: extract_text(&response.message.parts),
                    },
                },
                &folded,
                cx,
            )
            .await;
        }

        // Stuck and nudge detection is reconstructed from the assistant
        // messages already persisted for this turn.
        let fingerprints = recent_tool_fingerprints(&folded);
        let all_same = fingerprints
            .first()
            .is_some_and(|first| fingerprints.iter().all(|fingerprint| fingerprint == first));
        let window = fingerprints.len();
        if window >= STUCK_THRESHOLD && all_same {
            let tool = tool_calls[0].1.clone();
            return Self::fail_turn(
                events,
                &folded,
                format!("stuck in loop: tool '{tool}' called identically {STUCK_THRESHOLD} times"),
                false,
                cx,
            )
            .await;
        }
        if window >= NUDGE_THRESHOLD && all_same {
            // Answer every call with a nudge instead of executing it, and let
            // the model try again. Journaled — the fold is the only source of
            // history — where the old loop kept them in run-task locals. The
            // calls are answered, so the next advance makes the next call.
            for (tool_call_id, _, _) in &tool_calls {
                let message = Message::tool_result(
                    tool_call_id.clone(),
                    "You have called this tool with identical arguments multiple times. \
                     Please try a different approach.",
                    false,
                    Vec::new(),
                    now_ms(),
                );
                events.push(AgentDomainEvent::InputMessage { message });
            }
            return CommandEffect::persist(events);
        }

        // Tool calls are not dispatched from the callback. The message becomes
        // durable first; the next advance derives and dispatches its open calls.
        CommandEffect::persist(events)
    }

    /// One provider call failed: retry it, or fail the turn.
    async fn handle_llm_failed(
        error: LlmError,
        attempt: u32,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        // Retrying re-issues one request over the same durable history. The
        // attempt belongs only to the live provider variant; recovery closes
        // the interrupted marker instead of resuming retries.
        let max_retries = cx.params.max_retries;
        if error.is_transient() && attempt < max_retries {
            let next_attempt = attempt + 1;
            let delay = error
                .retry_after()
                .unwrap_or_else(|| Duration::from_millis(50u64 * (1u64 << next_attempt.min(6))));
            tracing::warn!(
                error = %error,
                attempt = next_attempt,
                delay_ms = delay.as_millis(),
                "transient provider error; retrying the call"
            );
            let state = cx.state.clone();
            Self::spawn_call(&state, cx, next_attempt, Some(delay));
            return CommandEffect::none();
        }
        let state = cx.state.clone();
        Self::fail_turn(
            Vec::new(),
            &state,
            error.to_string(),
            // Report the classification rather than assuming recoverable: a
            // permanent failure shown as transient invites the user to retry
            // something that can never succeed.
            error.is_transient(),
            cx,
        )
        .await
    }

    /// The batch settled on a stopper: the actor ran the calls, one of them
    /// ended the run, and what that *means* is decided here.
    pub(crate) async fn ended_by_tools(
        mut events: Vec<AgentDomainEvent>,
        stopped: Vec<StoppedCall>,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        events.push(Self::turn_completed(cx.state));
        let folded = RunLoop::apply_all(cx.state, &events);
        Self::finish(
            events,
            RunReport {
                outcome: RunOutcome::Stopped { calls: stopped },
            },
            &folded,
            cx,
        )
        .await
    }

    /// The turn was cancelled: bank what it spent, repair what it left
    /// dangling, and report it. Called by the boundary, which has already
    /// stopped everything that was running.
    pub(crate) async fn cancelled(cx: &mut CommandContext<'_>) -> CommandEffect<AgentDomainEvent> {
        let events = vec![Self::turn_aborted()];
        let folded = RunLoop::apply_all(cx.state, &events);
        Self::finish(
            events,
            RunReport {
                outcome: RunOutcome::Cancelled,
            },
            &folded,
            cx,
        )
        .await
    }
}

impl ProviderStep {
    /// Every callback is accepted only while its marker is the open top step.
    pub(crate) async fn handle(
        cmd: ProviderCommand,
        cx: &mut CommandContext<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            ProviderCommand::StepDone {
                marker_seq,
                response,
            } => {
                if !open_agent_marker(cx.state, marker_seq)
                    || cx.step_run.finish_provider(marker_seq).is_none()
                {
                    tracing::warn!(marker_seq, "dropping a callback for a closed agent step");
                    return CommandEffect::none();
                }
                Self::handle_responded(*response, cx).await
            }
            ProviderCommand::StepFailed { marker_seq, error } => {
                if !open_agent_marker(cx.state, marker_seq) {
                    return CommandEffect::none();
                }
                let Some(attempt) = cx.step_run.finish_provider(marker_seq) else {
                    return CommandEffect::none();
                };
                Self::handle_llm_failed(error, attempt, cx).await
            }
            ProviderCommand::StreamDelta { marker_seq, text } => {
                if open_agent_marker(cx.state, marker_seq)
                    && cx.step_run.push_delta(marker_seq, text)
                {
                    cx.publish_revision();
                }
                CommandEffect::none()
            }
            ProviderCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
        }
    }

    /// Fold the usage banked by a completed provider step.
    pub(crate) fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        if let AgentDomainEvent::MessageComplete { usage, .. } = event {
            state.bank_step_usage(usage);
        }
    }
}

/// How many turns an agent that owes a result may end without one before the
/// step is failed. Two: the first nudge is a plain message, the second forces
/// `submit_result` in `tool_choice`, and a model that defeats both is not going
/// to be talked round by a third.
pub(crate) const MAX_RESULT_NUDGES: u32 = 2;

#[cfg(test)]
mod tests {
    use super::*;
    use horsie_agentcore::{ContentPart, Role};
    use horsie_models::agent::ToolCallPart;

    fn fold(events: impl IntoIterator<Item = AgentDomainEvent>) -> AgentState {
        events
            .into_iter()
            .fold(AgentActor::initial_state(), RunLoop::apply)
    }

    fn assistant_call(id: &str, call_id: &str) -> AgentDomainEvent {
        AgentDomainEvent::MessageComplete {
            message: Message {
                created_at_ms: 1,
                started_at_ms: None,
                id: id.into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: call_id.into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "true"}),
                })],
            },
            usage: horsie_agentcore::Usage::without_cache(1, 1),
        }
    }

    #[test]
    fn default_iteration_budget_supports_long_workflows() {
        assert_eq!(MAX_ITERATIONS, 10_000);
    }

    #[test]
    fn provider_progress_is_derived_from_the_current_turn_history() {
        let state = fold([
            AgentDomainEvent::TurnBegan {
                consumed: Vec::new(),
                answered: Vec::new(),
                at_ms: 0,
            },
            assistant_call("a1", "tc1"),
            assistant_call("a2", "tc2"),
            assistant_call("a3", "tc3"),
        ]);
        assert_eq!(completed_provider_steps(&state), 3);
        assert_eq!(recent_tool_fingerprints(&state).len(), 3);
    }

    #[test]
    fn forced_result_choice_survives_recovery_through_history() {
        let state = fold([
            AgentDomainEvent::Nudged { at_ms: 1 },
            AgentDomainEvent::Nudged { at_ms: 2 },
        ]);
        assert!(matches!(
            provider_tool_choice(&state, true),
            horsie_agentcore::ToolChoice::Required(tool) if tool == SUBMIT_RESULT_TOOL
        ));
        assert!(matches!(
            provider_tool_choice(&state, false),
            horsie_agentcore::ToolChoice::Auto
        ));
    }
}
