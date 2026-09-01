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
//! What an ending *means* is [`super::conclude`]'s half of this component.

use super::*;
use crate::agent_loop::inbox::Summarise;
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
    ctxs: Option<Arc<TurnCtx>>,
    /// Completed provider calls this turn.
    iteration: u32,
    /// Consecutive failed attempts at the *current* call.
    attempt: u32,
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
            ctxs: None,
            iteration: 0,
            attempt: 0,
            fingerprints: VecDeque::new(),
            usage: Usage::without_cache(0, 0),
            context_tokens: cx.state.context_tokens,
            pending_calls: Vec::new(),
            stopped: Vec::new(),
        });
        cx.tell(AgentCommand::Provision(ProvisionCommand::Provide(
            Box::new(ProvideJob { turn: id, cancel }),
        )))
        .await;
    }

    /// The turn is over, however it ended: clear the flight and lower the
    /// queue's gate. Called by conclude at every ending.
    pub(super) fn clear_flight(&mut self, cx: &mut Cx<'_>) {
        self.flight = None;
        cx.scratch.turn_live = false;
        cx.scratch.live_turn = None;
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
        let due = flight.ctxs.as_ref().is_some_and(|c| {
            c.budget
                .is_some_and(|b| flight.context_tokens >= b.trigger_tokens())
        });
        match due {
            true => {
                if let Some(job) = self.compact_job(None) {
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

    /// The job a compaction run needs, from this turn's contexts. `manual`
    /// carries `/compact`'s instructions; `None` is the budget check firing.
    fn compact_job(&self, manual: Option<Option<String>>) -> Option<CompactJob> {
        let flight = self.flight.as_ref()?;
        let tctx = flight.ctxs.as_ref()?;
        Some(CompactJob {
            turn: flight.id,
            manual: manual.is_some(),
            instructions: manual.flatten(),
            tokens_before: flight.context_tokens,
            budget: tctx.budget,
            provider: tctx.provider.clone(),
            conversation_id: tctx.conversation_id.clone(),
            context_provider: tctx.context_provider.clone(),
            cancel: flight.cancel.clone(),
        })
    }

    /// Spawn one provider call over the folded state's prompt.
    fn spawn_llm(&self, folded: &AgentState, cx: &Cx<'_>, delay: Option<Duration>) {
        let Some(flight) = &self.flight else { return };
        let Some(tctx) = flight.ctxs.clone() else {
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

    /// The provision component produced this turn's contexts: dispatch the
    /// turn's first real step.
    async fn handle_context_ready(
        &mut self,
        ctx_box: Box<TurnCtx>,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(flight) = self.flight.as_mut() else {
            return CommandEffect::none();
        };
        let tctx = Arc::new(*ctx_box);
        flight.ctxs = Some(tctx.clone());
        let (turn, cancel) = (flight.id, flight.cancel.clone());
        match flight.summarise.clone() {
            Some(Summarise::SubSession(sub_sessions)) => {
                cx.tell(AgentCommand::Seed(SeedCommand::TakeSummary(Box::new(
                    SummaryJob {
                        turn,
                        sub_sessions,
                        provider: tctx.provider.clone(),
                        conversation_id: tctx.conversation_id.clone(),
                        cancel,
                    },
                ))))
                .await;
                CommandEffect::none()
            }
            Some(Summarise::Compact(instructions)) => {
                if let Some(job) = self.compact_job(Some(instructions)) {
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
        // Banked the moment the call answers, before anything downstream can
        // fail: every later exit reports this call's cost.
        flight.usage.input_tokens += response.usage.input_tokens;
        flight.usage.output_tokens += response.usage.output_tokens;
        flight.usage.cache_creation_tokens = sum_optional(
            flight.usage.cache_creation_tokens,
            response.usage.cache_creation_tokens,
        );
        flight.usage.cache_read_tokens = sum_optional(
            flight.usage.cache_read_tokens,
            response.usage.cache_read_tokens,
        );
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
        let inline_names = flight
            .ctxs
            .as_ref()
            .map(|c| c.inline_names.clone())
            .unwrap_or_default();
        let Some(tctx) = flight.ctxs.clone() else {
            return CommandEffect::persist(events);
        };
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
            RunCommand::ContextReady { turn, ctx } => {
                if self.fenced(turn).is_none() {
                    return CommandEffect::none();
                }
                self.handle_context_ready(ctx, cx).await
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
            RunCommand::CompactFinished { turn } => {
                let Some(flight) = self.fenced(turn) else {
                    return CommandEffect::none();
                };
                // Whatever the compaction did is folded by now — a landed
                // boundary already moved the durable `context_tokens`, and the
                // next provider response refreshes the in-memory figure.
                // Straight to the call, not back through the budget check: a
                // compaction that left the prompt over the trigger must not
                // loop.
                let (run_id, summarise_only) = (flight.id, flight.summarise_only);
                if summarise_only {
                    let state = cx.state.clone();
                    return self.finish_empty(run_id, &state, cx).await;
                }
                let state = cx.state.clone();
                self.spawn_llm(&state, cx, None);
                CommandEffect::none()
            }
            RunCommand::SummaryDone { turn } => {
                let Some(flight) = self.fenced(turn) else {
                    return CommandEffect::none();
                };
                let (run_id, summarise_only) = (flight.id, flight.summarise_only);
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
                .deliver(crate::agent_loop::context::AgentOutcome::Interrupted {
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
