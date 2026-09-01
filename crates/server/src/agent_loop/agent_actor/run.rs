//! The turn component: the actor-owned loop.
//!
//! There is no background run loop any more. This component drives each step
//! of a turn: it dispatches one provider call, reads what came back, routes
//! the tool calls, and dispatches the next call — journaling each step as an
//! ordinary command effect. Everything that does I/O — the per-turn setup, the
//! provider call, a remote tool call, a summarisation — happens on a spawned
//! task, never on the mailbox, and reports back as a command carrying the
//! turn's generation so a cancelled turn's stragglers are dropped.
//!
//! A turn arrives as [`RunCommand::StartTurn`], told by the queue after it
//! journaled the turn's input — the two components never call each other.
//! What an ending *means* is [`super::conclude`]'s half of this component.

use super::*;
use crate::agent_loop::inbox::Summarise;
use async_trait::async_trait;
use horsie_actor::{ActorRef, CommandEffect, ReplyTo};
use horsie_agentcore::{
    AgentEvent, AgentInput, AgentLogBody, CompactionBudget, EventSink, EventSinkError, LlmError,
    Message, StepError, StepRequest, StoppedCall, ToolOutcome, ToolSpec, Toolbox, Usage,
    extract_text, extract_tool_calls, tool_fingerprint,
};
use horsie_models::now_ms;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

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
    /// A summary this run was asked to take for sub sessions waiting on it.
    /// Delivered directly by the `Summarised` handler now, so always `None`
    /// here — kept on the struct because conclude owns the delivery seam.
    pub(super) seed_summary: Option<SeedSummary>,
}

/// What a run produced for the sub sessions waiting on it.
#[derive(Debug, Clone)]
pub struct SeedSummary {
    /// Every sub session seeded from this one summary. They share a branch
    /// point, so they are entitled to share the provider call.
    pub sub_sessions: Vec<Uuid>,
    pub result: Result<String, String>,
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

/// Everything one turn's steps share, built once by the setup task.
pub(super) struct TurnCtx {
    pub provider: Arc<dyn horsie_agentcore::LlmProvider>,
    /// The fully-composed, selection-filtered toolbox remote calls dispatch
    /// through. Inline tools (timers, `task_list`) never reach it — this
    /// component claims them by name first.
    pub toolbox: Arc<dyn Toolbox>,
    /// What the model is shown, already filtered.
    pub specs: Vec<ToolSpec>,
    /// The inline tool names that survived the filter, claimed on the mailbox.
    pub inline_names: std::collections::HashSet<String>,
    pub system_prompt: String,
    pub budget: Option<CompactionBudget>,
    pub conversation_id: String,
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// For the compaction hooks a compact step fires.
    pub context_provider: Arc<dyn crate::agent_loop::ContextProvider>,
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

/// What one spawned step reported back.
pub struct StepReport {
    pub(super) turn: u64,
    pub(super) outcome: StepOutcome,
}

pub(super) enum StepOutcome {
    /// The per-turn setup finished: runtime rehydrated, MCP connected,
    /// workspace scanned, toolbox composed.
    Prepared(Box<TurnCtx>),
    /// The setup could not produce this turn's contexts.
    ProvideFailed(crate::agent_loop::ContextError),
    /// The summary sub sessions were waiting on was taken (or failed).
    Summarised {
        sub_sessions: Vec<Uuid>,
        result: Result<String, String>,
    },
    /// A compaction happened; the boundary is journaled here.
    Compacted(Box<CompactedData>),
    /// A compaction found nothing to fold, was refused by a hook, or failed.
    /// `notice` says whether to tell the user (a typed `/compact` deserves an
    /// answer; the auto check declining is routine).
    CompactSkipped { notice: bool },
    /// One provider call finished.
    Responded(Box<horsie_agentcore::StepResponse>),
    /// One provider call failed.
    LlmFailed(LlmError),
}

/// What a compact step produced, ready to journal as
/// [`AgentDomainEvent::Compacted`].
pub(super) struct CompactedData {
    summary: String,
    carried_state: String,
    retained_from_message_id: Option<String>,
    trigger: horsie_agentcore::CompactionTrigger,
    instructions: Option<String>,
    tokens_before: u32,
    tokens_after: u32,
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

/// Adds the timer and `task_list` specs to a composed toolbox so the
/// selection filter sees the whole surface. Execution never reaches it for
/// those names — the turn claims them on the mailbox — so its `execute` for
/// them is an error by construction.
struct WithInlineSpecs {
    inner: Arc<dyn Toolbox>,
}

#[async_trait]
impl Toolbox for WithInlineSpecs {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(component::component_tool_specs());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
        tool_call_id: &str,
    ) -> Result<ToolOutcome, horsie_agentcore::ToolCallError> {
        if component::is_component_tool(name) {
            return Err(horsie_agentcore::ToolCallError::InvalidInput(format!(
                "'{name}' is handled by its component"
            )));
        }
        self.inner.execute(name, input, tool_call_id).await
    }
}

/// Forwards streamed text chunks to the mailbox, tagged with the turn so a
/// cancelled turn's stragglers are dropped. Everything else a provider emits
/// is ignored: coarse events are journaled from the step's response, not from
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

impl Turn {
    /// Begin a turn: record the flight and spawn the per-turn setup.
    ///
    /// Nothing here journals — the turn's input was journaled by whoever told
    /// `StartTurn`, and the first step dispatches only after `Prepared`
    /// reports back, so it reads a state those events are already folded into.
    pub(super) fn start(
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

        let self_ref = cx.actor.self_ref();
        let context_provider = cx.runtime.context_provider.clone();
        let configured_prompt = cx.params.system_prompt.clone();
        let run_def_tools = cx.params.tools.clone();
        let thinking_effort = cx.params.thinking_effort;
        let conversation_id = cx.runtime.journal_id.to_string();

        tokio::spawn(async move {
            // Provide this turn's contexts off the mailbox: rehydrate the
            // runtime, reconnect MCP, scan the workspace. Cancellable, because
            // this is the *most* likely place to hang — it awaits an MCP
            // connect and a workspace scan across a process boundary.
            let provided = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                provided = context_provider.provide() => provided,
            };
            let outcome = match provided {
                Err(error) => StepOutcome::ProvideFailed(error),
                Ok(contexts) => {
                    // The inline specs join before the filter so the agent's
                    // selection reaches them exactly as it reaches every other
                    // layer. A plugin's narrowing stacks after: two filters
                    // can only remove, so the narrower wins.
                    let composed: Arc<dyn Toolbox> = Arc::new(WithInlineSpecs {
                        inner: contexts.toolbox,
                    });
                    let toolbox = crate::agent_loop::FilteredToolbox::apply(
                        composed,
                        run_def_tools.as_deref(),
                    );
                    let toolbox = match &contexts.tool_narrowing {
                        None => toolbox,
                        Some(narrowed) => {
                            crate::agent_loop::FilteredToolbox::apply(toolbox, Some(narrowed))
                        }
                    };
                    let specs = toolbox.specs();
                    let inline_names = specs
                        .iter()
                        .map(|s| s.name.clone())
                        .filter(|n| component::is_component_tool(n))
                        .collect();
                    StepOutcome::Prepared(Box::new(TurnCtx {
                        provider: contexts.provider,
                        toolbox,
                        specs,
                        inline_names,
                        system_prompt: contexts
                            .system_prompt
                            .or(configured_prompt)
                            .unwrap_or_default(),
                        budget: contexts
                            .context_window
                            .map(|context_window| CompactionBudget {
                                context_window,
                                trigger_at_percent: COMPACT_AT_PERCENT,
                                retain_percent: COMPACT_RETAIN_PERCENT,
                            }),
                        conversation_id,
                        thinking_effort,
                        context_provider,
                    }))
                }
            };
            let _ = self_ref
                .tell(AgentCommand::Run(RunCommand::StepDone(Box::new(
                    StepReport { turn: id, outcome },
                ))))
                .await;
        });
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
                seed_summary: None,
            },
            &folded,
            cx,
        )
        .await
    }

    /// Dispatch the turn's next model-facing step: a compaction when one is
    /// due, otherwise the next provider call — or fail the turn when its
    /// iteration budget is spent.
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
            true => self.spawn_compact(folded, None, cx),
            false => self.spawn_llm(folded, cx, delay),
        }
        CommandEffect::persist(events)
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
            let outcome = match horsie_agentcore::run_step(&req, &history, &sink, &cancel).await {
                Ok(response) => StepOutcome::Responded(Box::new(response)),
                // The Cancel handler already concluded the turn; reporting
                // would be answered by the fence anyway.
                Err(StepError::Cancelled) => return,
                Err(StepError::Provider(e)) => StepOutcome::LlmFailed(e),
            };
            let _ = self_ref
                .tell(AgentCommand::Run(RunCommand::StepDone(Box::new(
                    StepReport { turn, outcome },
                ))))
                .await;
        });
    }

    /// Spawn a compaction step. `manual` carries `/compact`'s instructions;
    /// `None` is the automatic budget check firing.
    fn spawn_compact(&self, folded: &AgentState, manual: Option<Option<String>>, cx: &Cx<'_>) {
        let Some(flight) = &self.flight else { return };
        let Some(tctx) = flight.ctxs.clone() else {
            return;
        };
        let is_manual = manual.is_some();
        let instructions = manual.flatten();
        let carried_state = crate::agent_loop::carried_state::render_carried_state(folded);
        let retain = tctx.budget.map_or(0, |b| b.retain_tokens());
        let tokens_before = flight.context_tokens;
        let history = repair_unanswered_tool_calls(folded.prompt_messages());
        let turn = flight.id;
        let cancel = flight.cancel.clone();
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let outcome = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                outcome = compact_step(
                    &tctx, history, retain, tokens_before, carried_state,
                    instructions, is_manual,
                ) => outcome,
            };
            let _ = self_ref
                .tell(AgentCommand::Run(RunCommand::StepDone(Box::new(
                    StepReport { turn, outcome },
                ))))
                .await;
        });
    }

    /// Spawn the summarisation sub sessions are waiting on.
    fn spawn_summarise(&self, folded: &AgentState, sub_sessions: Vec<Uuid>, cx: &Cx<'_>) {
        let Some(flight) = &self.flight else { return };
        let Some(tctx) = flight.ctxs.clone() else {
            return;
        };
        let history = repair_unanswered_tool_calls(folded.prompt_messages());
        let turn = flight.id;
        let cancel = flight.cancel.clone();
        let self_ref = cx.actor.self_ref();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                result = horsie_agentcore::summarise_span(
                    &tctx.provider,
                    &tctx.conversation_id,
                    &history,
                    history.len(),
                    None,
                    None,
                ) => result.map_err(|e| e.to_string()),
            };
            if let Err(e) = &result {
                tracing::warn!(error = %e, "summarising a session for a sub session failed");
            }
            let _ = self_ref
                .tell(AgentCommand::Run(RunCommand::StepDone(Box::new(
                    StepReport {
                        turn,
                        outcome: StepOutcome::Summarised {
                            sub_sessions,
                            result,
                        },
                    },
                ))))
                .await;
        });
    }

    /// One spawned step reported back: fold it into the turn and decide what
    /// happens next.
    async fn handle_step(
        &mut self,
        report: StepReport,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent> {
        // The generation fence: a report from a superseded turn says nothing
        // about the one in flight now.
        if self.fenced(report.turn).is_none() {
            tracing::warn!(
                turn = report.turn,
                "dropping the report of a superseded step"
            );
            return CommandEffect::none();
        }
        match report.outcome {
            StepOutcome::Prepared(tctx) => {
                let Some(flight) = self.flight.as_mut() else {
                    return CommandEffect::none();
                };
                flight.ctxs = Some(Arc::new(*tctx));
                match flight.summarise.clone() {
                    Some(Summarise::SubSession(subs)) => {
                        self.spawn_summarise(cx.state, subs, cx);
                        CommandEffect::none()
                    }
                    Some(Summarise::Compact(instructions)) => {
                        self.spawn_compact(cx.state, Some(instructions), cx);
                        CommandEffect::none()
                    }
                    None => {
                        let state = cx.state.clone();
                        self.dispatch_model_step(Vec::new(), &state, cx, None).await
                    }
                }
            }
            StepOutcome::ProvideFailed(error) => {
                // Reported exactly as the old path did — `terminal` above all,
                // which tells the session its sandbox is gone for good.
                let Some(run_id) = self.flight_id() else {
                    return CommandEffect::none();
                };
                cx.runtime
                    .parent
                    .deliver(crate::agent_loop::context::AgentOutcome::Failed {
                        agent: cx.runtime.journal_id,
                        error: error.message,
                        recoverable: true,
                        terminal: error.terminal,
                    })
                    .await;
                let state = cx.state.clone();
                self.conclude(
                    RunReport {
                        run_id,
                        outcome: RunOutcome::AlreadyReported,
                        seed_summary: None,
                    },
                    &state,
                    cx,
                )
                .await
            }
            StepOutcome::Summarised {
                sub_sessions,
                result,
            } => {
                // Delivered before the turn's own outcome, and unconditionally:
                // the sub sessions waiting are a different session's business.
                cx.runtime
                    .parent
                    .deliver(crate::agent_loop::context::AgentOutcome::SeedSummary {
                        agent: cx.runtime.journal_id,
                        sub_sessions,
                        result,
                    })
                    .await;
                let Some(flight) = self.flight.as_ref() else {
                    return CommandEffect::none();
                };
                let (run_id, summarise_only) = (flight.id, flight.summarise_only);
                let state = cx.state.clone();
                match summarise_only {
                    true => {
                        self.finish(
                            Vec::new(),
                            RunReport {
                                run_id,
                                outcome: RunOutcome::Completed {
                                    text: String::new(),
                                },
                                seed_summary: None,
                            },
                            &state,
                            cx,
                        )
                        .await
                    }
                    false => self.dispatch_model_step(Vec::new(), &state, cx, None).await,
                }
            }
            StepOutcome::Compacted(data) => {
                let events = vec![AgentDomainEvent::Compacted {
                    summary: data.summary,
                    carried_state: data.carried_state,
                    retained_from_message_id: data.retained_from_message_id,
                    trigger: data.trigger,
                    instructions: data.instructions,
                    tokens_before: data.tokens_before,
                    tokens_after: data.tokens_after,
                    at_ms: now_ms(),
                }];
                let folded = Components::apply_all(cx.state, &events);
                let Some(flight) = self.flight.as_mut() else {
                    return CommandEffect::none();
                };
                // What the next auto-compaction check reads; the fold updated
                // the durable copy the same way.
                flight.context_tokens = data.tokens_after;
                let (run_id, summarise_only) = (flight.id, flight.summarise_only);
                match summarise_only {
                    true => {
                        self.finish(
                            events,
                            RunReport {
                                run_id,
                                outcome: RunOutcome::Completed {
                                    text: String::new(),
                                },
                                seed_summary: None,
                            },
                            &folded,
                            cx,
                        )
                        .await
                    }
                    false => {
                        // Straight to the call, not back through the budget
                        // check: a compaction that left the prompt over the
                        // trigger must not loop.
                        self.spawn_llm(&folded, cx, None);
                        CommandEffect::persist(events)
                    }
                }
            }
            StepOutcome::CompactSkipped { notice } => {
                let Some(flight) = self.flight.as_ref() else {
                    return CommandEffect::none();
                };
                let mut events = Vec::new();
                if notice {
                    events.push(AgentDomainEvent::LifecycleRecorded {
                        event: horsie_agentcore::LifecycleEvent::CompactionSkipped(
                            horsie_models::agent::CompactionSkippedLifecycle {
                                context_tokens: flight.context_tokens,
                                retain_tokens: flight
                                    .ctxs
                                    .as_ref()
                                    .and_then(|c| c.budget.map(|b| b.retain_tokens())),
                            },
                        ),
                        at_ms: now_ms(),
                    });
                }
                let folded = Components::apply_all(cx.state, &events);
                let (run_id, summarise_only) = (flight.id, flight.summarise_only);
                match summarise_only {
                    true => {
                        self.finish(
                            events,
                            RunReport {
                                run_id,
                                outcome: RunOutcome::Completed {
                                    text: String::new(),
                                },
                                seed_summary: None,
                            },
                            &folded,
                            cx,
                        )
                        .await
                    }
                    false => {
                        self.spawn_llm(&folded, cx, None);
                        CommandEffect::persist(events)
                    }
                }
            }
            StepOutcome::Responded(response) => self.handle_responded(*response, cx).await,
            StepOutcome::LlmFailed(error) => self.handle_llm_failed(error, cx).await,
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
                        seed_summary: None,
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
            // the model try again. Journaled now — the fold is the only source
            // of history — where the old loop kept them in run-task locals.
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
                seed_summary: None,
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
                self.start(cx, summarise, summarise_only);
                CommandEffect::none()
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
                                seed_summary: None,
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
            RunCommand::StepDone(report) => self.handle_step(*report, cx).await,
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
    // The fallthrough is unreachable by construction: `component::fold` routes
    // every variant to exactly one component, so an event added later fails to
    // compile *there* — where it should be classified — rather than silently
    // reaching the wrong fold here.
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
        // history now, so an input the model must see has to be in it.
        let message = AgentInput::user_message(new_message_id(), "continue the interrupted task")
            .to_message(now_ms());
        let (ack, _) = tokio::sync::oneshot::channel();
        cx.tell(AgentCommand::Run(RunCommand::PersistProgress {
            events: vec![AgentDomainEvent::InputMessage { message }],
            ack: ReplyTo::from_sender(ack),
        }))
        .await;
        self.start(cx, None, false);
    }
}

/// The body of a compact step, on its own task.
async fn compact_step(
    tctx: &TurnCtx,
    history: Vec<Message>,
    retain_tokens: u32,
    tokens_before: u32,
    carried_state: String,
    instructions: Option<String>,
    manual: bool,
) -> StepOutcome {
    use horsie_models::agent::{CompactionTrigger, EmptyOutcome};
    let cut = horsie_agentcore::choose_cut(&history, retain_tokens);
    if cut == 0 {
        // Nothing would be folded away. A typed `/compact` deserves to hear
        // that; the automatic check declining is routine and stays silent.
        return StepOutcome::CompactSkipped { notice: manual };
    }
    let trigger_name = if manual { "manual" } else { "auto" };
    let records = tctx
        .context_provider
        .compaction_hooks(horsie_models::runtime::ServerHookEvent::PreCompact(
            horsie_models::runtime::PreCompactInput {
                trigger: trigger_name.to_string(),
                instructions: instructions.clone(),
            },
        ))
        .await;
    if let Some(reason) = crate::agent_loop::carried_state::precompact_refusal(&records) {
        tracing::info!(reason, "a PreCompact hook abandoned this compaction");
        return StepOutcome::CompactSkipped { notice: false };
    }
    let summary = match horsie_agentcore::summarise_span(
        &tctx.provider,
        &tctx.conversation_id,
        &history,
        cut,
        instructions.as_deref(),
        None,
    )
    .await
    {
        Ok(summary) => summary,
        Err(e) => {
            tracing::warn!(error = %e, "a compaction failed; the turn continues uncompacted");
            return StepOutcome::CompactSkipped { notice: false };
        }
    };
    let retained_from_message_id = history.get(cut).map(|m| m.id.clone());
    let boundary = Message {
        id: format!("compaction:{}", history.len()),
        role: horsie_models::agent::Role::User,
        parts: vec![horsie_models::agent::ContentPart::Text(
            horsie_models::agent::TextPart {
                text: horsie_agentcore::boundary_text(&summary, &carried_state),
            },
        )],
        created_at_ms: now_ms(),
        started_at_ms: None,
    };
    let mut rewritten = vec![boundary];
    rewritten.extend_from_slice(&history[cut..]);
    let tokens_after = horsie_agentcore::approx_history_tokens(&rewritten);
    // Fire-and-forget: the boundary is about to exist, and nothing a
    // `PostCompact` hook says could change it.
    let _ = tctx
        .context_provider
        .compaction_hooks(horsie_models::runtime::ServerHookEvent::PostCompact(
            horsie_models::runtime::PostCompactInput {
                trigger: trigger_name.to_string(),
                tokens_before,
                tokens_after,
            },
        ))
        .await;
    StepOutcome::Compacted(Box::new(CompactedData {
        summary,
        carried_state,
        retained_from_message_id,
        trigger: match manual {
            true => CompactionTrigger::Manual(EmptyOutcome {}),
            false => CompactionTrigger::Auto(EmptyOutcome {}),
        },
        instructions,
        tokens_before,
        tokens_after,
    }))
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
