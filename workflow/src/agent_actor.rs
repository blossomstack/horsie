use crate::context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext, CONCLUDE_TOOL,
};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_agentcore::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentResult, ContentPart, EventSink,
    EventSinkError, LlmProvider, Message, Role, Toolbox, Usage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Per-agent configuration distilled from an [`AgentRunDef`]. Runtime only.
#[derive(Clone)]
pub struct AgentParams {
    pub system_prompt: Option<String>,
    /// Whether the agent produces structured output via `conclude`.
    pub has_output_schema: bool,
    /// Whether the agent may pause to ask the user.
    pub allow_ask_user: bool,
    /// Whether the agent may arm timers and park itself to await them.
    pub allow_timers: bool,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
    /// Interactive (session) mode: recovery never injects a synthetic continue —
    /// the next user message is the continuation — and the event log is never
    /// snapshot-compacted (SSE cursors are journal sequence numbers and must
    /// stay stable). Workflow agents keep the default `false`.
    pub interactive: bool,
    /// An optional, never-forced handoff tool name, set by callers with their
    /// own terminal tool that isn't the workflow `conclude` mechanism above
    /// (e.g. the server crate's `ask_user` tool for interactive sessions). When
    /// set, this takes over from `handoff_tool()`/forced `conclude`: `tool_choice`
    /// stays `auto`, plain text is a perfectly normal reply, and a voluntary call
    /// to this tool is still recognized as a handoff. `None` for workflow agents.
    pub optional_handoff_tool: Option<String>,
}

impl AgentParams {
    pub fn from_def(def: &AgentRunDef) -> Self {
        Self {
            system_prompt: def.system_prompt.clone(),
            has_output_schema: def.output_schema.is_some(),
            allow_ask_user: def.allow_ask_user,
            allow_timers: def.allow_timers.unwrap_or(false),
            max_iterations: def.max_iterations,
            max_retries: def.max_retries.unwrap_or(0),
            interactive: false,
            optional_handoff_tool: None,
        }
    }

    /// Whether a pause (ask/park/cancel) may snapshot-compact the journal.
    /// Interactive sessions never compact: their journal sequence numbers are
    /// the SSE cursor space and must stay stable across reconnects.
    fn compact_on_pause(&self) -> bool {
        !self.interactive
    }

    /// The agent's handoff tool — the synthesized `conclude` tool when it has an
    /// output schema, may ask, or may park on timers, else `None` (plain text end).
    fn handoff_tool(&self) -> Option<String> {
        if self.has_output_schema || self.allow_ask_user || self.allow_timers {
            Some(CONCLUDE_TOOL.to_string())
        } else {
            None
        }
    }
}

/// Commands accepted by an [`AgentActor`].
pub enum AgentCommand {
    /// Begin a turn with fresh user input.
    Run { input: String },
    /// Resume a paused agent, supplying the user's reply as the pending tool result.
    InjectToolResult {
        tool_call_id: String,
        content: String,
    },
    /// Cancel an in-flight run. `ack`, if given, fires once the run has actually
    /// terminated — immediately when none is in flight — so a caller that must
    /// know this incarnation will write nothing more (e.g. a session about to
    /// spawn a replacement agent on the same journal) can wait for it rather
    /// than racing it.
    Cancel {
        ack: Option<tokio::sync::oneshot::Sender<()>>,
    },
    /// Internal: coarse events captured mid-run. `ack` lets the emitting loop await
    /// the durable write before continuing, so persistence applies backpressure on
    /// the agent loop, and reports the write outcome so a journal failure aborts the
    /// run instead of proceeding on an unrecorded history. Persistence still flows
    /// through this one mailbox.
    PersistProgress {
        events: Vec<AgentDomainEvent>,
        ack: tokio::sync::oneshot::Sender<Result<(), horsie_actor::JournalError>>,
    },
    /// Internal: a background run finished. Boxed to keep the command enum small.
    RunFinished(Box<RunReport>),
    /// Arm a timer; replies with the new timer id once recorded.
    ArmTimer {
        label: String,
        message: String,
        kind: crate::timers::TimerKind,
        after_secs: u64,
        reply: tokio::sync::oneshot::Sender<crate::timers::TimerId>,
    },
    /// List active timers.
    ListTimers {
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerView>>,
    },
    /// Cancel one or all timers; replies with the ids actually removed.
    CancelTimer {
        selector: crate::timers::CancelSelector,
        reply: tokio::sync::oneshot::Sender<Vec<crate::timers::TimerId>>,
    },
    /// Internal: a timer's sleep elapsed.
    TimerFired { id: crate::timers::TimerId },
    /// Apply a `task_list` mutation (or just render `list`); durable like
    /// timers. Replies with the rendered list, or an error message if the
    /// action was rejected (unknown id, out-of-range position, ...).
    TaskListOp {
        action: crate::task_list::TaskListAction,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    /// Read a window of conversation history from in-memory state — no journal
    /// access, no run. Answers the server's paginated `/history` reads so a long
    /// session can be viewed without replaying its whole transcript.
    GetHistory {
        query: HistoryQuery,
        reply: tokio::sync::oneshot::Sender<AgentHistoryPage>,
    },
    /// Read this agent's own usage + context-size snapshot — no messages or
    /// tasks, cheaper than `GetHistory` when only the numbers are needed.
    /// Backs the session-level usage aggregation.
    GetUsage {
        reply: tokio::sync::oneshot::Sender<AgentUsageSnapshot>,
    },
}

/// A windowed history request over the agent's message log.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    /// Return the `limit` messages immediately *before* this message id. `None`
    /// requests the latest (tail) window — the initial view.
    pub before: Option<String>,
    /// Maximum messages to return.
    pub limit: usize,
}

/// One page of conversation history. `tasks`/`usage` ride only on the tail
/// window (`before = None`): the initial view seeds the task widget and usage
/// readout, while scroll-back pages carry messages alone.
#[derive(Debug, Clone)]
pub struct AgentHistoryPage {
    pub messages: Vec<Message>,
    /// Whether older messages exist before the returned window.
    pub has_more: bool,
    pub tasks: Option<Vec<crate::task_list::TaskRecord>>,
    pub usage: Option<UsageTotal>,
}

/// Coarse events that alter persisted agent state. Streaming observation events
/// (text/tool-input deltas) are emitted to the event sink but never journaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentDomainEvent {
    InputMessage {
        message: Message,
    },
    MessageComplete {
        message: Message,
    },
    ToolComplete {
        tool_call_id: String,
        output: String,
        is_error: bool,
    },
    /// One provider call's own usage, journaled as soon as that call returns.
    /// A run's cost lands incrementally rather than all at once, so an
    /// in-flight tool loop's spend is already durable and a run that never
    /// reaches `RunComplete` still accounts for what it burned.
    UsageDelta {
        usage: Usage,
        /// This call's prompt size alone.
        context_tokens: u32,
    },
    RunComplete {
        usage: Usage,
        iterations: u32,
        /// The last provider call's prompt size alone (not summed across
        /// iterations like `usage`) — what's actually in context now.
        context_tokens: u32,
    },
    RunCancelled,
    /// A timer was armed.
    TimerArmed {
        record: crate::timers::TimerRecord,
    },
    /// One or more timers were cancelled.
    TimerCancelled {
        ids: Vec<crate::timers::TimerId>,
    },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time for a
    /// recurring timer (so the fold stays pure); `None` removes a one-shot.
    TimerFired {
        id: crate::timers::TimerId,
        next_fire_at_unix_ms: Option<u64>,
    },
    /// The agent parked itself awaiting its timers.
    Parked,
    /// The task list changed (create/insert/update_status). Carries the full
    /// resulting state, not a delta — mirrors `MessageComplete`/`ToolComplete`,
    /// so replay never needs to re-derive or re-validate a past mutation.
    TaskListChanged {
        snapshot: crate::task_list::TaskListState,
    },
}

/// The conversation history reconstructed by folding [`AgentDomainEvent`]s, plus
/// any timers the agent has armed and whether it is currently parked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentState {
    pub messages: Vec<Message>,
    /// Active timers — durable so they re-arm on recovery and back `list`/`cancel`.
    #[serde(default)]
    pub timers: Vec<crate::timers::TimerRecord>,
    /// True while the agent has parked itself awaiting a timer (no run in flight).
    #[serde(default)]
    pub parked: bool,
    /// The agent's task list — durable so it survives an actor restart exactly
    /// like timers do; see `crate::task_list`.
    #[serde(default)]
    pub task_list: crate::task_list::TaskListState,
    /// Cumulative token usage across every provider call this agent has ever
    /// made — durable agent state, folded from `UsageDelta` as each call
    /// returns and reconciled by `RunComplete`. `u64` so a long session's
    /// re-sent-context input total can't overflow the per-turn `u32` wire
    /// counters. Answers the session's usage readout without replaying the
    /// whole journal.
    #[serde(default)]
    pub usage_total: UsageTotal,
    /// The current (or most recent) run's own usage — a per-run cost figure,
    /// summed across that run's tool-loop iterations but never across runs.
    /// Grows as the run goes, so a long tool loop's spend is visible before it
    /// ends. `None` before this agent's first provider call.
    #[serde(default)]
    pub last_turn_usage: Option<Usage>,
    /// How much of `usage_total` the current run has provisionally contributed
    /// and not yet had reconciled — the sum of this run's `UsageDelta`s, zeroed
    /// at each run start and again once `RunComplete` swaps it out for the
    /// run's authoritative total. Journals written before `UsageDelta` existed
    /// carry no deltas at all, so this stays zero throughout and `RunComplete`
    /// folds them to exactly the same totals as it always did.
    #[serde(default)]
    pub current_run_usage: UsageTotal,
    /// The most recently completed run's *last* provider call's prompt size
    /// alone (never summed) — what's actually loaded in this agent's context
    /// right now.
    #[serde(default)]
    pub context_tokens: u32,
}

/// Running token totals held in [`AgentState`]. Distinct from the per-turn wire
/// [`Usage`] (`u32`): this accumulates across all turns, so it is `u64` and owns
/// a `Default`, which the fluorite-generated `Usage` does not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotal {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
}

impl UsageTotal {
    fn add(&mut self, usage: &Usage) {
        self.input_tokens = self
            .input_tokens
            .saturating_add(u64::from(usage.input_tokens));
        self.output_tokens = self
            .output_tokens
            .saturating_add(u64::from(usage.output_tokens));
        self.cache_creation_tokens =
            add_optional(self.cache_creation_tokens, usage.cache_creation_tokens);
        self.cache_read_tokens = add_optional(self.cache_read_tokens, usage.cache_read_tokens);
    }

    /// Removes a previously-added sub-total — used to swap a run's provisional
    /// per-call sum for the authoritative figure `RunComplete` reports.
    fn subtract(&mut self, other: &UsageTotal) {
        self.input_tokens = self.input_tokens.saturating_sub(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_sub(other.output_tokens);
        self.cache_creation_tokens =
            sub_optional(self.cache_creation_tokens, other.cache_creation_tokens);
        self.cache_read_tokens = sub_optional(self.cache_read_tokens, other.cache_read_tokens);
    }

    /// This total as a per-run wire [`Usage`]. Saturates rather than wrapping:
    /// a single run overflowing `u32` is not a thing worth crashing over.
    fn to_usage(self) -> Usage {
        Usage {
            input_tokens: u32::try_from(self.input_tokens).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(self.output_tokens).unwrap_or(u32::MAX),
            cache_creation_tokens: self
                .cache_creation_tokens
                .map(|v| u32::try_from(v).unwrap_or(u32::MAX)),
            cache_read_tokens: self
                .cache_read_tokens
                .map(|v| u32::try_from(v).unwrap_or(u32::MAX)),
        }
    }

    /// The larger of two views of the *same* agent's cumulative usage, field by
    /// field — not a sum (see [`combine`](Self::combine) for that). Both a live
    /// agent's own fold and the copy pushed to its parent count the same tokens
    /// from zero, so whichever is ahead is simply the more current one, and
    /// taking the max means neither a lagging push nor a lost agent journal can
    /// walk a total backwards.
    pub fn at_least(&self, other: &UsageTotal) -> UsageTotal {
        UsageTotal {
            input_tokens: self.input_tokens.max(other.input_tokens),
            output_tokens: self.output_tokens.max(other.output_tokens),
            cache_creation_tokens: max_optional(
                self.cache_creation_tokens,
                other.cache_creation_tokens,
            ),
            cache_read_tokens: max_optional(self.cache_read_tokens, other.cache_read_tokens),
        }
    }

    /// Combines two agents' cumulative totals into a session-level aggregate.
    /// Only ever sums usage — never a context-size figure, which stays
    /// meaningfully per-agent (see `AgentUsageSnapshot::context_tokens`).
    pub fn combine(&self, other: &UsageTotal) -> UsageTotal {
        UsageTotal {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_creation_tokens: combine_optional(
                self.cache_creation_tokens,
                other.cache_creation_tokens,
            ),
            cache_read_tokens: combine_optional(self.cache_read_tokens, other.cache_read_tokens),
        }
    }
}

/// Sums an accumulating `u64` cache total with a per-turn `u32` delta. Stays
/// `None` only when neither side has ever reported cache data.
fn add_optional(total: Option<u64>, delta: Option<u32>) -> Option<u64> {
    match (total, delta) {
        (None, None) => None,
        (total, delta) => Some(
            total
                .unwrap_or(0)
                .saturating_add(u64::from(delta.unwrap_or(0))),
        ),
    }
}

/// Removes a sub-total's cache figure from a running one. A total that has
/// never seen cache data stays `None` — there is nothing to take away from it.
fn sub_optional(total: Option<u64>, part: Option<u64>) -> Option<u64> {
    match (total, part) {
        (None, _) => None,
        (Some(t), part) => Some(t.saturating_sub(part.unwrap_or(0))),
    }
}

/// The larger of two views of one agent's cache total. Stays `None` only when
/// neither view has ever seen cache data.
fn max_optional(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).max(b.unwrap_or(0))),
    }
}

/// Sums two agents' `u64` cache totals. Stays `None` only when neither agent
/// has ever reported cache data.
fn combine_optional(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

/// One agent's own usage + context-size snapshot, with no message/task
/// payload — cheaper than [`AgentHistoryPage`] when only the numbers are
/// needed. Backs the session-level usage aggregation.
#[derive(Debug, Clone, Default)]
pub struct AgentUsageSnapshot {
    pub usage_total: UsageTotal,
    pub last_turn_usage: Option<Usage>,
    pub context_tokens: u32,
}

impl AgentState {
    /// Answer a windowed [`HistoryQuery`] from the in-memory message log. The
    /// window is `[start, end)` of `messages`; `end` is just before `before`'s
    /// message (or the log end for the tail), `start` is `limit` back from
    /// `end`. `has_more` reports whether anything precedes `start`. Task list and
    /// usage ride only on the tail window.
    pub fn history_page(&self, query: &HistoryQuery) -> AgentHistoryPage {
        let end = match &query.before {
            None => self.messages.len(),
            Some(id) => self
                .messages
                .iter()
                .position(|m| &m.id == id)
                .unwrap_or(self.messages.len()),
        };
        let start = end.saturating_sub(query.limit);
        let is_tail = query.before.is_none();
        AgentHistoryPage {
            messages: self.messages[start..end].to_vec(),
            has_more: start > 0,
            tasks: is_tail.then(|| self.task_list.tasks().to_vec()),
            usage: is_tail.then_some(self.usage_total),
        }
    }

    /// This agent's own usage + context-size snapshot — always the full,
    /// current picture (unlike `history_page`, there is no tail/scroll-back
    /// distinction here).
    pub fn usage_snapshot(&self) -> AgentUsageSnapshot {
        AgentUsageSnapshot {
            usage_total: self.usage_total,
            last_turn_usage: self.last_turn_usage.clone(),
            context_tokens: self.context_tokens,
        }
    }
}

/// Result of a background run, sent back to the actor as [`AgentCommand::RunFinished`].
/// Coarse events are streamed separately and incrementally via
/// [`AgentCommand::PersistProgress`]; this carries only the terminal outcome.
pub struct RunReport {
    outcome: RunOutcome,
}

enum RunOutcome {
    /// Agent ended its turn with plain text (no `conclude` tool registered).
    Completed {
        text: String,
    },
    /// Agent called the `conclude` tool; `data` is its raw input.
    Concluded {
        data: Value,
        tool_call_id: Option<String>,
    },
    Cancelled,
    Failed {
        error: String,
        recoverable: bool,
    },
    /// Context preparation failed and the outcome was already delivered to the
    /// parent on the run task; the actor only needs to clear its `running` flag.
    AlreadyReported,
}

/// An agent run, modelled as an event-sourced actor. Each `Run`/`InjectToolResult`
/// drives a background `horsie_agentcore::Agent` loop; coarse events are journaled
/// incrementally so a crashed session recovers its conversation and continues.
pub struct AgentActor {
    ctx: AgentRuntimeContext,
    params: AgentParams,
    running: Option<CancellationToken>,
    /// A timer fired while a run was in flight; consume it when the run parks.
    pending_wake: bool,
    /// Callers waiting to hear that the in-flight run has terminated (see
    /// [`AgentCommand::Cancel`]). Drained the moment `RunFinished` is handled —
    /// the run task sends that as its very last act, so every journal write it
    /// could make has already happened by then.
    cancel_acks: Vec<tokio::sync::oneshot::Sender<()>>,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        Self {
            ctx,
            params,
            running: None,
            pending_wake: false,
            cancel_acks: Vec::new(),
        }
    }

    /// The journal identity of an agent session: kind `"agent"`, id = the session
    /// UUID. Centralizes the kind so the workflow (e.g. fork) and the actor agree.
    pub fn persistence_id_for(session_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", session_id.to_string())
    }

    fn start_run(&mut self, input: AgentInput, ctx: &ActorContext<Self>, history: Vec<Message>) {
        let cancel = CancellationToken::new();
        self.running = Some(cancel.clone());

        let self_ref = ctx.self_ref();
        let context_provider = self.ctx.context_provider.clone();
        let allow_timers = self.params.allow_timers;
        let inner_sink = self.ctx.event_sink.clone();
        let configured_prompt = self.params.system_prompt.clone();
        // An explicit optional handoff tool (e.g. the server crate's `ask_user`
        // tool for interactive sessions) always wins over the workflow `conclude`
        // mechanism and is never forced.
        let (handoff_tool, force_handoff_choice) = match self.params.optional_handoff_tool.clone() {
            Some(name) => (Some(name), false),
            None => (self.params.handoff_tool(), true),
        };
        let max_iterations = self.params.max_iterations;
        let max_retries = self.params.max_retries;
        let parent = self.ctx.parent.clone();
        let session_id = self.ctx.session_id;

        tokio::spawn(async move {
            // Provide this run's contexts on the spawned task (never the mailbox):
            // rehydrate the runtime, reconnect MCP, scan the workspace. A failure
            // here is a recoverable run failure -- report it and stop, exactly as a
            // provider/tool error would.
            let contexts = match context_provider.provide().await {
                Ok(c) => c,
                Err(error) => {
                    parent
                        .deliver(AgentOutcome::Failed {
                            session_id,
                            error,
                            recoverable: true,
                        })
                        .await;
                    let _ = self_ref
                        .tell(AgentCommand::RunFinished(Box::new(RunReport {
                            outcome: RunOutcome::AlreadyReported,
                        })))
                        .await;
                    return;
                }
            };
            // Timer-capable agents run with the timer control tools layered on; these
            // execute by `ask`ing this actor and are never sent to the sandboxed runtime.
            let toolbox: Arc<dyn Toolbox> = if allow_timers {
                Arc::new(TimerToolbox {
                    inner: contexts.toolbox,
                    actor: self_ref.clone(),
                })
            } else {
                contexts.toolbox
            };
            // `task_list` is always available, like `skill`/`inspect_workspace` --
            // it's a working-memory aid every agent can reach for, not a permission
            // that needs gating per agent.
            let toolbox: Arc<dyn Toolbox> = Arc::new(TaskListToolbox {
                inner: toolbox,
                actor: self_ref.clone(),
            });
            let system_prompt = contexts
                .system_prompt
                .or(configured_prompt)
                .unwrap_or_default();
            // The sink persists each coarse event by `ask`ing this actor and awaiting
            // the durable write, so the LLM loop has end-to-end backpressure:
            // `emit().await` does not return until the event is journaled. Persistence
            // still flows through the actor's single mailbox (`PersistProgress`),
            // never the journal directly.
            let sink: Arc<dyn EventSink> = Arc::new(PersistSink {
                inner: inner_sink,
                actor: self_ref.clone(),
            });
            let outcome = run_with_retries(
                contexts.provider,
                toolbox,
                sink,
                system_prompt,
                handoff_tool,
                force_handoff_choice,
                max_iterations,
                max_retries,
                history,
                input,
                cancel,
            )
            .await;
            // All coarse events were already persisted (each `emit` awaited its ack),
            // so `RunFinished` lands after them in mailbox order.
            let _ = self_ref
                .tell(AgentCommand::RunFinished(Box::new(RunReport { outcome })))
                .await;
        });
    }

    /// Interpret a `conclude` payload (or plain-text completion) and deliver the
    /// outcome to the parent. The conversation events were already persisted
    /// incrementally via [`AgentCommand::PersistProgress`], so this only records the
    /// terminal transition and decides the actor's lifecycle.
    async fn handle_finished(
        &mut self,
        report: RunReport,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        self.running = None;
        // Answered before any parent delivery below: a canceller is likely
        // blocking its own mailbox waiting on this, and those deliveries `tell`
        // into that same mailbox — replying first keeps the two from deadlocking.
        // The run task has already finished (this message is its last act), so
        // "it will write nothing more" is true now.
        for ack in self.cancel_acks.drain(..) {
            let _ = ack.send(());
        }
        let session_id = self.ctx.session_id;
        let parent = self.ctx.parent.clone();

        // Push usage before branching on *how* the run ended: the tokens were
        // spent either way, and a cancelled/failed/parked run must account for
        // them exactly like a concluded one. `usage_total` is cumulative, so the
        // parent overwriting its copy with it is idempotent.
        parent
            .deliver(AgentOutcome::UsageRecorded {
                session_id,
                usage_total: state.usage_total,
            })
            .await;

        match report.outcome {
            RunOutcome::Completed { text } => {
                // No conclude tool: treat the final text as the output.
                parent
                    .deliver(AgentOutcome::Concluded {
                        session_id,
                        output: Value::String(text),
                    })
                    .await;
                CommandEffect::stop()
            }
            RunOutcome::Concluded { data, tool_call_id } => {
                match self.interpret(data, tool_call_id) {
                    Conclusion::Output(output) => {
                        parent
                            .deliver(AgentOutcome::Concluded { session_id, output })
                            .await;
                        CommandEffect::stop()
                    }
                    Conclusion::Ask {
                        tool_call_id,
                        question,
                    } => {
                        parent
                            .deliver(AgentOutcome::Asked {
                                session_id,
                                tool_call_id,
                                question,
                            })
                            .await;
                        // Stay alive — InjectToolResult resumes this same session.
                        // Snapshot to compact the incrementally-persisted log
                        // (never in interactive mode: cursors must stay stable).
                        if self.params.compact_on_pause() {
                            CommandEffect::snapshot()
                        } else {
                            CommandEffect::none()
                        }
                    }
                    Conclusion::Park => self.park_or_resume(state, ctx, session_id, parent).await,
                }
            }
            RunOutcome::Cancelled => {
                // Snapshot to compact the incrementally-persisted log on cancel
                // (never in interactive mode: cursors must stay stable).
                let eff = CommandEffect::persist(vec![AgentDomainEvent::RunCancelled]);
                if self.params.compact_on_pause() {
                    eff.and_snapshot()
                } else {
                    eff
                }
            }
            RunOutcome::Failed { error, recoverable } => {
                parent
                    .deliver(AgentOutcome::Failed {
                        session_id,
                        error,
                        recoverable,
                    })
                    .await;
                // The partial conversation was already journaled incrementally, so the
                // failed session stays inspectable and a recoverable failure can
                // `resume`/`fork` from where it stopped.
                CommandEffect::stop()
            }
            RunOutcome::AlreadyReported => {
                // Context preparation failed before the loop began; the failure was
                // already delivered to the parent. Stop like any failed run so the
                // session can retry on the next message.
                CommandEffect::stop()
            }
        }
    }

    /// Decide whether a handoff payload is a final output, an ask, or a park.
    /// An `optional_handoff_tool` (e.g. the server crate's `ask_user` tool) is
    /// single-purpose — always an ask — so it bypasses `classify_conclusion`'s
    /// `has_output_schema`/`allow_ask_user`-based branching entirely, which
    /// exists only to disambiguate the workflow crate's multi-purpose `conclude`
    /// payload shape.
    fn interpret(&self, data: Value, tool_call_id: Option<String>) -> Conclusion {
        if self.params.optional_handoff_tool.is_some() {
            let question = data
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Conclusion::Ask {
                tool_call_id,
                question,
            };
        }
        classify_conclusion(
            self.params.has_output_schema,
            self.params.allow_ask_user,
            self.params.allow_timers,
            data,
            tool_call_id,
        )
    }

    /// Decide what a `park` conclusion means: an illegal park (no timers fails the
    /// run), an immediate resume (a timer fired during the run), or a real park
    /// (stay alive, status → Parked).
    async fn park_or_resume(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<Self>,
        session_id: uuid::Uuid,
        parent: Arc<dyn AgentOutcomeSink>,
    ) -> CommandEffect<AgentDomainEvent> {
        if state.timers.is_empty() {
            parent
                .deliver(AgentOutcome::Failed {
                    session_id,
                    error: "agent parked with no active timers — nothing would ever wake it"
                        .to_string(),
                    recoverable: false,
                })
                .await;
            return CommandEffect::stop();
        }
        if self.pending_wake {
            // A timer fired mid-run; go straight back to work instead of parking.
            self.pending_wake = false;
            let wake = AgentInput::user_message(
                new_message_id(),
                "A timer fired while you were busy — re-check now.".to_string(),
            );
            let input_event = AgentDomainEvent::InputMessage {
                message: wake.to_message(),
            };
            self.start_run(wake, ctx, state.messages.clone());
            return CommandEffect::persist(vec![input_event]);
        }
        parent.deliver(AgentOutcome::Parked { session_id }).await;
        let eff = CommandEffect::persist(vec![AgentDomainEvent::Parked]);
        if self.params.compact_on_pause() {
            eff.and_snapshot()
        } else {
            eff
        }
    }

    /// A timer's sleep elapsed. Re-arm a recurring timer, then resume the agent with
    /// a wake message — unless a run is already in flight, in which case coalesce the
    /// wake and let the run consume it when it parks.
    async fn handle_timer_fired(
        &mut self,
        id: crate::timers::TimerId,
        state: &AgentState,
        ctx: &ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Some(record) = state.timers.iter().find(|t| t.id == id).cloned() else {
            // Cancelled or already removed — a stale sleep. Ignore.
            return CommandEffect::none();
        };
        let display_count = record.fire_count + 1;
        let now = crate::timers::now_unix_ms();
        // Re-arm recurring; remove one-shot.
        let next_fire_at_unix_ms = match record.kind {
            crate::timers::TimerKind::Recurring => {
                let next = now.saturating_add(record.interval_secs.saturating_mul(1000));
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(record.interval_secs),
                );
                Some(next)
            }
            crate::timers::TimerKind::OneShot => None,
        };
        let fired = AgentDomainEvent::TimerFired {
            id,
            next_fire_at_unix_ms,
        };

        if self.running.is_some() {
            // A run is in flight: record the fire (re-arm) and remember to wake when
            // the run parks. Multiple fires coalesce into one wake.
            self.pending_wake = true;
            return CommandEffect::persist(vec![fired]);
        }

        // Idle/parked: start a fresh run with the wake message.
        let wake = AgentInput::user_message(new_message_id(), record.wake_message(display_count));
        let input_event = AgentDomainEvent::InputMessage {
            message: wake.to_message(),
        };
        self.start_run(wake, ctx, state.messages.clone());
        CommandEffect::persist(vec![fired, input_event])
    }
}

/// Classify a `conclude` payload into the agent's terminal intent. With timers the
/// payload is always `kind`-tagged (`submit`/`park`/`ask`); without, it follows the
/// legacy (has_output, allow_ask) shape.
fn classify_conclusion(
    has_output_schema: bool,
    allow_ask_user: bool,
    allow_timers: bool,
    data: Value,
    tool_call_id: Option<String>,
) -> Conclusion {
    let extract_question = |d: &Value| {
        d.get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    if allow_timers {
        let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
        return match kind {
            "park" => Conclusion::Park,
            "ask" => Conclusion::Ask {
                tool_call_id,
                question: extract_question(&data),
            },
            _ => Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null)),
        };
    }
    match (has_output_schema, allow_ask_user) {
        // Kind-tagged union.
        (true, true) => {
            let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
            if kind == "ask" {
                Conclusion::Ask {
                    tool_call_id,
                    question: extract_question(&data),
                }
            } else {
                Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null))
            }
        }
        // Output only: the payload is the output.
        (true, false) => Conclusion::Output(data),
        // Ask only: the payload is a question.
        (false, true) => Conclusion::Ask {
            tool_call_id,
            question: extract_question(&data),
        },
        // No conclude tool registered — shouldn't be reached via a handoff.
        (false, false) => Conclusion::Output(data),
    }
}

#[derive(Debug)]
enum Conclusion {
    Output(Value),
    Ask {
        tool_call_id: Option<String>,
        question: String,
    },
    Park,
}

#[async_trait]
impl EventSourcedActor for AgentActor {
    type Command = AgentCommand;
    type Event = AgentDomainEvent;
    type State = AgentState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.ctx.session_id)
    }

    fn initial_state() -> AgentState {
        AgentState::default()
    }

    fn apply_event(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
        match event {
            AgentDomainEvent::InputMessage { message } => {
                // A new turn began — the agent is no longer parked.
                state.parked = false;
                // Every run starts by persisting its input (`Run`,
                // `InjectToolResult`, a timer wake), so this is the one place
                // that marks a run boundary for per-run usage.
                state.current_run_usage = UsageTotal::default();
                state.messages.push(message);
            }
            AgentDomainEvent::MessageComplete { message } => state.messages.push(message),
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
            } => state
                .messages
                .push(Message::tool_result(tool_call_id, output, is_error)),
            AgentDomainEvent::TimerArmed { record } => state.timers.push(record),
            AgentDomainEvent::TimerCancelled { ids } => {
                state.timers.retain(|t| !ids.contains(&t.id));
            }
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms,
            } => match next_fire_at_unix_ms {
                Some(next) => {
                    if let Some(t) = state.timers.iter_mut().find(|t| t.id == id) {
                        t.fire_at_unix_ms = next;
                        t.fire_count += 1;
                    }
                }
                None => state.timers.retain(|t| t.id != id),
            },
            AgentDomainEvent::Parked => state.parked = true,
            AgentDomainEvent::TaskListChanged { snapshot } => state.task_list = snapshot,
            AgentDomainEvent::UsageDelta {
                usage,
                context_tokens,
            } => {
                state.usage_total.add(&usage);
                state.current_run_usage.add(&usage);
                state.context_tokens = context_tokens;
                state.last_turn_usage = Some(state.current_run_usage.to_usage());
            }
            AgentDomainEvent::RunComplete {
                usage,
                context_tokens,
                ..
            } => {
                // `usage` is the run's authoritative total. Swap out whatever
                // this run's deltas provisionally contributed for it: a no-op
                // when they agree, and on a pre-`UsageDelta` journal (no deltas,
                // so nothing to swap out) this adds the whole run exactly as the
                // old fold did.
                state.usage_total.subtract(&state.current_run_usage);
                state.usage_total.add(&usage);
                // Nothing provisional is outstanding once the run is reconciled.
                // Zero rather than `usage`: a legacy journal can hold two
                // `RunComplete`s with no delta between them, and the second must
                // not subtract the first run's total back out.
                state.current_run_usage = UsageTotal::default();
                state.context_tokens = context_tokens;
                state.last_turn_usage = Some(usage);
            }
            AgentDomainEvent::RunCancelled => {}
        }
        state
    }

    async fn handle_command(
        &mut self,
        state: &AgentState,
        cmd: AgentCommand,
        ctx: &mut ActorContext<Self>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            AgentCommand::Run { input } => {
                let agent_input = AgentInput::user_message(new_message_id(), input);
                // Persist the input message here (not via the streaming sink), so a
                // turn-restarting provider retry that re-emits it can never
                // double-persist it into two consecutive user messages.
                let input_event = AgentDomainEvent::InputMessage {
                    message: agent_input.to_message(),
                };
                // Sanitize on every turn start: a history recovered from a mid-turn
                // crash may carry dangling tool calls (a no-op when well-formed).
                self.start_run(
                    agent_input,
                    ctx,
                    sanitize_for_resume(state.messages.clone()),
                );
                CommandEffect::persist(vec![input_event])
            }
            AgentCommand::InjectToolResult {
                tool_call_id,
                content,
            } => {
                let agent_input = AgentInput::tool_result(tool_call_id, content, false);
                let input_event = AgentDomainEvent::InputMessage {
                    message: agent_input.to_message(),
                };
                self.start_run(
                    agent_input,
                    ctx,
                    sanitize_for_resume(state.messages.clone()),
                );
                CommandEffect::persist(vec![input_event])
            }
            AgentCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
            AgentCommand::Cancel { ack } => {
                match (&self.running, ack) {
                    (Some(token), ack) => {
                        token.cancel();
                        // Answered when the run reports back, not now: the point of
                        // the ack is "the run is over", and it is still winding down.
                        self.cancel_acks.extend(ack);
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
            AgentCommand::ArmTimer {
                label,
                message,
                kind,
                after_secs,
                reply,
            } => {
                let now = crate::timers::now_unix_ms();
                let record = crate::timers::TimerRecord::arm(
                    label,
                    message,
                    kind,
                    std::time::Duration::from_secs(after_secs),
                    now,
                );
                let id = record.id.clone();
                spawn_timer_sleep(
                    ctx.self_ref(),
                    id.clone(),
                    std::time::Duration::from_secs(after_secs),
                );
                let _ = reply.send(id);
                CommandEffect::persist(vec![AgentDomainEvent::TimerArmed { record }])
            }
            AgentCommand::ListTimers { reply } => {
                let now = crate::timers::now_unix_ms();
                let views = state.timers.iter().map(|t| t.view(now)).collect();
                let _ = reply.send(views);
                CommandEffect::none()
            }
            AgentCommand::CancelTimer { selector, reply } => {
                let ids: Vec<crate::timers::TimerId> = match selector {
                    crate::timers::CancelSelector::All => {
                        state.timers.iter().map(|t| t.id.clone()).collect()
                    }
                    crate::timers::CancelSelector::One(id) => {
                        if state.timers.iter().any(|t| t.id == id) {
                            vec![id]
                        } else {
                            vec![]
                        }
                    }
                };
                let _ = reply.send(ids.clone());
                if ids.is_empty() {
                    CommandEffect::none()
                } else {
                    CommandEffect::persist(vec![AgentDomainEvent::TimerCancelled { ids }])
                }
            }
            AgentCommand::TimerFired { id } => self.handle_timer_fired(id, state, ctx).await,
            AgentCommand::RunFinished(report) => self.handle_finished(*report, state, ctx).await,
            AgentCommand::TaskListOp { action, reply } => {
                let mut next = state.task_list.clone();
                match next.apply(action) {
                    Ok(()) => {
                        let text = next.render();
                        let _ = reply.send(Ok(text));
                        CommandEffect::persist(vec![AgentDomainEvent::TaskListChanged {
                            snapshot: next,
                        }])
                    }
                    Err(msg) => {
                        let _ = reply.send(Err(msg));
                        CommandEffect::none()
                    }
                }
            }
            AgentCommand::GetHistory { query, reply } => {
                let _ = reply.send(state.history_page(&query));
                CommandEffect::none()
            }
            AgentCommand::GetUsage { reply } => {
                let _ = reply.send(state.usage_snapshot());
                CommandEffect::none()
            }
        }
    }

    /// After recovery, re-drive an interrupted session. An empty history means
    /// nothing ran yet (the workflow will send `Run`); otherwise the process died
    /// mid-turn, so sanitize any dangling tool calls and re-enter the loop with a
    /// synthetic continuation message. The synthetic input is intentionally not
    /// persisted as a new turn boundary: if we crash again before progress,
    /// recovery simply re-synthesizes it.
    async fn on_recovery_complete(&mut self, state: &AgentState, ctx: &mut ActorContext<Self>) {
        // Re-arm every surviving timer with its remaining delay (fires immediately if
        // already due). Do this whether parked or mid-run, so timers keep firing.
        let now = crate::timers::now_unix_ms();
        for t in &state.timers {
            spawn_timer_sleep(ctx.self_ref(), t.id.clone(), t.remaining(now));
        }
        // Interactive sessions never self-continue: the user's next message is
        // the continuation (the session layer passes sanitized history on Run).
        if self.params.interactive {
            return;
        }
        // A parked agent waits for a timer — do not re-drive a turn.
        if state.parked {
            return;
        }
        if state.messages.is_empty() {
            return;
        }
        let history = sanitize_for_resume(state.messages.clone());
        self.start_run(
            AgentInput::user_message(new_message_id(), "continue the interrupted task"),
            ctx,
            history,
        );
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Spawn a one-shot sleep that tells the actor `TimerFired` after `delay`. The
/// firing is journaled/handled in the actor; a stale fire (timer since cancelled)
/// is ignored there, so an un-cancellable sleep task is harmless.
fn spawn_timer_sleep(
    self_ref: ActorRef<AgentCommand>,
    id: crate::timers::TimerId,
    delay: std::time::Duration,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = self_ref.tell(AgentCommand::TimerFired { id }).await;
    });
}

/// Wraps an agent's toolbox, adding the three timer control tools. They execute by
/// `ask`ing the owning [`AgentActor`] (never forwarded to the sandboxed runtime).
struct TimerToolbox {
    inner: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TimerToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.extend(crate::timers::timer_tool_specs());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
    ) -> Result<Value, horsie_agentcore::ToolCallError> {
        use crate::timers::{CancelSelector, TimerId, TimerKind};
        use horsie_agentcore::ToolCallError;
        match name {
            "set_timer" => {
                let kind = match input.get("kind").and_then(Value::as_str) {
                    Some("one_shot") => TimerKind::OneShot,
                    Some("recurring") => TimerKind::Recurring,
                    _ => {
                        return Err(ToolCallError::InvalidInput(
                            "set_timer.kind must be 'one_shot' or 'recurring'".to_string(),
                        ));
                    }
                };
                let Some(after_secs) = input
                    .get("after_secs")
                    .and_then(Value::as_u64)
                    .filter(|n| *n >= 1)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.after_secs must be an integer >= 1".to_string(),
                    ));
                };
                let label = input
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let Some(message) = input
                    .get("message")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                else {
                    return Err(ToolCallError::InvalidInput(
                        "set_timer.message must be a non-empty string".to_string(),
                    ));
                };
                let id = self
                    .actor
                    .ask(|reply| AgentCommand::ArmTimer {
                        label,
                        message,
                        kind,
                        after_secs,
                        reply,
                    })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                Ok(serde_json::json!({ "timer_id": id.0 }))
            }
            "list_timers" => {
                let views = self
                    .actor
                    .ask(|reply| AgentCommand::ListTimers { reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                serde_json::to_value(views)
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))
            }
            "cancel_timer" => {
                let selector = if input.get("all").and_then(Value::as_bool) == Some(true) {
                    CancelSelector::All
                } else if let Some(id) = input.get("id").and_then(Value::as_str) {
                    CancelSelector::One(TimerId(id.to_string()))
                } else {
                    return Err(ToolCallError::InvalidInput(
                        "cancel_timer requires 'id' or 'all': true".to_string(),
                    ));
                };
                let ids = self
                    .actor
                    .ask(|reply| AgentCommand::CancelTimer { selector, reply })
                    .await
                    .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
                let ids: Vec<String> = ids.into_iter().map(|i| i.0).collect();
                Ok(serde_json::json!({ "cancelled": ids }))
            }
            _ => self.inner.execute(name, input).await,
        }
    }
}

/// Wraps an agent's toolbox, adding the always-available `task_list` tool. It
/// executes by `ask`ing the owning [`AgentActor`] (never forwarded to the
/// sandboxed runtime), so its state is durable -- journaled and replayed
/// exactly like timers (see `crate::task_list`).
struct TaskListToolbox {
    inner: Arc<dyn Toolbox>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl Toolbox for TaskListToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        let mut specs = self.inner.specs();
        specs.push(crate::task_list::task_list_tool_spec());
        specs
    }

    async fn execute(
        &self,
        name: &str,
        input: Value,
    ) -> Result<Value, horsie_agentcore::ToolCallError> {
        use horsie_agentcore::ToolCallError;
        if name != crate::task_list::TASK_LIST_TOOL {
            return self.inner.execute(name, input).await;
        }
        let action = crate::task_list::TaskListAction::from_input(&input)?;
        let result = self
            .actor
            .ask(|reply| AgentCommand::TaskListOp { action, reply })
            .await
            .map_err(|e| ToolCallError::ExecutionFailed(e.to_string()))?;
        result
            .map(Value::String)
            .map_err(ToolCallError::InvalidInput)
    }
}

/// Captures coarse agent events while forwarding every event to the inner sink.
/// Used only inside [`run_with_retries`] to locate the handoff tool-call id;
/// persistence (with backpressure) happens in the inner [`PersistSink`].
struct CapturingSink {
    inner: Arc<dyn EventSink>,
    captured: Mutex<Vec<AgentEvent>>,
}

impl CapturingSink {
    fn new(inner: Arc<dyn EventSink>) -> Self {
        Self {
            inner,
            captured: Mutex::new(Vec::new()),
        }
    }

    fn take(&self) -> Vec<AgentEvent> {
        std::mem::take(&mut self.captured.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[async_trait]
impl EventSink for CapturingSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(event.clone());
        }
        // Propagate the inner sink's outcome so a durability failure aborts the run.
        self.inner.emit(event).await
    }
}

/// Persists each coarse domain event by `ask`ing the agent actor and awaiting the
/// durable write before returning — this is what gives the agent loop end-to-end
/// backpressure. Persistence flows through the actor's mailbox
/// ([`AgentCommand::PersistProgress`]), never the journal directly. Every event is
/// also forwarded to the inner observation sink.
///
/// `InputMessage` is intentionally NOT persisted here: the actor persists the input
/// itself when handling `Run`/`InjectToolResult`, so a turn-restarting retry that
/// re-emits the input can never double-persist it into two consecutive user
/// messages.
struct PersistSink {
    inner: Arc<dyn EventSink>,
    actor: ActorRef<AgentCommand>,
}

#[async_trait]
impl EventSink for PersistSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError> {
        if let Some(coarse) = coarse_event(&event) {
            // Await the durable write and act on its outcome:
            // - Ok(Ok(()))  → journaled; proceed.
            // - Ok(Err(je)) → the journal write FAILED. Abort the run rather than
            //   continue on a history that was never recorded.
            // - Err(_)      → the actor has stopped (the run is being torn down), so
            //   there is nothing to persist to and nothing to wait for; drop quietly.
            match self
                .actor
                .ask(|ack| AgentCommand::PersistProgress {
                    events: vec![coarse],
                    ack,
                })
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(je)) => {
                    return Err(EventSinkError(format!("journal write failed: {je}")));
                }
                Err(_actor_gone) => {}
            }
        }
        self.inner.emit(event).await
    }
}

/// Map a single streaming event to the coarse domain event that should be
/// persisted, or `None` for streaming noise and for `InputMessage` (see
/// [`PersistSink`]).
fn coarse_event(e: &AgentEvent) -> Option<AgentDomainEvent> {
    match e {
        AgentEvent::MessageComplete(ev) => Some(AgentDomainEvent::MessageComplete {
            message: ev.message.clone(),
        }),
        AgentEvent::ToolComplete(ev) => Some(AgentDomainEvent::ToolComplete {
            tool_call_id: ev.tool_call_id.clone(),
            output: ev.output.clone(),
            is_error: ev.is_error,
        }),
        AgentEvent::UsageUpdate(ev) => Some(AgentDomainEvent::UsageDelta {
            usage: ev.usage.clone(),
            context_tokens: ev.context_tokens,
        }),
        AgentEvent::RunComplete(ev) => Some(AgentDomainEvent::RunComplete {
            usage: ev.usage.clone(),
            iterations: ev.iterations,
            context_tokens: ev.context_tokens,
        }),
        AgentEvent::InputMessage(_)
        | AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::TextBlockStart(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingBlockStart(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ThinkingSignatureChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ContentBlockStop(_)
        | AgentEvent::ToolExecuting(_) => None,
    }
}

/// Make a recovered history well-formed for the provider: every `tool_use` in the
/// last assistant message must have a matching `tool_result`. Any missing one (an
/// interrupted tool call) gets a synthetic error result so the model can retry.
fn sanitize_for_resume(mut messages: Vec<Message>) -> Vec<Message> {
    let answered: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
            ContentPart::Text(_) | ContentPart::ToolCall(_) | ContentPart::Thinking(_) => None,
        })
        .collect();
    let dangling: Vec<String> = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.parts
                .iter()
                .filter_map(|p| match p {
                    ContentPart::ToolCall(tc) if !answered.contains(&tc.id) => Some(tc.id.clone()),
                    ContentPart::ToolCall(_)
                    | ContentPart::Text(_)
                    | ContentPart::ToolResult(_)
                    | ContentPart::Thinking(_) => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for id in dangling {
        messages.push(Message::tool_result(
            id,
            "interrupted by shutdown, not completed",
            true,
        ));
    }
    messages
}

/// Find the tool-call id of the handoff tool by scanning captured assistant messages.
fn find_tool_call_id(events: &[AgentEvent], tool_name: &str) -> Option<String> {
    events.iter().rev().find_map(|e| match e {
        AgentEvent::MessageComplete(mc) => mc.message.parts.iter().find_map(|p| match p {
            ContentPart::ToolCall(tc) if tc.name == tool_name => Some(tc.id.clone()),
            ContentPart::ToolCall(_)
            | ContentPart::Text(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Thinking(_) => None,
        }),
        AgentEvent::InputMessage(_)
        | AgentEvent::MessageStart(_)
        | AgentEvent::MessageStop(_)
        | AgentEvent::TextBlockStart(_)
        | AgentEvent::TextChunk(_)
        | AgentEvent::ThinkingBlockStart(_)
        | AgentEvent::ThinkingChunk(_)
        | AgentEvent::ThinkingSignatureChunk(_)
        | AgentEvent::ToolCallStart(_)
        | AgentEvent::ToolCallInputDelta(_)
        | AgentEvent::ContentBlockStop(_)
        | AgentEvent::ToolExecuting(_)
        | AgentEvent::ToolComplete(_)
        | AgentEvent::UsageUpdate(_)
        | AgentEvent::RunComplete(_) => None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_with_retries(
    provider: Arc<dyn LlmProvider>,
    toolbox: Arc<dyn Toolbox>,
    sink: Arc<dyn EventSink>,
    system_prompt: String,
    handoff_tool: Option<String>,
    force_handoff_choice: bool,
    max_iterations: Option<u32>,
    max_retries: u32,
    history: Vec<Message>,
    input: AgentInput,
    cancel: CancellationToken,
) -> RunOutcome {
    let mut attempt: u32 = 0;
    loop {
        // CapturingSink wraps the PersistSink: it records events only to locate the
        // handoff tool-call id; persistence (with backpressure) happens in PersistSink.
        let capture = CapturingSink::new(sink.clone());
        let config = AgentConfig {
            max_iterations: max_iterations.unwrap_or_else(|| AgentConfig::default().max_iterations),
            ..AgentConfig::default()
        };
        let mut builder = Agent::builder(provider.clone(), toolbox.clone())
            .with_system_prompt(system_prompt.clone())
            .with_config(config)
            .with_history(history.clone());
        if let Some(name) = &handoff_tool {
            builder = if force_handoff_choice {
                builder.with_handoff_tool(name.clone())
            } else {
                builder.with_handoff_tool_optional(name.clone())
            };
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

        let result = agent.run(input.clone(), &capture, cancel.clone()).await;
        let captured = capture.take();

        match result {
            Ok(output) => {
                return match output.result {
                    AgentResult::Completed(c) => RunOutcome::Completed { text: c.text },
                    AgentResult::Handoff(h) => {
                        let tool_call_id = find_tool_call_id(&captured, &h.tool_name);
                        RunOutcome::Concluded {
                            data: h.data,
                            tool_call_id,
                        }
                    }
                };
            }
            Err(AgentError::Cancelled) => return RunOutcome::Cancelled,
            Err(AgentError::Provider(e)) if attempt < max_retries => {
                attempt += 1;
                let backoff = Duration::from_millis(50u64 * (1u64 << attempt.min(6)));
                tracing::warn!(error = %e, attempt, "provider error; retrying after backoff");
                tokio::time::sleep(backoff).await;
                continue;
            }
            Err(AgentError::Provider(e)) => {
                return RunOutcome::Failed {
                    error: e.to_string(),
                    recoverable: true,
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
    use horsie_models::agent::{TextPart, ToolCallPart, ToolResultPart};

    fn user_msg(text: &str) -> Message {
        Message {
            id: "u".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
        }
    }

    fn def_fixture() -> AgentRunDef {
        AgentRunDef {
            system_prompt: None,
            output_schema: None,
            allow_ask_user: false,
            allow_timers: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        }
    }

    #[test]
    fn from_def_defaults_to_non_interactive() {
        assert!(!AgentParams::from_def(&def_fixture()).interactive);
    }

    #[test]
    fn from_def_defaults_optional_handoff_tool_to_none() {
        assert!(
            AgentParams::from_def(&def_fixture())
                .optional_handoff_tool
                .is_none()
        );
    }

    #[test]
    fn interactive_pause_does_not_compact() {
        let mut params = AgentParams::from_def(&def_fixture());
        assert!(params.compact_on_pause());
        params.interactive = true;
        assert!(!params.compact_on_pause());
    }

    #[test]
    fn apply_event_rebuilds_history_in_order() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hello"),
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::MessageComplete {
                message: Message {
                    id: "a".into(),
                    role: Role::Assistant,
                    parts: vec![ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "search".into(),
                        input: serde_json::json!({}),
                    })],
                },
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::ToolComplete {
                tool_call_id: "tc1".into(),
                output: "result".into(),
                is_error: false,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(1, 1),
                iterations: 1,
                context_tokens: 1,
            },
        );

        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0].role, Role::User);
        assert_eq!(state.messages[1].role, Role::Assistant);
        assert_eq!(state.messages[2].role, Role::Tool);
        match &state.messages[2].parts[0] {
            ContentPart::ToolResult(ToolResultPart {
                tool_call_id,
                output,
                ..
            }) => {
                assert_eq!(tool_call_id, "tc1");
                assert_eq!(output, "result");
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn run_cancelled_is_noop_on_state() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("hi"),
            },
        );
        let before = state.messages.len();
        state = AgentActor::apply_event(state, AgentDomainEvent::RunCancelled);
        assert_eq!(state.messages.len(), before);
    }

    #[test]
    fn sanitize_appends_error_results_for_dangling_tool_calls() {
        let history = vec![
            user_msg("do it"),
            Message {
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc1".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                    ContentPart::ToolCall(ToolCallPart {
                        id: "tc2".into(),
                        name: "bash".into(),
                        input: serde_json::json!({}),
                    }),
                ],
            },
            Message::tool_result("tc1", "ok", false),
        ];
        let fixed = sanitize_for_resume(history);
        // tc2 was dangling → an error tool_result is appended at the end.
        let last = fixed.last().unwrap();
        match &last.parts[0] {
            ContentPart::ToolResult(r) => {
                assert_eq!(r.tool_call_id, "tc2");
                assert!(r.is_error);
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_leaves_well_formed_history_untouched() {
        let history = vec![
            user_msg("do it"),
            Message {
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "tc1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                })],
            },
            Message::tool_result("tc1", "ok", false),
        ];
        let before = history.len();
        let fixed = sanitize_for_resume(history);
        assert_eq!(fixed.len(), before);
    }

    #[test]
    fn classify_park_kind_when_timers_enabled() {
        use serde_json::json;
        // timers on: a kind=park payload classifies as Park.
        let c = classify_conclusion(true, true, true, json!({"kind": "park"}), None);
        assert!(matches!(c, Conclusion::Park));
        // kind=submit classifies as Output(output field).
        let c = classify_conclusion(
            true,
            true,
            true,
            json!({"kind": "submit", "output": {"x": 1}}),
            None,
        );
        match c {
            Conclusion::Output(v) => assert_eq!(v["x"], 1),
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn timer_events_fold_into_state() {
        use crate::timers::{TimerKind, TimerRecord};
        use std::time::Duration;

        let rec = TimerRecord::arm(
            "pr".into(),
            String::new(),
            TimerKind::Recurring,
            Duration::from_secs(60),
            0,
        );
        let id = rec.id.clone();
        let mut state = AgentActor::initial_state();

        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: rec });
        assert_eq!(state.timers.len(), 1);

        // Recurring fire re-arms in place with a carried next fire time and bumped count.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired {
                id: id.clone(),
                next_fire_at_unix_ms: Some(120_000),
            },
        );
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].fire_count, 1);
        assert_eq!(state.timers[0].fire_at_unix_ms, 120_000);

        // One-shot fire (None) removes it.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms: None,
            },
        );
        assert!(state.timers.is_empty());
    }

    #[test]
    fn task_list_events_fold_into_state() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.task_list.render(), "No tasks.");

        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::task_list::TaskListAction::Create {
                tasks: vec!["a".to_string(), "b".to_string()],
            })
            .unwrap();
        state = AgentActor::apply_event(state, AgentDomainEvent::TaskListChanged { snapshot });
        assert!(state.task_list.render().contains("[ ] 1. a"));

        // A later snapshot replaces the whole state -- folding is a plain
        // assignment, not a merge.
        let mut snapshot = state.task_list.clone();
        snapshot
            .apply(crate::task_list::TaskListAction::UpdateStatus {
                ids: vec![1],
                status: crate::task_list::TaskStatus::Completed,
            })
            .unwrap();
        state = AgentActor::apply_event(state, AgentDomainEvent::TaskListChanged { snapshot });
        assert!(state.task_list.render().contains("Tasks (1/2 done)"));
    }

    fn with_messages(ids: &[&str]) -> AgentState {
        let mut state = AgentActor::initial_state();
        for id in ids {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::MessageComplete {
                    message: Message::user(*id, "x"),
                },
            );
        }
        state
    }

    fn page_ids(page: &AgentHistoryPage) -> Vec<String> {
        page.messages.iter().map(|m| m.id.clone()).collect()
    }

    #[test]
    fn history_tail_returns_last_limit_with_tasks_and_usage() {
        let mut state = with_messages(&["a", "b", "c", "d"]);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(4, 2),
                iterations: 1,
                context_tokens: 4,
            },
        );
        let page = state.history_page(&HistoryQuery {
            before: None,
            limit: 2,
        });
        assert_eq!(page_ids(&page), ["c", "d"]);
        assert!(page.has_more);
        assert!(page.tasks.is_some());
        assert_eq!(page.usage.unwrap().input_tokens, 4);
    }

    #[test]
    fn history_before_cursor_pages_backward_without_tasks() {
        let state = with_messages(&["a", "b", "c", "d"]);
        let page = state.history_page(&HistoryQuery {
            before: Some("c".into()),
            limit: 2,
        });
        // Two messages immediately before "c": "a", "b".
        assert_eq!(page_ids(&page), ["a", "b"]);
        assert!(!page.has_more);
        assert!(page.tasks.is_none());
        assert!(page.usage.is_none());
    }

    #[test]
    fn history_tail_shorter_than_limit_has_no_more() {
        let state = with_messages(&["a", "b"]);
        let page = state.history_page(&HistoryQuery {
            before: None,
            limit: 10,
        });
        assert_eq!(page_ids(&page), ["a", "b"]);
        assert!(!page.has_more);
    }

    /// The pre-`UsageDelta` journal shape: runs that only ever wrote
    /// `RunComplete`. Folding must still total them exactly as it always did,
    /// or every session recorded before this event existed loses its history.
    #[test]
    fn run_complete_accumulates_usage_total() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.usage_total, UsageTotal::default());
        for (input, output) in [(10u32, 5u32), (7, 3)] {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::RunComplete {
                    usage: Usage::without_cache(input, output),
                    iterations: 1,
                    context_tokens: input,
                },
            );
        }
        assert_eq!(state.usage_total.input_tokens, 17);
        assert_eq!(state.usage_total.output_tokens, 8);
    }

    #[test]
    fn run_complete_tracks_last_turn_and_context_tokens_separately_from_total() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.last_turn_usage, None);
        assert_eq!(state.context_tokens, 0);

        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage {
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_creation_tokens: Some(15),
                    cache_read_tokens: None,
                },
                iterations: 2,
                context_tokens: 12,
            },
        );
        // A multi-iteration turn: `usage` is the summed cost, `context_tokens`
        // is only the last call's prompt size — the two must stay distinct.
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 20);
        assert_eq!(state.context_tokens, 12);
        assert_eq!(state.usage_total.cache_creation_tokens, Some(15));
        assert_eq!(state.usage_total.cache_read_tokens, None);

        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage {
                    input_tokens: 30,
                    output_tokens: 8,
                    cache_creation_tokens: None,
                    cache_read_tokens: Some(25),
                },
                iterations: 1,
                context_tokens: 30,
            },
        );
        // `last_turn_usage`/`context_tokens` are overwritten, not accumulated;
        // `usage_total`'s cache fields sum even though only one side reported
        // each field on any given turn.
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 30);
        assert_eq!(state.context_tokens, 30);
        assert_eq!(state.usage_total.input_tokens, 50);
        assert_eq!(state.usage_total.cache_creation_tokens, Some(15));
        assert_eq!(state.usage_total.cache_read_tokens, Some(25));
    }

    fn input_event() -> AgentDomainEvent {
        AgentDomainEvent::InputMessage {
            message: Message::user(new_message_id(), "go"),
        }
    }

    fn usage_delta(input: u32, output: u32) -> AgentDomainEvent {
        AgentDomainEvent::UsageDelta {
            usage: Usage::without_cache(input, output),
            context_tokens: input,
        }
    }

    /// The point of the whole exercise: a run's cost is visible while the run is
    /// still going, instead of banked until it ends.
    #[test]
    fn usage_deltas_accumulate_while_a_run_is_still_going() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, input_event());
        for (input, output) in [(100u32, 10u32), (250, 20), (400, 5)] {
            state = AgentActor::apply_event(state, usage_delta(input, output));
        }
        assert_eq!(state.usage_total.input_tokens, 750);
        assert_eq!(state.usage_total.output_tokens, 35);
        // `last_turn_usage` is the run's cost *so far*, not just the last call's.
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 750);
        // `context_tokens` is the latest call's prompt size alone.
        assert_eq!(state.context_tokens, 400);
    }

    /// `RunComplete` reports the same tokens the deltas already did. Counting
    /// both would double a long tool loop's entire cost.
    #[test]
    fn run_complete_swaps_out_the_provisional_deltas_it_restates() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, input_event());
        state = AgentActor::apply_event(state, usage_delta(100, 10));
        state = AgentActor::apply_event(state, usage_delta(250, 20));
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(350, 30),
                iterations: 2,
                context_tokens: 250,
            },
        );
        assert_eq!(
            state.usage_total.input_tokens, 350,
            "counted once, not twice"
        );
        assert_eq!(state.usage_total.output_tokens, 30);
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 350);

        // A second run starts from zero again rather than re-restating the first.
        state = AgentActor::apply_event(state, input_event());
        state = AgentActor::apply_event(state, usage_delta(40, 4));
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(40, 4),
                iterations: 1,
                context_tokens: 40,
            },
        );
        assert_eq!(state.usage_total.input_tokens, 390);
        assert_eq!(state.last_turn_usage.as_ref().unwrap().input_tokens, 40);
    }

    /// Cancel, provider error, truncation and the iteration cap all end a run
    /// without a `RunComplete`. The tokens were still spent.
    #[test]
    fn a_run_that_never_completes_still_keeps_the_tokens_it_spent() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, input_event());
        state = AgentActor::apply_event(state, usage_delta(500, 40));
        state = AgentActor::apply_event(state, usage_delta(600, 30));
        state = AgentActor::apply_event(state, AgentDomainEvent::RunCancelled);
        assert_eq!(state.usage_total.input_tokens, 1100);

        // The next run adds to that rather than replacing it, and its own
        // `RunComplete` restates only its own deltas.
        state = AgentActor::apply_event(state, input_event());
        state = AgentActor::apply_event(state, usage_delta(50, 5));
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(50, 5),
                iterations: 1,
                context_tokens: 50,
            },
        );
        assert_eq!(state.usage_total.input_tokens, 1150);
        assert_eq!(state.usage_total.output_tokens, 75);
        assert_eq!(
            state.last_turn_usage.as_ref().unwrap().input_tokens,
            50,
            "the cancelled run's spend stays in the total but not in the per-run figure"
        );
    }

    /// A journal that mixes both shapes — old `RunComplete`-only runs recorded
    /// before `UsageDelta` existed, then new ones — folds to the sum of both.
    #[test]
    fn mixed_legacy_and_delta_runs_fold_to_the_same_total() {
        let mut state = AgentActor::initial_state();
        // Legacy run: input + RunComplete, no deltas at all.
        state = AgentActor::apply_event(state, input_event());
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(1000, 100),
                iterations: 3,
                context_tokens: 900,
            },
        );
        assert_eq!(state.usage_total.input_tokens, 1000);
        // New-shape run on the same journal.
        state = AgentActor::apply_event(state, input_event());
        state = AgentActor::apply_event(state, usage_delta(200, 20));
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(200, 20),
                iterations: 1,
                context_tokens: 200,
            },
        );
        assert_eq!(state.usage_total.input_tokens, 1200);
        assert_eq!(state.usage_total.output_tokens, 120);
    }

    #[test]
    fn usage_total_subtract_leaves_never_reported_cache_fields_none() {
        let mut total = UsageTotal {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: Some(10),
            cache_read_tokens: None,
        };
        total.subtract(&UsageTotal {
            input_tokens: 40,
            output_tokens: 20,
            cache_creation_tokens: Some(4),
            cache_read_tokens: Some(7),
        });
        assert_eq!(total.input_tokens, 60);
        assert_eq!(total.output_tokens, 30);
        assert_eq!(total.cache_creation_tokens, Some(6));
        assert_eq!(
            total.cache_read_tokens, None,
            "nothing to take away from a total that never saw cache reads"
        );
    }

    /// `at_least` reconciles two views of *one* agent, so it must never sum
    /// them the way `combine` sums two different agents.
    #[test]
    fn usage_total_at_least_takes_the_further_along_view_field_by_field() {
        let live = UsageTotal {
            input_tokens: 900,
            output_tokens: 10,
            cache_creation_tokens: None,
            cache_read_tokens: Some(4),
        };
        let durable = UsageTotal {
            input_tokens: 800,
            output_tokens: 12,
            cache_creation_tokens: Some(3),
            cache_read_tokens: None,
        };
        let merged = live.at_least(&durable);
        assert_eq!(merged.input_tokens, 900, "not 1700");
        assert_eq!(merged.output_tokens, 12);
        assert_eq!(merged.cache_creation_tokens, Some(3));
        assert_eq!(merged.cache_read_tokens, Some(4));
        assert_eq!(
            UsageTotal::default().at_least(&UsageTotal::default()),
            UsageTotal::default(),
            "two blank views stay blank, cache fields included"
        );
    }

    #[test]
    fn usage_total_combine_sums_two_agents_treating_no_cache_data_as_none() {
        let a = UsageTotal {
            input_tokens: 10,
            output_tokens: 5,
            cache_creation_tokens: Some(3),
            cache_read_tokens: None,
        };
        let b = UsageTotal {
            input_tokens: 20,
            output_tokens: 8,
            cache_creation_tokens: None,
            cache_read_tokens: None,
        };
        let combined = a.combine(&b);
        assert_eq!(combined.input_tokens, 30);
        assert_eq!(combined.output_tokens, 13);
        assert_eq!(combined.cache_creation_tokens, Some(3));
        assert_eq!(
            combined.cache_read_tokens, None,
            "neither agent ever reported cache reads"
        );
    }

    #[test]
    fn park_sets_parked_and_input_clears_it() {
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::Parked);
        assert!(state.parked);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::InputMessage {
                message: user_msg("wake"),
            },
        );
        assert!(!state.parked);
    }

    #[test]
    fn cancel_event_removes_selected_timers() {
        use crate::timers::{TimerKind, TimerRecord};
        use std::time::Duration;
        let a = TimerRecord::arm(
            "a".into(),
            String::new(),
            TimerKind::OneShot,
            Duration::from_secs(1),
            0,
        );
        let b = TimerRecord::arm(
            "b".into(),
            String::new(),
            TimerKind::OneShot,
            Duration::from_secs(1),
            0,
        );
        let (ia, ib) = (a.id.clone(), b.id.clone());
        let mut state = AgentActor::initial_state();
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: a });
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerArmed { record: b });
        state = AgentActor::apply_event(state, AgentDomainEvent::TimerCancelled { ids: vec![ia] });
        assert_eq!(state.timers.len(), 1);
        assert_eq!(state.timers[0].id, ib);
    }

    #[test]
    fn coarse_event_filters_streaming_noise_and_input() {
        use horsie_models::events::{InputMessageEvent, TextChunkEvent};
        // Streaming noise → None.
        assert!(
            coarse_event(&AgentEvent::TextChunk(TextChunkEvent {
                message_id: "m".into(),
                index: 0,
                text: "noise".into(),
            }))
            .is_none()
        );
        // InputMessage is suppressed from the persistence stream (persisted by the
        // actor instead).
        assert!(
            coarse_event(&AgentEvent::InputMessage(InputMessageEvent {
                message_id: "m".into(),
                input: AgentInput::user_message("m", "hi"),
            }))
            .is_none()
        );
    }
}
