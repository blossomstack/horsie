//! The turn component: a regular agent run, and nothing else.
//!
//! A turn arrives as [`RunCommand::StartTurn`], told by the queue after it
//! journaled the turn's input. This component asks the provision component for
//! the turn's contexts, then loops: dispatch one provider call, read what came
//! back — a message that ends the turn, or tool calls to route — and dispatch
//! the next. Everything else a turn can involve lives in the component that
//! owns it and answers by command: contexts (`ContextReady`), a compaction
//! (`CompactFinished`), a sub-session summary (`SummaryDone`), every tool
//! result (`ToolReturned`). All of it is fenced by the turn's generation, so a
//! cancelled turn's stragglers are dropped.
//!
//! The second half of this file is the conclusion: what an ending *means* —
//! a park, an ask, a submitted result, a contradiction — decided here because
//! only the turn knows what would wake the agent again.

use super::*;
use crate::agent_loop::context::{AgentOutcome, AgentOutcomeSink, AskedQuestion};
use crate::agent_loop::inbox::Summarise;
use crate::agent_loop::queued_turn;
use crate::sessions::ask_tool::ASK_USER_TOOL;
use crate::sessions::workflow::SUBMIT_RESULT_TOOL;
use async_trait::async_trait;
use horsie_actor::{ActorRef, CommandEffect, ReplyTo};
use horsie_agentcore::{
    AgentEvent, AgentInput, AgentLogBody, EventSink, EventSinkError, LlmError, Message, StepError,
    StepRequest, StoppedCall, ToolOutcome, Toolbox, Usage, extract_text, extract_tool_calls,
    tool_fingerprint,
};
use horsie_models::now_ms;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Defaults the old loop carried in its config; still server policy.
const MAX_ITERATIONS: u32 = 100;
const STUCK_THRESHOLD: usize = 5;
const NUDGE_THRESHOLD: usize = 3;

/// The turn in flight, and what a crash may leave of it. All in-memory on
/// purpose — a crash mid-turn is an interrupted turn, and recovery already
/// treats it as one.
#[derive(Default)]
pub(super) struct Turn {
    /// The turn in flight, if any.
    flight: Option<TurnFlight>,
    /// Id of the next turn. Monotonic for this actor's loaded lifetime, which
    /// is all the fence needs — a report can only be stale within it.
    next_turn_id: u64,
    /// Callers waiting to hear that the in-flight turn has terminated (see
    /// [`RunCommand::Cancel`]). Drained the moment a turn concludes.
    pub(super) cancel_acks: Vec<ReplyTo<()>>,
}

/// Result of a turn, interpreted by [`super::conclude`].
pub struct RunReport {
    /// Which turn this is the report of, against the generation fence.
    pub(super) run_id: u64,
    pub(super) outcome: RunOutcome,
}

#[derive(Debug)]
pub(super) enum RunOutcome {
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
    /// The failure was already delivered to the parent; only the turn needs
    /// clearing.
    AlreadyReported,
}

/// A dispatched remote call the turn is still waiting on.
struct PendingCall {
    id: String,
    name: String,
    input: Value,
}

/// One turn's in-flight bookkeeping — what the old background loop held in
/// locals.
pub(super) struct TurnFlight {
    pub(super) id: u64,
    pub(super) cancel: CancellationToken,
    tool_choice: horsie_agentcore::ToolChoice,
    summarise: Option<Summarise>,
    summarise_only: bool,
    /// Completed provider calls this turn.
    iteration: u32,
    /// Consecutive failed attempts at the *current* call.
    attempt: u32,
    /// A compaction has been requested and no provider call has answered
    /// since. What keeps the budget check from asking again before the next
    /// response refreshes `context_tokens` — the in-memory figure goes stale
    /// the moment a boundary lands.
    compact_requested: bool,
    fingerprints: VecDeque<String>,
    /// Banked the moment each call answers, so no later failure loses it.
    usage: Usage,
    /// The last call's prompt size alone — what is loaded in context now.
    context_tokens: u32,
    pending_calls: Vec<PendingCall>,
    stopped: Vec<StoppedCall>,
}

/// Forwards streamed text chunks to the mailbox, tagged with the turn so a
/// cancelled turn's stragglers are dropped. Everything else a provider emits
/// is ignored: coarse events are journaled from the call's response, not from
/// the stream.
struct DeltaSink {
    actor: ActorRef<AgentCommand>,
    turn: u64,
}

#[async_trait]
impl EventSink for DeltaSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let AgentEvent::TextChunk(chunk) = &event {
            let _ = self
                .actor
                .tell(AgentCommand::Run(RunCommand::StreamDelta {
                    turn: self.turn,
                    text: chunk.text.clone(),
                }))
                .await;
        }
        Ok(())
    }
}

/// Sums two optional per-turn cache-token counts. Stays `None` only when
/// *neither* side reported anything — a turn/provider that's silent about
/// cache data shouldn't zero out a total another turn already contributed to.
fn sum_optional(a: Option<u32>, b: Option<u32>) -> Option<u32> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

/// Bank one call's cost into the turn's running total — the provider calls,
/// and the summarising calls other components make on this turn's behalf.
fn bank_usage(total: &mut Usage, spent: &Usage) {
    total.input_tokens += spent.input_tokens;
    total.output_tokens += spent.output_tokens;
    total.cache_creation_tokens =
        sum_optional(total.cache_creation_tokens, spent.cache_creation_tokens);
    total.cache_read_tokens = sum_optional(total.cache_read_tokens, spent.cache_read_tokens);
}

impl Turn {
    /// Begin a turn: record the flight and ask the provision component for
    /// this turn's contexts.
    ///
    /// Nothing here journals — the turn's input was journaled by whoever told
    /// `StartTurn`, and the first call dispatches only after `ContextReady`
    /// reports back, so it reads a state those events are already folded into.
    pub(super) async fn start(
        &mut self,
        cx: &mut Cx<'_>,
        summarise: Option<Summarise>,
        summarise_only: bool,
    ) {
        cx.scratch.turn_live = true;
        let cancel = CancellationToken::new();
        let id = self.next_turn_id;
        self.next_turn_id += 1;
        cx.scratch.live_turn = Some(id);
        cx.scratch.turn_cancel = Some(cancel.clone());
        self.flight = Some(TurnFlight {
            id,
            cancel: cancel.clone(),
            tool_choice: cx
                .scratch
                .pending_tool_choice
                .take()
                .unwrap_or(horsie_agentcore::ToolChoice::Auto),
            summarise,
            summarise_only,
            iteration: 0,
            attempt: 0,
            compact_requested: false,
            fingerprints: VecDeque::new(),
            usage: Usage::without_cache(0, 0),
            context_tokens: cx.state.context_tokens,
            pending_calls: Vec::new(),
            stopped: Vec::new(),
        });
        cx.tell(AgentCommand::Provision(ProvisionCommand::Provide {
            turn: id,
        }))
        .await;
    }

    /// The turn is over, however it ended: clear the flight and lower the
    /// queue's gate. Called by conclude at every ending.
    pub(super) fn clear_flight(&mut self, cx: &mut Cx<'_>) {
        self.flight = None;
        cx.scratch.turn_live = false;
        cx.scratch.live_turn = None;
        cx.scratch.turn_cancel = None;
        cx.scratch.turn_ctx = None;
    }

    /// The turn in flight's id, for conclude's superseded-report guard.
    pub(super) fn flight_id(&self) -> Option<u64> {
        self.flight.as_ref().map(|f| f.id)
    }

    /// The message a cancelled run was part-way through writing, if it had
    /// written anything worth keeping.
    ///
    /// Reads the deltas, which are the only copy: a streamed message becomes
    /// durable when the provider finishes it, and a cancelled call never
    /// reaches that point.
    pub(super) fn aborted_message(cx: &Cx<'_>) -> Option<Message> {
        let text = cx.scratch.deltas.concat();
        (!text.trim().is_empty()).then(|| Message::assistant_text(new_message_id(), text, now_ms()))
    }

    /// The turn in flight, if `turn` still names it — the generation fence.
    fn fenced(&mut self, turn: u64) -> Option<&mut TurnFlight> {
        self.flight.as_mut().filter(|f| f.id == turn)
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
        let Some(flight) = &self.flight else {
            return CommandEffect::persist(events);
        };
        let run_id = flight.id;
        events.push(AgentDomainEvent::RunAborted {
            usage: flight.usage.clone(),
            context_tokens: flight.context_tokens,
            at_ms: now_ms(),
        });
        let folded = Components::apply_all(state, &events);
        self.finish(
            events,
            RunReport {
                run_id,
                outcome: RunOutcome::Failed { error, recoverable },
            },
            &folded,
            cx,
        )
        .await
    }

    /// Conclude the turn with the empty completion a summarise-only turn ends
    /// with: nothing was said to the model, and nothing is owed.
    async fn finish_empty(
        &mut self,
        run_id: u64,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        self.finish(
            Vec::new(),
            RunReport {
                run_id,
                outcome: RunOutcome::Completed {
                    text: String::new(),
                },
            },
            state,
            cx,
        )
        .await
    }

    /// Dispatch the turn's next model-facing step: hand over to the compaction
    /// component when the budget says so, otherwise spawn the next provider
    /// call — or fail the turn when its iteration budget is spent.
    async fn dispatch_model_step(
        &mut self,
        events: Vec<AgentDomainEvent>,
        folded: &AgentState,
        cx: &mut Cx<'_>,
        delay: Option<Duration>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(flight) = &self.flight else {
            return CommandEffect::persist(events);
        };
        let max_iterations = cx.params.max_iterations.unwrap_or(MAX_ITERATIONS);
        if flight.iteration >= max_iterations {
            return self
                .fail_turn(
                    events,
                    folded,
                    format!("max iterations exceeded (max={max_iterations})"),
                    false,
                    cx,
                )
                .await;
        }
        let due = !flight.compact_requested
            && cx.scratch.turn_ctx.as_ref().is_some_and(|c| {
                c.budget
                    .is_some_and(|b| flight.context_tokens >= b.trigger_tokens())
            });
        match due {
            true => {
                if let Some(job) = self.compact_job(None) {
                    if let Some(flight) = self.flight.as_mut() {
                        flight.compact_requested = true;
                    }
                    cx.tell(AgentCommand::Compaction(CompactionCommand::Compact(
                        Box::new(job),
                    )))
                    .await;
                }
            }
            false => self.spawn_llm(folded, cx, delay),
        }
        CommandEffect::persist(events)
    }

    /// The facts a compaction run needs and only this turn knows. `manual`
    /// carries `/compact`'s instructions; `None` is the budget check firing.
    fn compact_job(&self, manual: Option<Option<String>>) -> Option<CompactJob> {
        let flight = self.flight.as_ref()?;
        Some(CompactJob {
            turn: flight.id,
            manual: manual.is_some(),
            instructions: manual.flatten(),
            tokens_before: flight.context_tokens,
        })
    }

    /// Spawn one provider call over the folded state's prompt.
    fn spawn_llm(&self, folded: &AgentState, cx: &Cx<'_>, delay: Option<Duration>) {
        let Some(flight) = &self.flight else { return };
        let Some(tctx) = cx.scratch.turn_ctx.clone() else {
            return;
        };
        // The wire-shape guard: every `tool_use` must have a `tool_result`.
        // Repairs are journaled at the moments a call becomes unanswerable, so
        // this should find nothing; it stays as the last line of defence.
        let history = repair_unanswered_tool_calls(folded.prompt_messages());
        let req = StepRequest {
            provider: tctx.provider.clone(),
            conversation_id: tctx.conversation_id.clone(),
            system_prompt: tctx.system_prompt.clone(),
            specs: tctx.specs.clone(),
            tool_choice: flight.tool_choice.clone(),
            max_tokens: None,
            thinking_effort: tctx.thinking_effort,
            artifact_source: cx.runtime.artifacts.clone(),
        };
        let turn = flight.id;
        let cancel = flight.cancel.clone();
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
                turn,
            };
            let cmd = match horsie_agentcore::run_step(&req, &history, &sink, &cancel).await {
                Ok(response) => RunCommand::LlmResponded {
                    turn,
                    response: Box::new(response),
                },
                // The Cancel handler already concluded the turn; reporting
                // would be answered by the fence anyway.
                Err(StepError::Cancelled) => return,
                Err(StepError::Provider(error)) => RunCommand::LlmFailed { turn, error },
            };
            let _ = self_ref.tell(AgentCommand::Run(cmd)).await;
        });
    }

    /// The provision component published this turn's contexts: dispatch the
    /// turn's first real step.
    async fn handle_context_ready(&mut self, cx: &mut Cx<'_>) -> CommandEffect<AgentDomainEvent> {
        let Some(flight) = self.flight.as_mut() else {
            return CommandEffect::none();
        };
        let turn = flight.id;
        match flight.summarise.clone() {
            Some(Summarise::SubSession(sub_sessions)) => {
                cx.tell(AgentCommand::Seed(SeedCommand::TakeSummary {
                    turn,
                    sub_sessions,
                }))
                .await;
                CommandEffect::none()
            }
            Some(Summarise::Compact(instructions)) => {
                if let Some(job) = self.compact_job(Some(instructions)) {
                    if let Some(flight) = self.flight.as_mut() {
                        flight.compact_requested = true;
                    }
                    cx.tell(AgentCommand::Compaction(CompactionCommand::Compact(
                        Box::new(job),
                    )))
                    .await;
                }
                CommandEffect::none()
            }
            None => {
                let state = cx.state.clone();
                self.dispatch_model_step(Vec::new(), &state, cx, None).await
            }
        }
    }

    /// One provider call answered: journal it, then read what it asks for.
    async fn handle_responded(
        &mut self,
        response: horsie_agentcore::StepResponse,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(flight) = self.flight.as_mut() else {
            return CommandEffect::none();
        };
        flight.iteration += 1;
        flight.attempt = 0;
        flight.compact_requested = false;
        // Banked the moment the call answers, before anything downstream can
        // fail: every later exit reports this call's cost.
        bank_usage(&mut flight.usage, &response.usage);
        flight.context_tokens = response.usage.input_tokens;

        let tool_calls = extract_tool_calls(&response.message.parts);
        let mut events = vec![AgentDomainEvent::MessageComplete {
            message: response.message.clone(),
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
            events.push(AgentDomainEvent::RunComplete {
                usage: flight.usage.clone(),
                iterations: flight.iteration,
                context_tokens: flight.context_tokens,
                at_ms: now_ms(),
            });
            let run_id = flight.id;
            let folded = Components::apply_all(cx.state, &events);
            return self
                .finish(
                    events,
                    RunReport {
                        run_id,
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
            // history — where the old loop kept them in run-task locals.
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
            let folded = Components::apply_all(cx.state, &events);
            return self.dispatch_model_step(events, &folded, cx, None).await;
        }

        // Route the batch. A component-claimed tool becomes a command to its
        // component; everything else goes to the toolbox on a spawned task.
        // Both answer on the same channel — `ToolReturned` — so the batch
        // bookkeeping cannot tell them apart.
        let Some(tctx) = cx.scratch.turn_ctx.clone() else {
            return CommandEffect::persist(events);
        };
        let inline_names = tctx.inline_names.clone();
        let turn = flight.id;
        let cancel = flight.cancel.clone();
        let self_ref = cx.actor.self_ref();
        for (id, name, input) in tool_calls {
            let call = PendingCall {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            };
            // Claimed AND permitted: a filtered-out component tool still goes
            // to the toolbox, whose filter answers "not permitted".
            let routed = match inline_names.contains(&name) {
                true => component::route_tool_call(ComponentToolCall {
                    turn,
                    tool_call_id: id,
                    name,
                    input,
                }),
                false => None,
            };
            match routed {
                Some(cmd) => cx.tell(cmd).await,
                None => spawn_tool_call(
                    &tctx.toolbox,
                    call.name.clone(),
                    call.input.clone(),
                    call.id.clone(),
                    turn,
                    cancel.clone(),
                    self_ref.clone(),
                ),
            }
            flight.pending_calls.push(call);
        }
        CommandEffect::persist(events)
    }

    /// One provider call failed: retry it, or fail the turn.
    async fn handle_llm_failed(
        &mut self,
        error: LlmError,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(flight) = self.flight.as_mut() else {
            return CommandEffect::none();
        };
        // Retrying re-issues *one request* over the same folded history —
        // nothing was journaled by the failed attempt, so the old "did the
        // attempt write anything durable" check holds by construction.
        if error.is_transient() && flight.attempt < cx.params.max_retries {
            flight.attempt += 1;
            let delay = error
                .retry_after()
                .unwrap_or_else(|| Duration::from_millis(50u64 * (1u64 << flight.attempt.min(6))));
            tracing::warn!(
                error = %error,
                attempt = flight.attempt,
                delay_ms = delay.as_millis(),
                "transient provider error; retrying the call"
            );
            self.spawn_llm(cx.state, cx, Some(delay));
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

    /// One dispatched tool call answered.
    async fn handle_tool_returned(
        &mut self,
        turn: u64,
        tool_call_id: String,
        outcome: ToolReturn,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(flight) = self.fenced(turn) else {
            tracing::warn!(turn, tool_call_id, "dropping a superseded tool result");
            return CommandEffect::none();
        };
        let Some(position) = flight
            .pending_calls
            .iter()
            .position(|c| c.id == tool_call_id)
        else {
            tracing::warn!(tool_call_id, "a tool result answered no pending call");
            return CommandEffect::none();
        };
        let call = flight.pending_calls.remove(position);
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
                at_ms: now_ms(),
            }),
            // No result is recorded for a stopper: the dangling `tool_use` is
            // what an answer arrives against later.
            ToolReturn::Stopped => flight.stopped.push(StoppedCall {
                tool: call.name,
                tool_call_id: call.id,
                input: call.input,
            }),
        }
        let folded = Components::apply_all(cx.state, &events);
        if !flight.pending_calls.is_empty() {
            return CommandEffect::persist(events);
        }
        // The batch settled.
        let stopped = std::mem::take(&mut flight.stopped);
        let run_id = flight.id;
        if stopped.is_empty() {
            return self.dispatch_model_step(events, &folded, cx, None).await;
        }
        self.finish(
            events,
            RunReport {
                run_id,
                outcome: RunOutcome::Stopped { calls: stopped },
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

    async fn handle(
        &mut self,
        cmd: RunCommand,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            RunCommand::StartTurn {
                summarise,
                summarise_only,
            } => {
                // The queue gates on `turn_live`, so a second start reaching
                // here is a bug — refuse it rather than orphan the first
                // turn's cancel token.
                if self.flight.is_some() {
                    tracing::warn!("refusing to start a turn while one is in flight");
                    return CommandEffect::none();
                }
                self.start(cx, summarise, summarise_only).await;
                CommandEffect::none()
            }
            RunCommand::ContextReady { turn } => {
                if self.fenced(turn).is_none() {
                    return CommandEffect::none();
                }
                self.handle_context_ready(cx).await
            }
            RunCommand::ContextFailed { turn } => {
                if self.fenced(turn).is_none() {
                    return CommandEffect::none();
                }
                let state = cx.state.clone();
                self.conclude(
                    RunReport {
                        run_id: turn,
                        outcome: RunOutcome::AlreadyReported,
                    },
                    &state,
                    cx,
                )
                .await
            }
            RunCommand::LlmResponded { turn, response } => {
                if self.fenced(turn).is_none() {
                    tracing::warn!(turn, "dropping a superseded provider response");
                    return CommandEffect::none();
                }
                self.handle_responded(*response, cx).await
            }
            RunCommand::LlmFailed { turn, error } => {
                if self.fenced(turn).is_none() {
                    return CommandEffect::none();
                }
                self.handle_llm_failed(error, cx).await
            }
            RunCommand::Resume { turn, usage } => {
                let Some(flight) = self.fenced(turn) else {
                    return CommandEffect::none();
                };
                // Whoever paused this turn spent on its behalf; the cost is
                // the turn's cost.
                if let Some(usage) = &usage {
                    bank_usage(&mut flight.usage, usage);
                }
                let (run_id, summarise_only) = (flight.id, flight.summarise_only);
                // A turn that was only ever its summarisation is over; any
                // other turn takes its next step against the folded state.
                if summarise_only {
                    let state = cx.state.clone();
                    return self.finish_empty(run_id, &state, cx).await;
                }
                let state = cx.state.clone();
                self.dispatch_model_step(Vec::new(), &state, cx, None).await
            }
            RunCommand::Cancel { ack } => {
                match (&self.flight, ack) {
                    (Some(flight), ack) => {
                        flight.cancel.cancel();
                        // Answered by the conclusion below: the fence
                        // guarantees the dying tasks can write nothing more,
                        // so "the run is over" is true the moment it lands.
                        self.cancel_acks.extend(ack);
                        let run_id = flight.id;
                        // Bank what the turn spent before concluding — the
                        // conclusion reads usage off the folded state.
                        let events = vec![AgentDomainEvent::RunAborted {
                            usage: flight.usage.clone(),
                            context_tokens: flight.context_tokens,
                            at_ms: now_ms(),
                        }];
                        let folded = Components::apply_all(cx.state, &events);
                        self.finish(
                            events,
                            RunReport {
                                run_id,
                                outcome: RunOutcome::Cancelled,
                            },
                            &folded,
                            cx,
                        )
                        .await
                    }
                    // Nothing in flight (idle, or paused on a pending ask): the
                    // caller's guarantee already holds.
                    (None, Some(ack)) => {
                        let _ = ack.send(());
                        CommandEffect::none()
                    }
                    (None, None) => CommandEffect::none(),
                }
            }
            RunCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
            RunCommand::ToolReturned {
                turn,
                tool_call_id,
                outcome,
            } => {
                self.handle_tool_returned(turn, tool_call_id, outcome, cx)
                    .await
            }
            RunCommand::StreamDelta { turn, text } => {
                // The fence again: a dead turn's chunks must not pollute the
                // next turn's delta buffer.
                if self.flight.as_ref().map(|f| f.id) == Some(turn) {
                    cx.scratch.deltas.push(text);
                    cx.publish_revision();
                }
                CommandEffect::none()
            }
        }
    }

    /// What a run wrote, and what it cost.
    // The fallthrough is unreachable by construction: `Components::apply`
    // routes every variant to exactly one component, so an event added later
    // fails to compile *there* rather than silently reaching the wrong fold.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn apply(state: &mut AgentState, event: AgentDomainEvent) {
        match event {
            AgentDomainEvent::MessageComplete { message }
            | AgentDomainEvent::MessageAborted { message } => {
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
            AgentDomainEvent::RunComplete {
                usage,
                context_tokens,
                ..
            } => {
                state.usage_total.add(&usage);
                state.context_tokens = context_tokens;
                state.last_turn_usage = Some(usage);
                state.turn_in_flight = false;
            }
            AgentDomainEvent::RunAborted {
                usage,
                context_tokens,
                ..
            } => {
                state.usage_total.add(&usage);
                state.context_tokens = context_tokens;
                state.turn_in_flight = false;
            }
            AgentDomainEvent::RunCancelled { .. } => state.turn_in_flight = false,
            AgentDomainEvent::Nudged { .. } => {
                state.nudges = state.nudges.saturating_add(1);
                state.turn_in_flight = false;
            }
            _ => {}
        }
    }

    /// Repair the tool call the dead process was running, report the turn it
    /// died inside, and — for an agent nobody will message — re-drive it.
    async fn on_load(&mut self, cx: &mut Cx<'_>) {
        // A tool call the dead process was running has no result and never
        // will. Record the repair once, here, where it still belongs at the
        // end of the transcript.
        let repairs = missing_tool_results(&cx.state.prompt_messages(), &parked_call_ids(cx.state));
        if !repairs.is_empty() {
            let (ack, _) = tokio::sync::oneshot::channel();
            let ack = ReplyTo::from_sender(ack);
            cx.tell(AgentCommand::Run(RunCommand::PersistProgress {
                events: repairs
                    .into_iter()
                    .map(|message| AgentDomainEvent::InputMessage { message })
                    .collect(),
                ack,
            }))
            .await;
        }
        // A turn still open in the fold is one no process is running any more.
        // Tell the owner, from here rather than from a command: this hook runs
        // before the first live command, so the report is ordered ahead of
        // anything queued while the actor was loading.
        if cx.state.turn_in_flight {
            cx.runtime
                .parent
                .deliver(AgentOutcome::Interrupted {
                    agent: cx.runtime.journal_id,
                })
                .await;
        }
        // Interactive sessions never self-continue: the user's next message is
        // the continuation. An empty history means nothing ran yet, and a
        // parked agent is waiting for a timer — neither is an interrupted
        // turn.
        if cx.params.interactive || cx.state.parked || cx.state.log.is_empty() {
            return;
        }
        // The continuation is journaled — the fold is the only source of
        // history, so an input the model must see has to be in it.
        let message = AgentInput::user_message(new_message_id(), "continue the interrupted task")
            .to_message(now_ms());
        let (ack, _) = tokio::sync::oneshot::channel();
        cx.tell(AgentCommand::Run(RunCommand::PersistProgress {
            events: vec![AgentDomainEvent::InputMessage { message }],
            ack: ReplyTo::from_sender(ack),
        }))
        .await;
        self.start(cx, None, false).await;
    }
}

/// Dispatch one remote tool call on its own task. Whether the toolbox guards
/// its wire with timeouts is its own business; cancel is the rescue either
/// way, and the fence drops whatever a dead turn's task still says.
fn spawn_tool_call(
    toolbox: &Arc<dyn Toolbox>,
    name: String,
    input: Value,
    tool_call_id: String,
    turn: u64,
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
            .tell(AgentCommand::Run(RunCommand::ToolReturned {
                turn,
                tool_call_id,
                outcome,
            }))
            .await;
    });
}

/// How many turns an agent that owes a result may end without one before the
/// step is failed. Two: the first nudge is a plain message, the second forces
/// `submit_result` in `tool_choice`, and a model that defeats both is not going
/// to be talked round by a third.
pub(super) const MAX_RESULT_NUDGES: u32 = 2;

#[derive(Debug)]
pub(super) enum Conclusion {
    Output(Value),
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
    pub(super) async fn conclude(
        &mut self,
        report: RunReport,
        state: &AgentState,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        // A report from a turn that has already been superseded says nothing
        // about the one in flight now.
        if self.flight_id() != Some(report.run_id) {
            tracing::warn!(
                run_id = report.run_id,
                current = ?self.flight_id(),
                "dropping the report of a superseded run"
            );
            return CommandEffect::none();
        }
        // The turn is over before anything below is decided: clear the flight
        // and lower the queue's gate, so the drains told below can start the
        // next one.
        self.clear_flight(cx);
        // Answered before any parent delivery below: a canceller is likely
        // blocking its own mailbox waiting on this, and those deliveries
        // `tell` into that same mailbox — replying first keeps the two from
        // deadlocking. The fence guarantees a cancelled turn's tasks can make
        // nothing more durable, so "it will write nothing more" is true now.
        for ack in self.cancel_acks.drain(..) {
            let _ = ack.send(());
        }
        let agent = cx.runtime.journal_id;
        let parent = cx.runtime.parent.clone();

        match report.outcome {
            RunOutcome::Completed { text } => {
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                        context_tokens: state.context_tokens,
                    })
                    .await;
                if cx.params.requires_result {
                    return self.ended_without_result(state, cx, agent, parent).await;
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
                    if queued_turn(&state.inbox, &state.asks).is_some() {
                        cx.drain().await;
                        return CommandEffect::none();
                    }
                    if !state.timers.is_empty()
                        || crate::agent_loop::carried_state::has_outstanding_children(state)
                    {
                        parent.deliver(AgentOutcome::Parked { agent }).await;
                        let parked = AgentDomainEvent::Parked { at_ms: now_ms() };
                        return CommandEffect::persist(vec![parked]).and_snapshot();
                    }
                }
                parent
                    .deliver(AgentOutcome::Concluded {
                        agent,
                        output: Value::String(text),
                    })
                    .await;
                // Resident: the agent goes idle, it does not die. A turn
                // ending is a boundary, so whatever queued while it ran
                // starts the next one.
                cx.drain().await;
                CommandEffect::none()
            }
            RunOutcome::Stopped { calls } => {
                match Self::interpret(calls) {
                    Conclusion::Output(output) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                                context_tokens: state.context_tokens,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Concluded { agent, output })
                            .await;
                        // Submitting says the work is done, which makes any
                        // armed timer moot: nothing is left for it to wake.
                        // Dropping them here rather than calling it a
                        // contradiction keeps one rule — the agent decides when
                        // it is finished — and avoids a failure mode the agent
                        // could not have been warned about at the tool
                        // boundary, where its own timers are invisible.
                        let mut events = Vec::new();
                        if !state.timers.is_empty() {
                            events.push(AgentDomainEvent::TimerCancelled {
                                ids: state.timers.iter().map(|t| t.id.clone()).collect(),
                                at_ms: now_ms(),
                            });
                        }
                        cx.drain().await;
                        CommandEffect::persist(events)
                    }
                    Conclusion::Ask(asks) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                                context_tokens: state.context_tokens,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Asked {
                                agent,
                                asks: asks.clone(),
                            })
                            .await;
                        // An ask is a turn boundary, but a parked agent only
                        // drains for a person changing their mind — the drain
                        // told here finds the asks folded in and holds
                        // anything queued behind them.
                        let recorded = AgentDomainEvent::AskRecorded {
                            asks,
                            at_ms: now_ms(),
                        };
                        cx.drain().await;
                        // Snapshot to compact the incrementally-persisted log:
                        // history and streams read state, so it is invisible.
                        CommandEffect::persist(vec![recorded]).and_snapshot()
                    }
                    Conclusion::Contradiction(calls) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                                context_tokens: state.context_tokens,
                            })
                            .await;
                        self.correct_contradiction(calls, state, cx).await
                    }
                }
            }
            RunOutcome::Cancelled => {
                // The tokens were spent whatever became of the turn that spent
                // them; the caller banked them as `RunAborted` in the same
                // batch, so the total read here includes them.
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                        context_tokens: state.context_tokens,
                    })
                    .await;
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
                events.push(AgentDomainEvent::RunCancelled { at_ms: now_ms() });
                // A stop cancels the turn, not the promise: anything queued
                // while the cancelled turn ran starts the next one.
                cx.drain().await;
                CommandEffect::persist(events).and_snapshot()
            }
            RunOutcome::Failed { error, recoverable } => {
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                        context_tokens: state.context_tokens,
                    })
                    .await;
                parent
                    .deliver(AgentOutcome::Failed {
                        agent,
                        error,
                        recoverable,
                        // A run that failed inside the loop says nothing about
                        // whether the sandbox still exists.
                        terminal: false,
                    })
                    .await;
                // The partial turn was already journaled step by step, so the
                // failed run stays inspectable. The agent stays alive: a
                // failed turn is not a dead agent, and the next message
                // reuses it.
                CommandEffect::none()
            }
            RunOutcome::AlreadyReported => {
                // The failure was already delivered to the parent. Stay alive
                // so the next message can retry against the same transcript.
                CommandEffect::none()
            }
        }
    }

    /// What the tools that ended this run meant.
    ///
    /// A match on names, and nothing else. Each of these tools does exactly one
    /// thing, so there is no payload shape to disambiguate — which is the whole
    /// reason they are separate tools rather than one with a `kind` field.
    pub(super) fn interpret(calls: Vec<StoppedCall>) -> Conclusion {
        if calls.is_empty() {
            return Conclusion::Output(Value::Null);
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
            return Conclusion::Output(only.input.clone());
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
    pub(super) async fn ended_without_result(
        &mut self,
        state: &AgentState,
        cx: &mut Cx<'_>,
        agent: uuid::Uuid,
        parent: Arc<dyn AgentOutcomeSink>,
    ) -> CommandEffect<AgentDomainEvent> {
        // The queue first: a subagent report that landed while the turn was
        // ending starts the next turn, and nothing needs classifying at all.
        if queued_turn(&state.inbox, &state.asks).is_some() {
            cx.drain().await;
            return CommandEffect::none();
        }
        if !state.timers.is_empty()
            || crate::agent_loop::carried_state::has_outstanding_children(state)
        {
            parent.deliver(AgentOutcome::Parked { agent }).await;
            let parked = AgentDomainEvent::Parked { at_ms: now_ms() };
            return CommandEffect::persist(vec![parked]).and_snapshot();
        }
        if state.nudges >= MAX_RESULT_NUDGES {
            parent
                .deliver(AgentOutcome::Failed {
                    agent,
                    error: format!(
                        "the step ended {} turns without calling `{SUBMIT_RESULT_TOOL}`, \
                         and nothing would wake it",
                        state.nudges + 1
                    ),
                    recoverable: false,
                    terminal: false,
                })
                .await;
            return CommandEffect::none();
        }
        // The second attempt names the tool in `tool_choice`, so the model can
        // emit nothing else. Not the first: a model that realises it is *not*
        // finished must still be able to go back to work, and a forcing would
        // forbid that.
        if state.nudges + 1 >= MAX_RESULT_NUDGES {
            cx.scratch.pending_tool_choice = Some(horsie_agentcore::ToolChoice::Required(
                SUBMIT_RESULT_TOOL.to_string(),
            ));
        }
        let nudge = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::User {
                id: format!("nudge-result:{}", state.nudges),
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
        // The drain told here finds the nudge folded into the queue and starts
        // the turn that answers it.
        cx.drain().await;
        CommandEffect::persist(vec![nudge, nudged])
    }

    /// The model called two turn-enders at once. Tell each call why, and run
    /// the turn again.
    ///
    /// Error results rather than silence: every `tool_use` needs a
    /// `tool_result` for the session to stay valid, and a call left
    /// dangling is indistinguishable later from a question still waiting on the
    /// user.
    pub(super) async fn correct_contradiction(
        &mut self,
        calls: Vec<StoppedCall>,
        state: &AgentState,
        cx: &mut Cx<'_>,
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
        if folded.nudges > MAX_RESULT_NUDGES {
            return CommandEffect::persist(events);
        }
        let resume = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::Continue {
                id: format!("contradiction:{}", folded.nudges),
                reason,
            },
            at_ms,
        };
        events.push(resume);
        // The continuation is in the queue once these persist; the drain
        // starts the turn that acts on it.
        cx.drain().await;
        CommandEffect::persist(events)
    }
}
