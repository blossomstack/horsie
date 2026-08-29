//! The turn in flight: starting one, what it writes, and stopping it.
//!
//! Everything expensive happens on a spawned task, never on the mailbox.
//! Rehydrating the runtime, reconnecting MCP, scanning the workspace and
//! running the start hooks all cross a process boundary, and doing them here
//! would mean a stalled peer wedged the run exactly where `Stop` could not
//! reach it. So the task holds the cancel token, and the mailbox holds only the
//! handle.
//!
//! The run reports back through [`RunCommand::RunFinished`]; what that report
//! *means* is [`super::conclude`]'s job.

use super::*;
use crate::agent_loop::context::AgentOutcome;
use crate::agent_loop::inbox::Summarise;
use async_trait::async_trait;
use horsie_actor::{ActorContext, CommandEffect, ReplyTo};
use horsie_agentcore::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentLogBody, AgentResult, EventSink,
    LlmProvider, Message, StoppedCall, Toolbox,
};
use horsie_models::now_ms;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Result of a background run, sent back to the actor as
/// [`AgentCommand::RunFinished`]. Coarse events are streamed separately and
/// incrementally via [`AgentCommand::PersistProgress`]; this carries only the
/// terminal outcome.
pub struct RunReport {
    /// Which run this is the report of. A cancelled run is still unwinding when
    /// the next one may already have started, and a report that arrives after
    /// its run was superseded must be dropped rather than clearing the *new*
    /// run's handle and delivering the old run's outcome as if it were its own.
    pub(super) run_id: u64,
    pub(super) outcome: RunOutcome,
    /// A summary this run was asked to take for sub sessions waiting on it,
    /// and how that went.
    ///
    /// Beside the outcome rather than inside it because the two are
    /// independent: a turn that summarises for a sub session can still go on
    /// to answer a message queued alongside it, exactly as a queued `/compact`
    /// does. `None` means nothing asked.
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

/// The in-flight run: its identity and the token that cancels it.
pub(super) struct RunHandle {
    pub(super) id: u64,
    pub(super) cancel: CancellationToken,
}

#[derive(Debug)]
pub(super) enum RunOutcome {
    /// Agent ended its turn with plain text. Whether that is a park or a
    /// mistake is decided by the actor, which alone knows what would wake it.
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
    /// Context preparation failed and the outcome was already delivered to the
    /// parent on the run task; the actor only needs to clear its `running`
    /// flag.
    AlreadyReported,
}

impl AgentActor {
    pub(super) fn start_run(
        &mut self,
        input: AgentInput,
        ctx: &ActorContext<AgentCommand>,
        history: Vec<Message>,
        // The prompt size the previous turn left behind, from durable state.
        context_tokens: u32,
        // A summary this turn was asked for, and what becomes of it.
        summarise: Option<Summarise>,
        // Whether that summary is all this turn is.
        summarise_only: bool,
    ) {
        let cancel = CancellationToken::new();
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.running = Some(RunHandle {
            id: run_id,
            cancel: cancel.clone(),
        });

        let self_ref = ctx.self_ref();
        let context_provider = self.ctx.context_provider.clone();
        // Cloned here, used inside the spawned task: resolving bytes is
        // database I/O, and this actor's mailbox must never block on it.
        let artifact_source = self.ctx.artifacts.clone();
        let configured_prompt = self.params.system_prompt.clone();
        // Normally `None`, meaning `Auto`: a turn may end with text, and which
        // tools end a run is the toolbox's business. Set only when this turn is
        // re-running one that ended without the result it owed.
        let tool_choice = self.pending_tool_choice.take();
        let max_iterations = self.params.max_iterations;
        let run_def_tools = self.params.tools.clone();
        let thinking_effort = self.params.thinking_effort;
        let max_retries = self.params.max_retries;
        let parent = self.ctx.parent.clone();
        let agent = self.ctx.journal_id;
        // The same value, named for the other job it does. `journal_id` is this
        // agent's own identity, and only a *main* agent's identity is a session
        // id — a subagent or a workflow step carries its own uuid. Each has its
        // own history, and so its own cacheable prefix, which is exactly the
        // granularity a provider grouping requests by session wants.
        let conversation_id = agent.to_string();

        tokio::spawn(async move {
            // Provide this run's contexts on the spawned task (never the
            // mailbox): rehydrate the runtime, reconnect MCP, scan the
            // workspace. A failure here is a recoverable run failure -- report
            // it and stop, exactly as a provider/tool error would.
            //
            // Cancellable, because this is the *most* likely place to hang: it
            // awaits an MCP connect, a workspace scan and a SessionStart hook,
            // all of which cross a process boundary. Leaving it outside the
            // cancel path meant a stalled peer wedged the run exactly where
            // `Stop` could not reach it — `halt()` gave up after its timeout
            // and the task leaked for the process lifetime (#61 item 5b).
            let provided = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                let _ = self_ref
                    .tell(AgentCommand::Run(RunCommand::RunFinished(Box::new(RunReport {
                        run_id,
                        outcome: RunOutcome::Cancelled,
                        seed_summary: None}))))
                    .await;
                return;
            }
            provided = context_provider.provide() => provided};
            let contexts = match provided {
                Ok(c) => c,
                Err(error) => {
                    parent
                        .deliver(AgentOutcome::Failed {
                            agent,
                            error: error.message,
                            recoverable: true,
                            terminal: error.terminal,
                        })
                        .await;
                    let _ = self_ref
                        .tell(AgentCommand::Run(RunCommand::RunFinished(Box::new(
                            RunReport {
                                run_id,
                                outcome: RunOutcome::AlreadyReported,
                                seed_summary: None,
                            },
                        ))))
                        .await;
                    return;
                }
            };
            // The timer and `task_list` layers are stacked unconditionally and
            // narrowed afterwards by the selection, like every other layer.
            // Both execute by `ask`ing this actor and are never sent to the
            // sandboxed runtime.
            let tool_narrowing = contexts.tool_narrowing;
            let toolbox: Arc<dyn Toolbox> = Arc::new(TimerToolbox {
                inner: contexts.toolbox,
                actor: self_ref.clone(),
            });
            let toolbox: Arc<dyn Toolbox> = Arc::new(TaskListToolbox {
                inner: toolbox,
                actor: self_ref.clone(),
            });
            // The agent's tool selection, applied once and last, so it reaches
            // every layer above — the runtime tools, the timers, `task_list`,
            // the session's own. Applied here rather than deeper because here
            // is the only place the stack is whole; narrowing at any inner
            // layer is how a selection came to mean "runtime tools, and
            // nothing else you might reasonably have meant".
            //
            // `None` is not a bypass: it resolves to the default set, which
            // leaves the control plane out. See `crate::tools`.
            let toolbox =
                crate::agent_loop::FilteredToolbox::apply(toolbox, run_def_tools.as_deref());
            // A plugin's agent definition may narrow further. Stacked rather
            // than merged: two filters can only ever remove, so whichever list
            // is the narrower wins without anyone having to compute which.
            let toolbox = match &tool_narrowing {
                None => toolbox,
                Some(narrowed) => {
                    crate::agent_loop::FilteredToolbox::apply(toolbox, Some(narrowed))
                }
            };
            let system_prompt = contexts
                .system_prompt
                .or(configured_prompt)
                .unwrap_or_default();
            // The sink persists each coarse event by `ask`ing this actor and
            // awaiting the durable write, so the LLM loop has end-to-end
            // backpressure: `emit().await` does not return until the event is
            // journaled. Persistence still flows through the actor's single
            // mailbox (`PersistProgress`), never the journal directly.
            let sink: Arc<dyn EventSink> = Arc::new(PersistSink {
                actor: self_ref.clone(),
            });
            // Auto-compaction is on unless the context layer withheld a
            // window — which it does both when the session turned it off and
            // when the model's card declares none.
            let compaction =
                contexts
                    .context_window
                    .map(|context_window| horsie_agentcore::CompactionBudget {
                        context_window,
                        trigger_at_percent: COMPACT_AT_PERCENT,
                        retain_percent: COMPACT_RETAIN_PERCENT,
                    });
            let (outcome, seed_summary) = run_with_retries(
                contexts.provider,
                toolbox,
                sink,
                conversation_id,
                system_prompt,
                tool_choice,
                max_iterations,
                max_retries,
                thinking_effort,
                history,
                input,
                cancel,
                compaction,
                Arc::new(
                    crate::agent_loop::carried_state::ActorCompactionPolicy::new(
                        self_ref.clone(),
                        context_provider.clone(),
                    ),
                ),
                context_tokens,
                summarise,
                summarise_only,
                artifact_source,
            )
            .await;
            // All coarse events were already persisted (each `emit` awaited
            // its ack), so `RunFinished` lands after them in mailbox order.
            let _ = self_ref
                .tell(AgentCommand::Run(RunCommand::RunFinished(Box::new(
                    RunReport {
                        run_id,
                        outcome,
                        seed_summary,
                    },
                ))))
                .await;
        });
    }

    /// The message a cancelled run was part-way through writing, if it had
    /// written anything worth keeping.
    ///
    /// Reads the deltas, which are the only copy: a streamed message becomes
    /// durable when the provider finishes it, and a cancelled call never
    /// reaches that point. Whitespace alone is not an answer, so it is not
    /// worth an entry.
    pub(super) fn aborted_message(&self) -> Option<Message> {
        let text = self.deltas.concat();
        (!text.trim().is_empty()).then(|| Message::assistant_text(new_message_id(), text, now_ms()))
    }
}

/// The turn in flight: stopping it, and what it writes and reports.
pub(super) struct Run;

impl Run {
    pub(super) async fn handle(
        actor: &mut AgentActor,
        state: &AgentState,
        cmd: RunCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            RunCommand::Cancel { ack } => {
                match (&actor.running, ack) {
                    (Some(run), ack) => {
                        run.cancel.cancel();
                        // Answered when the run reports back, not now: the
                        // point of the ack is "the run is over", and it is
                        // still winding down.
                        actor.cancel_acks.extend(ack);
                    }
                    // Nothing in flight (idle, or paused on a pending ask): the
                    // caller's guarantee already holds.
                    (None, Some(ack)) => {
                        let _ = ack.send(());
                    }
                    (None, None) => {}
                }
                CommandEffect::none()
            }
            RunCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
            RunCommand::RunFinished(report) => actor.handle_finished(*report, state, ctx).await,
        }
    }
}

#[async_trait]
impl Component for Run {
    /// Repair the tool call the dead process was running, report the turn it
    /// died inside, and — for an agent nobody will message — re-drive it.
    async fn on_load(actor: &mut AgentActor, state: &AgentState, ctx: &ActorContext<AgentCommand>) {
        // A tool call the dead process was running has no result and never
        // will. Record the repair once, here, where it still belongs at the
        // end of the transcript — recomputing it per turn instead is what let
        // it drift into the middle of a history nobody could then repair in
        // place.
        let repairs = missing_tool_results(&state.prompt_messages(), &parked_call_ids(state));
        if !repairs.is_empty() {
            let (ack, _) = tokio::sync::oneshot::channel();
            let ack = ReplyTo::from_sender(ack);
            let _ = ctx
                .self_ref()
                .tell(AgentCommand::Run(RunCommand::PersistProgress {
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
        // anything queued while the actor was loading — including a message
        // that starts a real turn. An owner therefore never has to work out
        // which turn the report is about, which is exactly the question its own
        // status could not answer.
        //
        // Nothing is journaled to clear the flag. It would have to be self-sent
        // and would land *behind* that queued message, clearing the flag over a
        // turn that had since begun — so the next crash would go undetected. It
        // stays set until a turn reaches a boundary under its own power, and a
        // second load before then simply reports again, which the owner reads
        // against a status that has already moved on.
        if state.turn_in_flight {
            actor
                .ctx
                .parent
                .deliver(AgentOutcome::Interrupted {
                    agent: actor.ctx.journal_id,
                })
                .await;
        }
        // Interactive sessions never self-continue: the user's next message is
        // the continuation. An empty history means nothing ran yet, and a
        // parked agent is waiting for a timer — neither is an interrupted
        // turn.
        if actor.params.interactive || state.parked || state.log.is_empty() {
            return;
        }
        // Deliberately not persisted as a new turn boundary: if the process
        // dies again before making progress, recovery simply re-synthesizes
        // it.
        let history = repair_unanswered_tool_calls(state.prompt_messages());
        actor.start_run(
            AgentInput::user_message(new_message_id(), "continue the interrupted task"),
            ctx,
            history,
            state.context_tokens,
            None,
            false,
        );
    }

    /// What a run wrote, and what it cost.
    // The fallthrough is unreachable by construction: `AgentActor::apply_event`
    // routes every variant to exactly one module, so an event added later fails
    // to compile *there* — where it should be classified — rather than silently
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
                at_ms,
            } => state.push(
                at_ms,
                AgentLogBody::Llm(Message::tool_result(tool_call_id, output, is_error, at_ms)),
            ),
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
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_with_retries(
    provider: Arc<dyn LlmProvider>,
    toolbox: Arc<dyn Toolbox>,
    sink: Arc<dyn EventSink>,
    conversation_id: String,
    system_prompt: String,
    tool_choice: Option<horsie_agentcore::ToolChoice>,
    max_iterations: Option<u32>,
    max_retries: u32,
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    history: Vec<Message>,
    input: AgentInput,
    cancel: CancellationToken,
    compaction: Option<horsie_agentcore::CompactionBudget>,
    compaction_policy: Arc<dyn horsie_agentcore::CompactionPolicy>,
    context_tokens: u32,
    summarise: Option<Summarise>,
    summarise_only: bool,
    // Where the agent resolves artifact bytes. `None` shows the model none.
    artifact_source: Option<Arc<dyn horsie_agentcore::ArtifactSource>>,
) -> (RunOutcome, Option<SeedSummary>) {
    // Whatever a sub session is waiting on is taken first, before this turn
    // can say anything to the model: the summary has to describe the history
    // the branch marker was written into, not one this turn went on to extend.
    let (compact, seed_summary) = match summarise {
        Some(Summarise::Compact(instructions)) => (Some(instructions), None),
        Some(Summarise::SubSession(sub_sessions)) => {
            let result = summarise_for_sub_sessions(
                &provider,
                &toolbox,
                &conversation_id,
                &history,
                thinking_effort,
            )
            .await;
            if let Err(e) = &result {
                tracing::warn!(error = %e, "summarising a session for a sub session failed");
            }
            (
                None,
                Some(SeedSummary {
                    sub_sessions,
                    result,
                }),
            )
        }
        None => (None, None),
    };
    // A turn whose whole job was that summary is over: there is nothing to
    // send. The compaction case cannot short-circuit here, because it needs
    // the agent the loop below builds.
    if summarise_only && compact.is_none() {
        return (
            RunOutcome::Completed {
                text: String::new(),
            },
            seed_summary,
        );
    }
    (
        run_turn_attempts(
            provider,
            toolbox,
            sink,
            conversation_id,
            system_prompt,
            tool_choice,
            max_iterations,
            max_retries,
            thinking_effort,
            history,
            input,
            cancel,
            compaction,
            compaction_policy,
            context_tokens,
            compact,
            summarise_only,
            artifact_source,
        )
        .await,
        seed_summary,
    )
}

/// Summarise a session for the sub sessions branching off it.
///
/// A throwaway `Agent` over the same provider and history: the summary is a
/// *reading* of this session for somebody else, so nothing is journaled,
/// nothing is streamed, and this agent's own history is left exactly as it was.
pub(super) async fn summarise_for_sub_sessions(
    provider: &Arc<dyn LlmProvider>,
    toolbox: &Arc<dyn Toolbox>,
    conversation_id: &str,
    history: &[Message],
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
) -> Result<String, String> {
    let agent = Agent::builder(provider.clone(), toolbox.clone(), conversation_id)
        .with_config(AgentConfig {
            thinking_effort,
            ..AgentConfig::default()
        })
        .with_history(history.to_vec())
        .build()
        .map_err(|e| e.to_string())?;
    agent.summarise_all(None).await.map_err(|e| e.to_string())
}

/// The retry loop proper: everything a turn does once its summarisation, if it
/// had one, has been dealt with.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_turn_attempts(
    provider: Arc<dyn LlmProvider>,
    toolbox: Arc<dyn Toolbox>,
    sink: Arc<dyn EventSink>,
    conversation_id: String,
    system_prompt: String,
    tool_choice: Option<horsie_agentcore::ToolChoice>,
    max_iterations: Option<u32>,
    max_retries: u32,
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    history: Vec<Message>,
    input: AgentInput,
    cancel: CancellationToken,
    compaction: Option<horsie_agentcore::CompactionBudget>,
    compaction_policy: Arc<dyn horsie_agentcore::CompactionPolicy>,
    context_tokens: u32,
    compact: Option<Option<String>>,
    compact_only: bool,
    // Where the agent resolves artifact bytes. `None` shows the model none.
    artifact_source: Option<Arc<dyn horsie_agentcore::ArtifactSource>>,
) -> RunOutcome {
    let mut attempt: u32 = 0;
    loop {
        // CapturingSink wraps the PersistSink: it records events only to
        // locate the handoff tool-call id; persistence (with backpressure)
        // happens in PersistSink.
        let capture = CapturingSink::new(sink.clone());
        let config = AgentConfig {
            max_iterations: max_iterations.unwrap_or_else(|| AgentConfig::default().max_iterations),
            thinking_effort,
            compaction,
            ..AgentConfig::default()
        };
        let mut builder = Agent::builder(provider.clone(), toolbox.clone(), &conversation_id)
            .with_system_prompt(system_prompt.clone())
            .with_config(config)
            .with_history(history.clone())
            .with_compaction(compaction_policy.clone())
            .with_context_tokens(context_tokens);
        if let Some(source) = artifact_source.clone() {
            builder = builder.artifact_source(source);
        }
        if let Some(choice) = tool_choice.clone() {
            builder = builder.with_tool_choice(choice);
        }

        let mut agent = match builder.build() {
            Ok(a) => a,
            Err(e) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
                };
            }
        };

        // A queued `/compact` runs before anything this turn says to the model,
        // and when it is the whole turn, instead of it. Not retried: a
        // compaction that failed leaves the history it started with, which is
        // exactly what the turn would have run on anyway.
        if let Some(instructions) = compact.clone() {
            if let Err(e) = agent.compact_only(instructions, &capture).await {
                tracing::warn!(error = %e, "a requested compaction failed");
            }
            if compact_only {
                return RunOutcome::Completed {
                    text: String::new(),
                };
            }
        }

        let result = agent.run(input.clone(), &capture, cancel.clone()).await;
        let captured = capture.take();

        match result {
            Ok(output) => {
                return match output.result {
                    AgentResult::Completed(c) => RunOutcome::Completed { text: c.text },
                    AgentResult::Stopped(s) => RunOutcome::Stopped { calls: s.calls },
                };
            }
            Err(AgentError::Cancelled) => return RunOutcome::Cancelled,
            Err(AgentError::Provider(e)) => {
                // Whether the failed attempt already wrote something durable.
                // `PersistSink` journals exactly the events `coarse_event`
                // maps, so this is the same test it applied — no proxy, no
                // guessing. `RunAborted` is the exception: it is written *by*
                // this failure rather than by anything the attempt achieved,
                // so counting it would make every transient error look like
                // partial progress and no attempt would ever be retried.
                let journaled = captured.iter().any(|ev| {
                    !matches!(ev, AgentEvent::RunAborted(_)) && coarse_event(ev).is_some()
                });
                // Three independent conditions, all required:
                //
                // 1. Budget remains.
                // 2. The failure is transient. `LlmError` already distinguishes
                //    RateLimit / Overloaded / Network from a permanent ApiError,
                //    and this layer used to discard all of it — retrying a 401 or
                //    a 400 context-length error exactly as eagerly as a 429.
                // 3. Nothing durable was written. The retry rebuilds the turn from
                //    the ORIGINAL `history`, which does not contain the events the
                //    failed attempt persisted, so retrying after partial progress
                //    leaves a phantom turn in the transcript that the model never
                //    saw — replayed into every later turn (#61 item 21). This is
                //    the same "only retry when nothing has been emitted" rule the
                //    providers already apply to their own streams.
                if attempt < max_retries && e.is_transient() && !journaled {
                    attempt += 1;
                    // Honour a provider-supplied delay when there is one; the
                    // exponential backoff is the fallback, not the rule.
                    let delay = e
                        .retry_after()
                        .unwrap_or_else(|| Duration::from_millis(50u64 * (1u64 << attempt.min(6))));
                    tracing::warn!(
                        error = %e,
                        attempt,
                        delay_ms = delay.as_millis(),
                        "transient provider error with nothing journaled; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
                if journaled && e.is_transient() && attempt < max_retries {
                    tracing::warn!(
                        error = %e,
                        "not retrying: the attempt already journaled progress that a \
                         restart from the original history would duplicate"
                    );
                }
                return RunOutcome::Failed {
                    // Report the classification rather than assuming
                    // recoverable: a permanent failure shown as transient
                    // invites the user to retry something that can never
                    // succeed.
                    recoverable: e.is_transient(),
                    error: e.to_string(),
                };
            }
            Err(e) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: false,
                };
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use crate::agent_loop::agent_actor::testing::*;
    use crate::agent_loop::context::{AgentOutcome, AgentOutcomeSink};
    /// The one seam the session id can regress at silently. Everything
    /// downstream is typed — the field is required, so a provider cannot be
    /// handed a request without one — but *which* id `start_run` reads is a
    /// plain assignment, and getting it wrong (a fresh uuid, the run id) costs
    /// only a colder prompt cache. Nothing fails, so nothing would catch it.
    #[tokio::test]
    async fn a_run_tells_the_provider_the_agent_s_own_id() {
        use horsie_actor::{ActorSystem, InMemoryJournal, Journal};
        use horsie_agentcore::EmptyToolbox;
        use horsie_agentcore::testkit::MockProvider;

        struct MockContext(Arc<MockProvider>);
        #[async_trait]
        impl crate::agent_loop::ContextProvider for MockContext {
            async fn provide(
                &self,
            ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError> {
                Ok(crate::agent_loop::Contexts {
                    provider: self.0.clone(),
                    toolbox: Arc::new(EmptyToolbox),
                    tool_narrowing: None,
                    system_prompt: None,
                    context_window: None,
                })
            }
        }
        /// Forwards outcomes so the test awaits the run's end rather than
        /// sleeping on it.
        struct ReportingParent(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
        #[async_trait]
        impl AgentOutcomeSink for ReportingParent {
            async fn deliver(&self, outcome: AgentOutcome) {
                let _ = self.0.send(outcome);
            }
        }

        let provider = MockProvider::text("done");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // The agent's own identity: a session id for a main agent, its own uuid
        // for a subagent or a workflow step. Distinct from every other id in
        // scope, so a test that passes cannot be reading the wrong one.
        let session_id = uuid::Uuid::new_v4();
        let ctx = AgentRuntimeContext {
            artifacts: None,
            context_provider: Arc::new(MockContext(provider.clone())),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(ReportingParent(tx)),
            journal_id: session_id,
            ready: true,
        };
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
        );

        agent
            .tell(AgentCommand::Queue(QueueCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m1".into(),
                    text: "hi".into(),
                    artifacts: Vec::new(),
                },
                ack: None,
            }))
            .await
            .unwrap();

        // `Started` precedes the work and `UsageRecorded` rides alongside the
        // terminal outcome, so read past both until the run itself reports.
        loop {
            match rx.recv().await.expect("the run must report an outcome") {
                AgentOutcome::Started { .. }
                | AgentOutcome::UsageRecorded { .. }
                | AgentOutcome::SeedSummary { .. } => continue,
                AgentOutcome::Concluded { .. } => break,
                other => panic!("expected the turn to conclude, got {other:?}"),
            }
        }

        let ids: Vec<String> = provider
            .requests()
            .into_iter()
            .map(|r| r.conversation_id)
            .collect();
        assert_eq!(
            ids,
            vec![session_id.to_string()],
            "the provider must be told this agent's own id, not any other"
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod retry_tests {
    use super::*;
    use crate::agent_loop::agent_actor::testing::*;
    use horsie_agentcore::EventSinkError;
    use horsie_agentcore::testkit::{
        CollectingEventSink, FailingEventSink, MockProvider, MockToolbox, Script,
    };
    use horsie_agentcore::{AgentInput, ContentPart, ToolOutcome};
    use horsie_agentcore::{CompletionResponse, EmptyToolbox, LlmError, StopReason, ToolSpec};
    use horsie_models::agent::{TextPart, ToolCallPart, Usage};

    fn text_response(text: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
            stop_reason: StopReason::EndTurn,
            usage: Usage::without_cache(1, 1),
        }
    }

    fn tool_response(id: &str, name: &str) -> CompletionResponse {
        CompletionResponse {
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({}),
            })],
            stop_reason: StopReason::ToolUse,
            usage: Usage::without_cache(1, 1),
        }
    }

    fn echo_toolbox() -> Arc<MockToolbox> {
        MockToolbox::new(
            vec![ToolSpec {
                name: "echo".into(),
                description: "echo".into(),
                input_schema: serde_json::json!({ "type": "object" }),
            }],
            Arc::new(|_, input| Ok(ToolOutcome::result(input))),
        )
    }

    async fn run(
        provider: Arc<MockProvider>,
        toolbox: Arc<dyn Toolbox>,
        max_retries: u32,
    ) -> (RunOutcome, usize) {
        let sink: Arc<dyn EventSink> = Arc::new(CollectingEventSink::new());
        let outcome = run_turn_attempts(
            provider.clone(),
            toolbox,
            sink,
            "test-conversation".to_string(),
            "sys".into(),
            None,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
            // Retry behaviour is what these exercise; compaction is off so a
            // budget can never change how many calls a retry makes.
            None,
            Arc::new(NeverCompacts),
            0,
            None,
            false,
            None,
        )
        .await;
        let calls = provider.calls();
        (outcome, calls)
    }

    #[tokio::test]
    async fn a_transient_error_is_retried_when_nothing_was_journaled() {
        let provider = MockProvider::scripted(Script::of([
            Err(LlmError::Overloaded),
            Ok(text_response("second time lucky")),
        ]));
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 1).await;

        assert!(
            matches!(outcome, RunOutcome::Completed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(calls, 2, "the transient failure should have been retried");
    }

    /// The accounting event a failed attempt writes must not be mistaken for
    /// progress. It is written *by* the failure, so counting it as "something
    /// durable was written" would suppress every retry there is.
    #[tokio::test]
    async fn a_runs_own_accounting_does_not_count_as_journaled_progress() {
        let provider = MockProvider::scripted(Script::of([
            Err(LlmError::Overloaded),
            Err(LlmError::Overloaded),
            Ok(text_response("third time lucky")),
        ]));
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 2).await;
        assert!(
            matches!(outcome, RunOutcome::Completed { .. }),
            "got {outcome:?}"
        );
        assert_eq!(calls, 3, "both transient failures should have been retried");
    }

    #[tokio::test]
    async fn a_permanent_error_is_not_retried() {
        // #61 item 21: every AgentError::Provider used to be retried identically,
        // so a 401 or a 400 context-length error burned the whole retry budget.
        let provider = MockProvider::failing(LlmError::ApiError {
            status: 401,
            message: "bad key".into(),
        });
        let (outcome, calls) = run(provider, Arc::new(EmptyToolbox), 3).await;

        assert_eq!(calls, 1, "a permanent error must not be retried");
        match outcome {
            RunOutcome::Failed { recoverable, .. } => assert!(
                !recoverable,
                "a 401 must not be reported to the user as recoverable"
            ),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    async fn run_with_sink(
        provider: Arc<MockProvider>,
        sink: Arc<dyn EventSink>,
        max_retries: u32,
    ) -> (RunOutcome, usize) {
        let outcome = run_turn_attempts(
            provider.clone(),
            Arc::new(EmptyToolbox),
            sink,
            "test-conversation".to_string(),
            "sys".into(),
            None,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
            // Retry behaviour is what these exercise; compaction is off so a
            // budget can never change how many calls a retry makes.
            None,
            Arc::new(NeverCompacts),
            0,
            None,
            false,
            None,
        )
        .await;
        let calls = provider.calls();
        (outcome, calls)
    }

    /// #61 item 22, half one: the failure raised *inside* `complete()`.
    ///
    /// A journal write failure surfacing through the provider arrives as
    /// `LlmError::EventSink` → `AgentError::Provider`, which this layer used to
    /// retry against the LLM — burning tokens on a disk fault.
    #[tokio::test]
    async fn a_sink_failure_from_the_provider_is_not_retried_against_the_llm() {
        let provider = MockProvider::scripted(Script::of([]).then_repeating_with(|| {
            Err(LlmError::EventSink(EventSinkError(
                "journal write failed: disk full".into(),
            )))
        }));
        let sink: Arc<dyn EventSink> = Arc::new(CollectingEventSink::new());
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(
            calls, 1,
            "a journal failure must not be retried against the LLM"
        );
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(!recoverable, "a disk failure is not a recoverable turn");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    /// #61 item 22, half two: the same root cause raised by the agent loop's own
    /// `events.emit(...)?`, which becomes `AgentError::EventSink`.
    ///
    /// The issue's complaint was that one root cause got two different verdicts
    /// depending on where it surfaced. Both paths must agree, and neither may
    /// retry against the LLM.
    #[tokio::test]
    async fn a_sink_failure_at_turn_start_costs_no_tokens() {
        // `Agent::run` journals the input message before it ever calls the
        // provider, so a journal that is already down fails the turn for free.
        let provider = MockProvider::text("hello");
        let sink: Arc<dyn EventSink> = Arc::new(FailingEventSink::always("journal write failed"));
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(calls, 0, "the provider must never be reached");
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(!recoverable, "a disk failure is not a recoverable turn");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_sink_failure_mid_turn_is_not_retried_and_agrees_with_the_provider_path() {
        // Let the input message and the message-start through, so the provider
        // is genuinely engaged before the journal dies — the realistic shape.
        let provider = MockProvider::text("hello");
        let sink: Arc<dyn EventSink> = Arc::new(FailingEventSink::after(2, "journal write failed"));
        let (outcome, calls) = run_with_sink(provider, sink, 3).await;

        assert_eq!(
            calls, 1,
            "the turn must not be re-run against the LLM after a journal failure"
        );
        match outcome {
            RunOutcome::Failed { recoverable, .. } => {
                assert!(
                    !recoverable,
                    "both sink-failure paths must report the same verdict"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_transient_error_after_journaled_progress_is_not_retried() {
        // The crux of #61 item 21: the retry rebuilds the turn from the
        // ORIGINAL history, which does not contain the events the failed
        // attempt already persisted. Retrying here would leave a phantom turn
        // in the durable transcript that the model never saw, replayed into
        // every later turn.
        let provider = MockProvider::scripted(Script::of([
            Ok(tool_response("call-1", "echo")),
            Err(LlmError::Overloaded),
            Ok(text_response("must never be reached")),
        ]));
        let (outcome, calls) = run(provider, echo_toolbox(), 3).await;

        assert_eq!(
            calls, 2,
            "once a tool result is journaled the turn must not restart from a \
             history that omits it"
        );
        assert!(
            matches!(outcome, RunOutcome::Failed { .. }),
            "got {outcome:?}"
        );
    }
}
