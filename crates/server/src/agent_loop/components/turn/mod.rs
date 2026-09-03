//! The turn component: what one provider call says, and what an ending means.
//!
//! This file does not decide when a call happens —
//! [`Components::advance`](super::boundary) does, and it calls
//! [`Turn::run_step`]. What comes back is this component's to *journal*: an
//! assistant message, or a turn that is over. It is not this component's to
//! run — the tool calls a message carries are dispatched by the actor, which
//! reads them off the persisted state; this file hears nothing until a
//! stopper hands it an ending. Every report is fenced by the open marker sequence,
//! so a cancelled turn's stragglers are dropped.
//!
//! Between calls this component holds nothing the agent depends on, so a
//! crash mid-batch and a live batch look identical, and a compaction that
//! lands between two calls is invisible here.
//!
//! The second half of this file is the conclusion: what an ending *means* —
//! a park, an ask, a submitted result, a contradiction — decided here because
//! only the turn knows what would wake the agent again.

use crate::agent_loop::context::AskedQuestion;
use crate::agent_loop::prelude::*;
use crate::sessions::ask_tool::ASK_USER_TOOL;
use crate::sessions::workflow::SUBMIT_RESULT_TOOL;
use async_trait::async_trait;
use horsie_actor::{ActorRef, CommandEffect};
use horsie_agentcore::{
    AgentEvent, AgentLogBody, EventSink, EventSinkError, LlmError, Message, StepError, StepRequest,
    StoppedCall, extract_text, extract_tool_calls, tool_fingerprint,
};
use horsie_models::now_ms;
use serde_json::Value;
use std::collections::VecDeque;
use std::time::Duration;

/// Defaults the old loop carried in its config; still server policy.
const MAX_ITERATIONS: u32 = 100;
const STUCK_THRESHOLD: usize = 5;
const NUDGE_THRESHOLD: usize = 3;

/// Process-local bookkeeping for normal provider steps. Durable run state,
/// open calls, and correction counts are all derived from history; a crash
/// reconstructs them rather than recovering this value.
#[derive(Default)]
pub(crate) struct Turn {
    /// The turn in flight, if any. Created lazily at the first call of a turn
    /// and dropped when it concludes — the durable `turn_in_flight` is what
    /// says a turn exists, and this is only what the turn has spent so far.
    flight: Option<TurnFlight>,
}

/// Result of a turn, interpreted by [`Turn::conclude`].
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

/// One turn's in-flight bookkeeping — what the old background loop held in
/// locals.
pub(crate) struct TurnFlight {
    tool_choice: horsie_agentcore::ToolChoice,
    /// Completed provider calls this turn.
    iteration: u32,
    /// Consecutive failed attempts at the *current* call.
    attempt: u32,
    fingerprints: VecDeque<String>,
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
                .tell(AgentCommand::Run(RunCommand::StreamDelta {
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
        .is_some_and(|(seq, kind)| seq == marker_seq && *kind == StepKind::Agent)
}

impl Turn {
    /// The turn's bookkeeping, started if this is its first call.
    ///
    /// Lazily, because the durable state is what says a turn is owed a call:
    /// a turn that began in a process that has since died is still owed one,
    /// and the new incarnation starts counting from zero.
    fn flight(&mut self, cx: &mut Cx<'_>) -> &mut TurnFlight {
        self.flight.get_or_insert_with(|| TurnFlight {
            tool_choice: cx
                .step_run
                .pending_tool_choice
                .take()
                .unwrap_or(horsie_agentcore::ToolChoice::Auto),
            iteration: 0,
            attempt: 0,
            fingerprints: VecDeque::new(),
        })
    }

    /// Make this turn's next provider call — or fail the turn when its
    /// iteration budget is spent.
    ///
    /// Called by the boundary, which has already established that the agent
    /// owes a call, that nothing else is running, and that the contexts are
    /// fresh. Everything it reads about the conversation it reads from state.
    pub(crate) async fn run_step(&mut self, cx: &mut Cx<'_>) -> CommandEffect<AgentDomainEvent> {
        let max_iterations = cx.params.max_iterations.unwrap_or(MAX_ITERATIONS);
        if self.flight(cx).iteration >= max_iterations {
            let state = cx.state.clone();
            return self
                .fail_turn(
                    Vec::new(),
                    &state,
                    format!("max iterations exceeded (max={max_iterations})"),
                    false,
                    cx,
                )
                .await;
        }
        self.spawn_call(cx.state, cx, None);
        CommandEffect::none()
    }

    /// The message a cancelled run was part-way through writing, if it had
    /// written anything worth keeping.
    ///
    /// Reads the deltas, which are the only copy: a streamed message becomes
    /// durable when the provider finishes it, and a cancelled call never
    /// reaches that point.
    pub(crate) fn aborted_message(cx: &Cx<'_>) -> Option<Message> {
        let text = cx.step_run.deltas.concat();
        (!text.trim().is_empty()).then(|| Message::assistant_text(new_message_id(), text, now_ms()))
    }

    /// Merge `events` with whatever concluding the turn adds, in one effect.
    async fn finish(
        &mut self,
        mut events: Vec<AgentDomainEvent>,
        report: RunReport,
        folded: &AgentState,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let tail = self.conclude(report, folded, cx).await;
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
        &mut self,
        mut events: Vec<AgentDomainEvent>,
        state: &AgentState,
        error: String,
        recoverable: bool,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        events.push(self.run_aborted());
        let folded = Components::apply_all(state, &events);
        self.finish(
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
    fn run_complete(&self) -> AgentDomainEvent {
        AgentDomainEvent::RunComplete {
            iterations: self.flight.as_ref().map_or(0, |flight| flight.iteration),
            at_ms: now_ms(),
        }
    }

    /// Mark a failed or cancelled run boundary. There is no bill here: any
    /// completed provider step was persisted and banked when it returned.
    fn run_aborted(&self) -> AgentDomainEvent {
        AgentDomainEvent::RunAborted { at_ms: now_ms() }
    }

    /// Spawn one provider call over the state's prompt.
    fn spawn_call(&mut self, folded: &AgentState, cx: &mut Cx<'_>, delay: Option<Duration>) {
        let Some(tctx) = cx.step_run.ctx.clone() else {
            return;
        };
        let tool_choice = self.flight(cx).tool_choice.clone();
        // The wire-shape guard: every `tool_use` must have a `tool_result`.
        // Repairs are journaled at the moments a call becomes unanswerable, so
        // this should find nothing; it stays as the last line of defence.
        let history = repair_unanswered_tool_calls(folded.prompt_messages());
        let req = StepRequest {
            provider: tctx.provider.clone(),
            conversation_id: tctx.conversation_id.clone(),
            system_prompt: tctx.system_prompt.clone(),
            specs: tctx.specs.clone(),
            tool_choice,
            max_tokens: None,
            thinking_effort: tctx.thinking_effort,
            artifact_source: cx.runtime.artifacts.clone(),
        };
        let Some((marker_seq, StepKind::Agent)) = folded.open_step() else {
            tracing::warn!("refusing a provider call without an open Agent marker");
            return;
        };
        let cancel = cx.step_run.begin(StepPhase::CallingProvider, marker_seq);
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
                Ok(response) => RunCommand::StepDone {
                    marker_seq,
                    response: Box::new(response),
                },
                Err(StepError::Cancelled) => return,
                Err(StepError::Provider(error)) => RunCommand::StepFailed { marker_seq, error },
            };
            let _ = self_ref.tell(AgentCommand::Run(cmd)).await;
        });
    }

    /// One provider call answered: journal it, then read what it asks for.
    async fn handle_responded(
        &mut self,
        response: horsie_agentcore::StepResponse,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let flight = self.flight(cx);
        flight.iteration += 1;
        flight.attempt = 0;
        let tool_calls = extract_tool_calls(&response.message.parts);
        let mut events = vec![AgentDomainEvent::MessageComplete {
            message: response.message.clone(),
            usage: response.usage.clone(),
        }];
        let folded = Components::apply_all(cx.state, &events);

        // A truncated turn is not a finished turn. Tool calls are exempt: a
        // backend may report `length` alongside a complete tool call, and the
        // turn can still execute it and continue.
        if response.stop_reason == horsie_agentcore::StopReason::MaxTokens && tool_calls.is_empty()
        {
            return self
                .fail_turn(
                    events,
                    &folded,
                    "response truncated at the max_tokens limit".to_string(),
                    false,
                    cx,
                )
                .await;
        }

        if tool_calls.is_empty() {
            events.push(self.run_complete());
            let folded = Components::apply_all(cx.state, &events);
            return self
                .finish(
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

        // Stuck and nudge detection over the same fingerprints the old loop
        // kept.
        let fingerprint = tool_fingerprint(&tool_calls);
        let flight = self.flight(cx);
        flight.fingerprints.push_back(fingerprint.clone());
        if flight.fingerprints.len() > STUCK_THRESHOLD {
            flight.fingerprints.pop_front();
        }
        let all_same = flight.fingerprints.iter().all(|f| f == &fingerprint);
        let window = flight.fingerprints.len();
        if window >= STUCK_THRESHOLD && all_same {
            let tool = tool_calls[0].1.clone();
            return self
                .fail_turn(
                    events,
                    &folded,
                    format!(
                        "stuck in loop: tool '{tool}' called identically {STUCK_THRESHOLD} times"
                    ),
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
                    now_ms(),
                );
                events.push(AgentDomainEvent::InputMessage { message });
            }
            return CommandEffect::persist(events);
        }

        // Tool calls are not dispatched here — running what the model asked
        // for is the actor's job, not this component's. The message persists,
        // the advance that follows finds the calls open in the state, and the
        // actor runs them. This component hears nothing until they have all
        // answered.
        CommandEffect::persist(events)
    }

    /// One provider call failed: retry it, or fail the turn.
    async fn handle_llm_failed(
        &mut self,
        error: LlmError,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        // Retrying re-issues *one request* over the same history — nothing was
        // journaled by the failed attempt, so the old "did the attempt write
        // anything durable" check holds by construction.
        let max_retries = cx.params.max_retries;
        let flight = self.flight(cx);
        if error.is_transient() && flight.attempt < max_retries {
            flight.attempt += 1;
            let attempt = flight.attempt;
            let delay = error
                .retry_after()
                .unwrap_or_else(|| Duration::from_millis(50u64 * (1u64 << attempt.min(6))));
            tracing::warn!(
                error = %error,
                attempt,
                delay_ms = delay.as_millis(),
                "transient provider error; retrying the call"
            );
            let state = cx.state.clone();
            self.spawn_call(&state, cx, Some(delay));
            return CommandEffect::none();
        }
        let state = cx.state.clone();
        self.fail_turn(
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
        &mut self,
        mut events: Vec<AgentDomainEvent>,
        stopped: Vec<StoppedCall>,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        events.push(self.run_complete());
        let folded = Components::apply_all(cx.state, &events);
        self.finish(
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
    pub(crate) async fn cancelled(&mut self, cx: &mut Cx<'_>) -> CommandEffect<AgentDomainEvent> {
        let events = vec![self.run_aborted()];
        let folded = Components::apply_all(cx.state, &events);
        self.finish(
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

#[async_trait]
impl Component for Turn {
    type Command = RunCommand;

    /// Every callback is accepted only while its marker is the open top step.
    async fn handle(
        &mut self,
        cmd: RunCommand,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            RunCommand::StepDone {
                marker_seq,
                response,
            } => {
                if !open_agent_marker(cx.state, marker_seq) || !cx.step_run.finished(marker_seq) {
                    tracing::warn!(marker_seq, "dropping a callback for a closed agent step");
                    return CommandEffect::none();
                }
                self.handle_responded(*response, cx).await
            }
            RunCommand::StepFailed { marker_seq, error } => {
                if !open_agent_marker(cx.state, marker_seq) || !cx.step_run.finished(marker_seq) {
                    return CommandEffect::none();
                }
                self.handle_llm_failed(error, cx).await
            }
            RunCommand::StreamDelta { marker_seq, text } => {
                if open_agent_marker(cx.state, marker_seq)
                    && cx.step_run.push_delta(marker_seq, text)
                {
                    cx.publish_revision();
                }
                CommandEffect::none()
            }
            RunCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
            RunCommand::StopHookDone { .. } => CommandEffect::none(),
        }
    }

    /// What a run wrote, and what it cost.
    ///
    /// The cost goes to the usage part, through the one method that adds one:
    /// this component holds no numbers of its own, and cannot reach that
    /// part's fields.
    // The fallthrough is unreachable by construction: `Components::apply`
    // routes every variant to exactly one component, so an event added later
    // fails to compile *there* rather than silently reaching the wrong fold.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::MessageComplete { message, usage } => {
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
                state.bank_step_usage(usage);
            }
            AgentDomainEvent::MessageAborted { message } => {
                let at_ms = message.created_at_ms;
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
                artifacts,
                at_ms,
            } => {
                let mut message = Message::tool_result(tool_call_id, output, is_error, at_ms);
                if let Some(horsie_agentcore::ContentPart::ToolResult(r)) =
                    message.parts.first_mut()
                {
                    r.artifacts = artifacts;
                }
                state.push(at_ms, AgentLogBody::Llm(message));
            }
            AgentDomainEvent::RunComplete { .. }
            | AgentDomainEvent::RunAborted { .. }
            | AgentDomainEvent::RunCancelled { .. }
            | AgentDomainEvent::Nudged { .. } => {}
            _ => {}
        }
    }
}

/// How many turns an agent that owes a result may end without one before the
/// step is failed. Two: the first nudge is a plain message, the second forces
/// `submit_result` in `tool_choice`, and a model that defeats both is not going
/// to be talked round by a third.
pub(crate) const MAX_RESULT_NUDGES: u32 = 2;

#[derive(Debug)]
pub(crate) enum Conclusion {
    /// The step's result, and the `submit_result` call that carried it —
    /// `None` when the run stopped with no calls at all.
    Output(Value, Option<String>),
    /// One or more questions, all parked on together.
    Ask(Vec<AskedQuestion>),
    /// Two turn-enders at once. The calls are named so each can be told why.
    Contradiction(Vec<StoppedCall>),
}

impl Turn {
    /// Interpret what ended the run — a tool that stopped it, or a plain-text
    /// completion — and deliver the outcome to the parent. The turn's events
    /// were already persisted step by step, so this only records the terminal
    /// transition and lowers the gate.
    pub(crate) async fn conclude(
        &mut self,
        report: RunReport,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        self.flight = None;

        match report.outcome {
            RunOutcome::Completed { text } => {
                if cx.params.requires_result {
                    return self.ended_without_result(state, cx).await;
                }
                // An agent that owes its parent one report is not done while
                // work it delegated is still running: its conclusion would be
                // consumed now and the children's results would arrive at an
                // agent whose requester already moved on. The queue is checked
                // first — a child's report that landed mid-turn simply starts
                // the next turn — and otherwise the agent parks; the children
                // finishing is what wakes it, and its next conclusion is the
                // report.
                if cx.params.park_on_outstanding_work {
                    if state.queued_offer().is_some() {
                        return CommandEffect::none();
                    }
                    if !state.timers().is_empty()
                        || crate::agent_loop::shared::carried_state::has_outstanding_children(state)
                    {
                        let at_ms = now_ms();
                        return CommandEffect::persist(vec![
                            AgentDomainEvent::Parked { at_ms },
                            AgentDomainEvent::RunEnded {
                                reason: RunEnd::Parked,
                                at_ms,
                            },
                        ])
                        .and_snapshot();
                    }
                }
                let _ = text;
                CommandEffect::none()
            }
            RunOutcome::Stopped { calls } => {
                match Self::interpret(calls) {
                    Conclusion::Output(output, submitted) => {
                        // Submitting says the work is done, which makes any
                        // armed timer moot: nothing is left for it to wake.
                        // Dropping them here rather than calling it a
                        // contradiction keeps one rule — the agent decides when
                        // it is finished — and avoids a failure mode the agent
                        // could not have been warned about at the tool
                        // boundary, where its own timers are invisible.
                        let mut events = Vec::new();
                        // The submitting call gets its result *journaled*: a
                        // dangling `tool_use` left behind would read as an
                        // open call to the next turn — and an actor that runs
                        // open calls would submit this result a second time.
                        if let Some(tool_call_id) = submitted {
                            events.push(AgentDomainEvent::ToolComplete {
                                tool_call_id,
                                output: "result submitted".to_string(),
                                is_error: false,
                                artifacts: Vec::new(),
                                at_ms: now_ms(),
                            });
                        }
                        if !state.timers().is_empty() {
                            events.push(AgentDomainEvent::TimerCancelled {
                                ids: state.timers().iter().map(|t| t.id.clone()).collect(),
                                at_ms: now_ms(),
                            });
                        }
                        let _ = output;
                        CommandEffect::persist(events)
                    }
                    Conclusion::Ask(asks) => {
                        // An ask is a turn boundary, but a parked agent only
                        // drains for a person changing their mind — the drain
                        // told here finds the asks folded in and holds
                        // anything queued behind them.
                        let at_ms = now_ms();
                        let recorded = AgentDomainEvent::AskRecorded {
                            asks: asks.clone(),
                            at_ms,
                        };
                        CommandEffect::persist(vec![
                            recorded,
                            AgentDomainEvent::RunEnded {
                                reason: RunEnd::AwaitingInput { asks },
                                at_ms,
                            },
                        ])
                        .and_snapshot()
                    }
                    Conclusion::Contradiction(calls) => self.correct_contradiction(calls, state),
                }
            }
            RunOutcome::Cancelled => {
                // The tokens were spent whatever became of the turn that spent
                // them; the caller banked them as `RunAborted` in the same
                // batch, so the total read here includes them.
                // A cancelled tool call has no result and never will get one.
                // Journal the synthetic result now, where it belongs —
                // directly after the assistant message that made the call —
                // rather than recomputing it on a clone at the top of every
                // later turn.
                let mut events: Vec<AgentDomainEvent> =
                    missing_tool_results(&state.prompt_messages(), &parked_call_ids(state))
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect();
                // Whatever the model had already written is the only copy
                // there is: deltas are unjournaled by design, and the boundary
                // entry the stop is about to append clears them.
                //
                // After the synthetic results, not before: a cancelled call's
                // result belongs directly under the message that made it, and
                // this text is a later message than that one.
                if let Some(salvaged) = Turn::aborted_message(cx) {
                    events.push(AgentDomainEvent::MessageAborted { message: salvaged });
                }
                let at_ms = now_ms();
                events.push(AgentDomainEvent::RunCancelled { at_ms });
                events.push(AgentDomainEvent::RunEnded {
                    reason: RunEnd::Cancelled,
                    at_ms,
                });
                CommandEffect::persist(events).and_snapshot()
            }
            RunOutcome::Failed { error, recoverable } => CommandEffect::persist(vec![
                AgentDomainEvent::StepFailed {
                    reason: StepFailure::Provider(error.clone()),
                },
                AgentDomainEvent::RunEnded {
                    reason: RunEnd::Failed {
                        error,
                        recoverable,
                        terminal: false,
                    },
                    at_ms: now_ms(),
                },
            ]),
        }
    }

    /// What the tools that ended this run meant.
    ///
    /// A match on names, and nothing else. Each of these tools does exactly one
    /// thing, so there is no payload shape to disambiguate — which is the whole
    /// reason they are separate tools rather than one with a `kind` field.
    pub(crate) fn interpret(calls: Vec<StoppedCall>) -> Conclusion {
        if calls.is_empty() {
            return Conclusion::Output(Value::Null, None);
        }
        // Several questions in one turn is ordinary: they are asked together
        // and answered together.
        if calls.iter().all(|c| c.tool == ASK_USER_TOOL) {
            return Conclusion::Ask(
                calls
                    .into_iter()
                    .map(|call| AskedQuestion {
                        tool_call_id: Some(call.tool_call_id),
                        question: call
                            .input
                            .get("question")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        // Read off the same input the transcript renders from,
                        // once, here — so the inbox and the transcript offer
                        // the identical set rather than each parsing the call
                        // for themselves.
                        choices: call
                            .input
                            .get("choices")
                            .and_then(Value::as_array)
                            .map(|cs| {
                                cs.iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_string)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        multiple: call
                            .input
                            .get("multiple")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    })
                    .collect(),
            );
        }
        if let [only] = calls.as_slice()
            && only.tool == SUBMIT_RESULT_TOOL
        {
            return Conclusion::Output(only.input.clone(), Some(only.tool_call_id.clone()));
        }
        // Finishing *and* asking, or submitting twice: contradictory, and only
        // the model can resolve it. Every call gets an error result, so nothing
        // is left dangling, and the turn runs again.
        Conclusion::Contradiction(calls)
    }

    /// A step's turn ended with text instead of `submit_result`.
    ///
    /// That is legitimate exactly when something will wake this agent again: a
    /// queued message, an armed timer, or a subagent that still owes it a
    /// report. Otherwise nothing would ever start another turn and the step
    /// would sit "running" for ever, so the model is nudged — first with a
    /// plain message, then with `submit_result` forced, and only then is the
    /// step failed.
    ///
    /// All three facts are read off the shared state: the queue and the timers
    /// are in it, and the log carries every subagent lifecycle record the
    /// session wrote onto it. Nothing here asks another component anything.
    pub(crate) async fn ended_without_result(
        &mut self,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        // The queue first: a subagent report that landed while the turn was
        // ending starts the next turn, and nothing needs classifying at all.
        if state.queued_offer().is_some() {
            return CommandEffect::none();
        }
        if !state.timers().is_empty()
            || crate::agent_loop::shared::carried_state::has_outstanding_children(state)
        {
            let at_ms = now_ms();
            return CommandEffect::persist(vec![
                AgentDomainEvent::Parked { at_ms },
                AgentDomainEvent::RunEnded {
                    reason: RunEnd::Parked,
                    at_ms,
                },
            ])
            .and_snapshot();
        }
        if state.nudges() >= MAX_RESULT_NUDGES {
            let error = format!(
                "the step ended {} turns without calling `{SUBMIT_RESULT_TOOL}`, and nothing would wake it",
                state.nudges() + 1
            );
            return CommandEffect::persist(vec![AgentDomainEvent::RunEnded {
                reason: RunEnd::Failed {
                    error,
                    recoverable: false,
                    terminal: false,
                },
                at_ms: now_ms(),
            }]);
        }
        // The second attempt names the tool in `tool_choice`, so the model can
        // emit nothing else. Not the first: a model that realises it is *not*
        // finished must still be able to go back to work, and a forcing would
        // forbid that.
        if state.nudges() + 1 >= MAX_RESULT_NUDGES {
            cx.step_run.pending_tool_choice = Some(horsie_agentcore::ToolChoice::Required(
                SUBMIT_RESULT_TOOL.to_string(),
            ));
        }
        let nudge = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::User {
                id: format!("nudge-result:{}", state.nudges()),
                text: format!(
                    "Your turn ended without calling `{SUBMIT_RESULT_TOOL}`, and nothing will \
                     wake you — you have no armed timers and no delegated work still running. \
                     If the step's work is done, call `{SUBMIT_RESULT_TOOL}` now. If it is \
                     not, carry on working."
                ),
                // A nudge is the server talking to the model.
                artifacts: Vec::new(),
            },
            at_ms: now_ms(),
        };
        let nudged = AgentDomainEvent::Nudged { at_ms: now_ms() };
        // Queued, not run: the nudge is something addressed to this agent, and
        // the advance that follows this write takes it like anything else.
        CommandEffect::persist(vec![nudge, nudged])
    }

    /// The model called two turn-enders at once. Tell each call why, and run
    /// the turn again.
    ///
    /// Error results rather than silence: every `tool_use` needs a
    /// `tool_result` for the session to stay valid, and a call left
    /// dangling is indistinguishable later from a question still waiting on the
    /// user.
    pub(crate) fn correct_contradiction(
        &mut self,
        calls: Vec<StoppedCall>,
        state: &AgentState,
    ) -> CommandEffect<AgentDomainEvent> {
        let named = calls
            .iter()
            .map(|c| c.tool.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let reason = format!(
            "You ended your turn with more than one finishing tool ({named}). Do one thing: \
             either ask the user, or submit this step's result."
        );
        let at_ms = now_ms();
        let mut events: Vec<AgentDomainEvent> = calls
            .iter()
            .map(|c| AgentDomainEvent::ToolComplete {
                tool_call_id: c.tool_call_id.clone(),
                output: reason.clone(),
                is_error: true,
                artifacts: Vec::new(),
                at_ms,
            })
            .collect();
        let nudged = AgentDomainEvent::Nudged { at_ms };
        events.push(nudged);
        let folded = Components::apply_all(state, &events);
        if folded.nudges() > MAX_RESULT_NUDGES {
            return CommandEffect::persist(events);
        }
        let resume = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::Continue {
                id: format!("contradiction:{}", folded.nudges()),
                reason,
            },
            at_ms,
        };
        events.push(resume);
        CommandEffect::persist(events)
    }
}
