use crate::context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext, AskedQuestion, CONCLUDE_TOOL,
};
use async_trait::async_trait;
use horsie_actor::{ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId};
use horsie_agentcore::{
    Agent, AgentConfig, AgentError, AgentEvent, AgentInput, AgentResult, ContentPart, EventSink,
    EventSinkError, HandoffCall, LlmProvider, Message, Role, Toolbox, Usage,
};
use horsie_models::now_ms;
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
    /// Canonical thinking effort for this agent's runs, already resolved from
    /// the session's choice and the model's default. `None` sends no control.
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
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
            thinking_effort: None,
            interactive: false,
            optional_handoff_tool: None,
        }
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

    /// Every tool name a call to which *parks* this agent rather than running.
    ///
    /// A handoff tool is never executed: the run ends on the call and its result
    /// arrives later as an `InjectToolResult` (the user's answer to `ask_user`,
    /// a timer firing). So a dangling call to one is the normal shape of a
    /// parked agent, not the wreckage of an interrupted one — see
    /// [`missing_tool_results`], which must not journal a repair for it.
    fn handoff_tools(&self) -> Vec<String> {
        self.handoff_tool()
            .into_iter()
            .chain(self.optional_handoff_tool.clone())
            .collect()
    }
}

/// Commands accepted by an [`AgentActor`].
pub enum AgentCommand {
    /// Start a turn. `results` are tool results to record first — the answers to
    /// a park, or the results an abandoned park still owes the wire — and
    /// `message` is the user message that starts it.
    ///
    /// With no `message`, the results themselves are the turn's input, which is
    /// how an answered park resumes. With no `results`, it is an ordinary turn.
    /// At least one of the two must be present.
    Resume {
        results: Vec<horsie_models::agent::ToolResultInput>,
        message: Option<String>,
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
    /// Stop this actor. Sent when the session it belongs to unloads: the agent
    /// is resident for the session's *loaded* lifetime, not forever, and going
    /// cold must not leave a task behind holding a whole transcript in memory.
    Shutdown,
    /// Read this agent's own usage + context-size snapshot — no messages or
    /// tasks, cheaper than `GetHistory` when only the numbers are needed.
    /// Backs the session-level usage aggregation.
    GetUsage {
        reply: tokio::sync::oneshot::Sender<AgentUsageSnapshot>,
    },
    /// Read this agent's current values — task list plus usage — for the agent
    /// document. Distinct from `GetHistory`, which returns transcript appends:
    /// these are values a client re-reads rather than accumulates.
    GetState {
        reply: tokio::sync::oneshot::Sender<AgentStateView>,
    },
}

/// A windowed history request over the agent's message log.
///
/// The two cursors are the same space — a message id — read in opposite
/// directions, and they are mutually exclusive: `after` wins if both are set.
/// Because `state.messages` is append-only and never truncated, a cursor stays
/// valid for the life of the session, which is what lets a live stream resume
/// from one without any journal involvement.
#[derive(Debug, Clone, Default)]
pub struct HistoryQuery {
    /// Return the `limit` messages immediately *before* this message id. `None`
    /// requests the latest (tail) window — the initial view.
    pub before: Option<String>,
    /// Return up to `limit` messages immediately *after* this message id — the
    /// forward page a reconnecting stream backfills with.
    pub after: Option<String>,
    /// Maximum messages to return.
    pub limit: usize,
}

/// One page of conversation history — messages alone. Current values (task
/// list, usage) are a different category and live on the agent document, so a
/// page means exactly one thing regardless of which cursor produced it.
#[derive(Debug, Clone)]
pub struct AgentHistoryPage {
    pub messages: Vec<Message>,
    /// Whether older messages exist before the returned window.
    pub has_more_before: bool,
    /// Whether newer messages exist after the returned window — how a forward
    /// backfill learns it must ask for another page.
    pub has_more_after: bool,
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
        /// When the tool finished. Journaled rather than re-read at fold time:
        /// this variant rebuilds its `Message` in `apply_event`, so a recovered
        /// transcript would otherwise stamp every past tool result with the
        /// moment of recovery.
        at_ms: u64,
    },
    RunComplete {
        usage: Usage,
        iterations: u32,
        /// The last provider call's prompt size alone (not summed across
        /// iterations like `usage`) — what's actually in context now.
        context_tokens: u32,
        at_ms: u64,
    },
    RunCancelled {
        at_ms: u64,
    },
    /// A timer was armed.
    TimerArmed {
        record: crate::timers::TimerRecord,
        at_ms: u64,
    },
    /// One or more timers were cancelled.
    TimerCancelled {
        ids: Vec<crate::timers::TimerId>,
        at_ms: u64,
    },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time for a
    /// recurring timer (so the fold stays pure); `None` removes a one-shot.
    TimerFired {
        id: crate::timers::TimerId,
        next_fire_at_unix_ms: Option<u64>,
        at_ms: u64,
    },
    /// The agent parked itself awaiting its timers.
    Parked {
        at_ms: u64,
    },
    /// The task list changed (create/insert/update_status). Carries the full
    /// resulting state, not a delta — mirrors `MessageComplete`/`ToolComplete`,
    /// so replay never needs to re-derive or re-validate a past mutation.
    TaskListChanged {
        snapshot: crate::task_list::TaskListState,
        at_ms: u64,
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
    /// Cumulative token usage across every completed run — durable agent state,
    /// folded from `RunComplete`. `u64` so a long session's re-sent-context input
    /// total can't overflow the per-turn `u32` wire counters. Answers the
    /// session's usage readout without replaying the whole journal.
    #[serde(default)]
    pub usage_total: UsageTotal,
    /// The most recently completed run's own usage — a per-run cost figure,
    /// summed across that run's tool-loop iterations but never across runs.
    /// `None` before this agent's first completed run.
    #[serde(default)]
    pub last_turn_usage: Option<Usage>,
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

/// One agent's current values: the task list and its usage/context numbers.
/// Everything here is a value the client re-reads, never a log it accumulates —
/// which is why none of it rides on a history page.
#[derive(Debug, Clone, Default)]
pub struct AgentStateView {
    pub tasks: Vec<crate::task_list::TaskRecord>,
    pub usage_total: UsageTotal,
    pub last_turn_usage: Option<Usage>,
    pub context_tokens: u32,
}

impl AgentState {
    /// This agent's current values, for the agent document.
    pub fn state_view(&self) -> AgentStateView {
        AgentStateView {
            tasks: self.task_list.tasks().to_vec(),
            usage_total: self.usage_total,
            last_turn_usage: self.last_turn_usage.clone(),
            context_tokens: self.context_tokens,
        }
    }

    /// Answer a windowed [`HistoryQuery`] from the in-memory message log,
    /// returning the half-open window `[start, end)` of `messages`.
    ///
    /// - `after`: forward from just past that message, up to `limit`.
    /// - `before`: the `limit` messages ending just before that message.
    /// - neither: the tail, the last `limit` messages.
    ///
    /// An unresolvable `after` cursor yields an empty window with nothing owed
    /// in either direction, which is the honest answer: the caller asked to
    /// continue from a message this log does not contain, so it must re-seed
    /// from the tail rather than be handed a silently wrong window. An
    /// unresolvable `before` falls back to the tail, preserving the existing
    /// scroll-back behaviour.
    pub fn history_page(&self, query: &HistoryQuery) -> AgentHistoryPage {
        let len = self.messages.len();
        let position = |id: &String| self.messages.iter().position(|m| &m.id == id);
        let (start, end) = match (&query.after, &query.before) {
            (Some(id), _) => match position(id) {
                Some(pos) => {
                    let start = pos + 1;
                    (start, start.saturating_add(query.limit).min(len))
                }
                // Unresolvable: report nothing owed in *either* direction rather
                // than letting `start == len` imply a backward page the caller
                // never asked for.
                None => {
                    return AgentHistoryPage {
                        messages: Vec::new(),
                        has_more_before: false,
                        has_more_after: false,
                    };
                }
            },
            (None, Some(id)) => {
                let end = position(id).unwrap_or(len);
                (end.saturating_sub(query.limit), end)
            }
            (None, None) => (len.saturating_sub(query.limit), len),
        };
        AgentHistoryPage {
            messages: self.messages[start..end].to_vec(),
            has_more_before: start > 0,
            has_more_after: end < len,
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
    /// Which run this is the report of. A cancelled run is still unwinding when
    /// the next one may already have started, and a report that arrives after
    /// its run was superseded must be dropped rather than clearing the *new*
    /// run's handle and delivering the old run's outcome as if it were its own.
    run_id: u64,
    outcome: RunOutcome,
}

/// The in-flight run: its identity and the token that cancels it.
struct RunHandle {
    id: u64,
    cancel: CancellationToken,
}

#[derive(Debug)]
enum RunOutcome {
    /// Agent ended its turn with plain text (no `conclude` tool registered).
    Completed {
        text: String,
    },
    /// Agent called its handoff tool; `calls` are the raw inputs, one per call.
    Concluded {
        calls: Vec<HandoffCall>,
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

/// Observer of an agent's durable history, notified once per event that is both
/// journaled and folded into state.
///
/// This is how a live stream learns what happened without reading the journal:
/// the actor is the only thing that touches its own log, and this is the seam it
/// publishes through. Implementations must not block — they run on the actor's
/// mailbox — and must treat delivery as best-effort.
pub trait AgentObserver: Send + Sync {
    /// `state` is the state *after* `event` was folded, so an observer that needs
    /// the resulting message can read `state.messages.last()` rather than
    /// re-deriving it from the event.
    fn publish(&self, event: &AgentDomainEvent, state: &AgentState);
}

/// An agent run, modelled as an event-sourced actor. Each `Run`/`InjectToolResult`
/// drives a background `horsie_agentcore::Agent` loop; coarse events are journaled
/// incrementally so a crashed session recovers its conversation and continues.
pub struct AgentActor {
    ctx: AgentRuntimeContext,
    params: AgentParams,
    running: Option<RunHandle>,
    /// Where durable history is published, when anyone is listening. `None` for
    /// workflow agents, which have no live stream.
    observer: Option<Arc<dyn AgentObserver>>,
    /// Id of the next run to start. Monotonic for this actor's loaded lifetime,
    /// which is all the fence needs — a report can only be stale within it.
    next_run_id: u64,
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
            observer: None,
            next_run_id: 0,
            pending_wake: false,
            cancel_acks: Vec::new(),
        }
    }

    /// Same actor, publishing its durable history to `observer` — what a session
    /// agent needs and a workflow agent does not.
    pub fn with_observer(
        ctx: AgentRuntimeContext,
        params: AgentParams,
        observer: Arc<dyn AgentObserver>,
    ) -> Self {
        Self {
            observer: Some(observer),
            ..Self::new(ctx, params)
        }
    }

    /// The journal identity of an agent session: kind `"agent"`, id = the session
    /// UUID. Centralizes the kind so the workflow (e.g. fork) and the actor agree.
    pub fn persistence_id_for(session_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", session_id.to_string())
    }

    /// Refuse to begin a turn while one is already in flight.
    ///
    /// `start_run` overwrites `self.running` with a fresh token, so a second start
    /// orphans the first run's cancel token and leaves two background loops
    /// persisting interleaved events into one journal — including two
    /// `tool_result`s for the same `tool_call_id`, which makes the provider 400 on
    /// every later turn (#61 item 3). Callers gate on session status, but that is a
    /// different actor's state; this is the invariant enforced where it lives.
    fn reject_if_running(&self, command: &str) -> Option<CommandEffect<AgentDomainEvent>> {
        self.running.as_ref()?;
        tracing::warn!(
            command,
            "refusing to start a turn while one is already running"
        );
        Some(CommandEffect::none())
    }

    fn start_run(&mut self, input: AgentInput, ctx: &ActorContext<Self>, history: Vec<Message>) {
        let cancel = CancellationToken::new();
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.running = Some(RunHandle {
            id: run_id,
            cancel: cancel.clone(),
        });

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
        let thinking_effort = self.params.thinking_effort;
        let max_retries = self.params.max_retries;
        let parent = self.ctx.parent.clone();
        let session_id = self.ctx.session_id;

        tokio::spawn(async move {
            // Provide this run's contexts on the spawned task (never the mailbox):
            // rehydrate the runtime, reconnect MCP, scan the workspace. A failure
            // here is a recoverable run failure -- report it and stop, exactly as a
            // provider/tool error would.
            //
            // Cancellable, because this is the *most* likely place to hang: it
            // awaits an MCP connect, a workspace scan and a SessionStart hook, all
            // of which cross a process boundary. Leaving it outside the cancel
            // path meant a stalled peer wedged the run exactly where `Stop` could
            // not reach it — `halt()` gave up after its timeout and the task
            // leaked for the process lifetime (#61 item 5b).
            let provided = tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    let _ = self_ref
                        .tell(AgentCommand::RunFinished(Box::new(RunReport {
                            run_id,
                            outcome: RunOutcome::Cancelled,
                        })))
                        .await;
                    return;
                }
                provided = context_provider.provide() => provided,
            };
            let contexts = match provided {
                Ok(c) => c,
                Err(error) => {
                    parent
                        .deliver(AgentOutcome::Failed {
                            session_id,
                            error: error.message,
                            recoverable: true,
                            terminal: error.terminal,
                        })
                        .await;
                    let _ = self_ref
                        .tell(AgentCommand::RunFinished(Box::new(RunReport {
                            run_id,
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
                thinking_effort,
                history,
                input,
                cancel,
            )
            .await;
            // All coarse events were already persisted (each `emit` awaited its ack),
            // so `RunFinished` lands after them in mailbox order.
            let _ = self_ref
                .tell(AgentCommand::RunFinished(Box::new(RunReport {
                    run_id,
                    outcome,
                })))
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
        // A report from a run that has already been superseded says nothing
        // about the run that is in flight now: clearing the handle on its word
        // would leave the live run unstoppable, and delivering its outcome
        // would tell the parent that a turn it never saw is over.
        if self.running.as_ref().map(|r| r.id) != Some(report.run_id) {
            tracing::warn!(
                run_id = report.run_id,
                current = ?self.running.as_ref().map(|r| r.id),
                "dropping the report of a superseded run"
            );
            return CommandEffect::none();
        }
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

        match report.outcome {
            RunOutcome::Completed { text } => {
                // No conclude tool: treat the final text as the output.
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        session_id,
                        usage_total: state.usage_total,
                    })
                    .await;
                parent
                    .deliver(AgentOutcome::Concluded {
                        session_id,
                        output: Value::String(text),
                    })
                    .await;
                // Resident: the agent goes idle, it does not die. Its whole
                // transcript stays in memory for the next turn and for history
                // reads, and nothing has to replay a journal to answer either.
                CommandEffect::none()
            }
            RunOutcome::Concluded { calls } => {
                match self.interpret(calls) {
                    Conclusion::Output(output) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                session_id,
                                usage_total: state.usage_total,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Concluded { session_id, output })
                            .await;
                        CommandEffect::none()
                    }
                    Conclusion::Ask(asks) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                session_id,
                                usage_total: state.usage_total,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Asked { session_id, asks })
                            .await;
                        // Stay alive — InjectToolResult resumes this same session.
                        // Snapshot to compact the incrementally-persisted log.
                        // Unconditional now that no cursor is a journal position:
                        // history and streams read state, so compaction is invisible.
                        CommandEffect::snapshot()
                    }
                    Conclusion::Park => self.park_or_resume(state, ctx, session_id, parent).await,
                }
            }
            RunOutcome::Cancelled => {
                // A cancelled tool call has no result and never will get one.
                // Journal the synthetic result now, where it belongs — directly
                // after the assistant message that made the call — rather than
                // recomputing it on a clone at the top of every later turn. The
                // journal is then a faithful record of what the model was shown,
                // and a mid-history dangle can no longer accumulate.
                let mut events: Vec<AgentDomainEvent> =
                    missing_tool_results(&state.messages, &self.params.handoff_tools())
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect();
                events.push(AgentDomainEvent::RunCancelled { at_ms: now_ms() });
                // Snapshot to compact the incrementally-persisted log on cancel.
                CommandEffect::persist(events).and_snapshot()
            }
            RunOutcome::Failed { error, recoverable } => {
                parent
                    .deliver(AgentOutcome::Failed {
                        session_id,
                        error,
                        recoverable,
                        // A run that failed inside the loop says nothing about
                        // whether the sandbox still exists.
                        terminal: false,
                    })
                    .await;
                // The partial conversation was already journaled incrementally, so the
                // failed session stays inspectable. The agent stays alive: a failed
                // turn is not a dead agent, and the next message reuses it.
                CommandEffect::none()
            }
            RunOutcome::AlreadyReported => {
                // Context preparation failed before the loop began; the failure was
                // already delivered to the parent. Stay alive so the next message
                // can retry against the same in-memory transcript.
                CommandEffect::none()
            }
        }
    }

    /// Decide whether a handoff payload is a final output, an ask, or a park.
    /// An `optional_handoff_tool` (e.g. the server crate's `ask_user` tool) is
    /// single-purpose — always an ask — so it bypasses `classify_conclusion`'s
    /// `has_output_schema`/`allow_ask_user`-based branching entirely, which
    /// exists only to disambiguate the workflow crate's multi-purpose `conclude`
    /// payload shape.
    fn interpret(&self, calls: Vec<HandoffCall>) -> Conclusion {
        if self.params.optional_handoff_tool.is_some() {
            return Conclusion::Ask(
                calls
                    .into_iter()
                    .map(|call| AskedQuestion {
                        tool_call_id: Some(call.tool_call_id),
                        question: call
                            .data
                            .get("question")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    })
                    .collect(),
            );
        }
        // A forced handoff is one conclusion, and `validate_handoff` rejects a
        // turn that calls it twice — so there is exactly one call here.
        let Some(call) = calls.into_iter().next() else {
            return Conclusion::Output(Value::Null);
        };
        classify_conclusion(
            self.params.has_output_schema,
            self.params.allow_ask_user,
            self.params.allow_timers,
            call.data,
            Some(call.tool_call_id),
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
                    terminal: false,
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
                message: wake.to_message(now_ms()),
            };
            self.start_run(wake, ctx, state.messages.clone());
            return CommandEffect::persist(vec![input_event]);
        }
        parent.deliver(AgentOutcome::Parked { session_id }).await;
        CommandEffect::persist(vec![AgentDomainEvent::Parked { at_ms: now_ms() }]).and_snapshot()
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
        let now = now_ms();
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
            at_ms: now,
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
            message: wake.to_message(now_ms()),
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
            "ask" => Conclusion::Ask(vec![AskedQuestion {
                tool_call_id,
                question: extract_question(&data),
            }]),
            _ => Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null)),
        };
    }
    match (has_output_schema, allow_ask_user) {
        // Kind-tagged union.
        (true, true) => {
            let kind = data.get("kind").and_then(Value::as_str).unwrap_or("submit");
            if kind == "ask" {
                Conclusion::Ask(vec![AskedQuestion {
                    tool_call_id,
                    question: extract_question(&data),
                }])
            } else {
                Conclusion::Output(data.get("output").cloned().unwrap_or(Value::Null))
            }
        }
        // Output only: the payload is the output.
        (true, false) => Conclusion::Output(data),
        // Ask only: the payload is a question.
        (false, true) => Conclusion::Ask(vec![AskedQuestion {
            tool_call_id,
            question: extract_question(&data),
        }]),
        // No conclude tool registered — shouldn't be reached via a handoff.
        (false, false) => Conclusion::Output(data),
    }
}

#[derive(Debug)]
enum Conclusion {
    Output(Value),
    /// One or more questions, all parked on together.
    Ask(Vec<AskedQuestion>),
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
                state.messages.push(message);
            }
            AgentDomainEvent::MessageComplete { message } => state.messages.push(message),
            AgentDomainEvent::ToolComplete {
                tool_call_id,
                output,
                is_error,
                at_ms,
            } => state
                .messages
                .push(Message::tool_result(tool_call_id, output, is_error, at_ms)),
            AgentDomainEvent::TimerArmed { record, .. } => state.timers.push(record),
            AgentDomainEvent::TimerCancelled { ids, .. } => {
                state.timers.retain(|t| !ids.contains(&t.id));
            }
            AgentDomainEvent::TimerFired {
                id,
                next_fire_at_unix_ms,
                ..
            } => match next_fire_at_unix_ms {
                Some(next) => {
                    if let Some(t) = state.timers.iter_mut().find(|t| t.id == id) {
                        t.fire_at_unix_ms = next;
                        t.fire_count += 1;
                    }
                }
                None => state.timers.retain(|t| t.id != id),
            },
            AgentDomainEvent::Parked { .. } => state.parked = true,
            AgentDomainEvent::TaskListChanged { snapshot, .. } => state.task_list = snapshot,
            AgentDomainEvent::RunComplete {
                usage,
                context_tokens,
                ..
            } => {
                state.usage_total.add(&usage);
                state.context_tokens = context_tokens;
                state.last_turn_usage = Some(usage);
            }
            AgentDomainEvent::RunCancelled { .. } => {}
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
            AgentCommand::Resume { results, message } => {
                if let Some(reason) = self.reject_if_running("Resume") {
                    return reason;
                }
                if results.is_empty() && message.is_none() {
                    tracing::warn!("Resume with neither results nor a message; ignoring");
                    return CommandEffect::none();
                }
                // The ids answered here are not dangling, whatever the recovered
                // history says: their results are in this very input.
                let answering: std::collections::HashSet<String> =
                    results.iter().map(|r| r.tool_call_id.clone()).collect();
                // Sanitize on every turn start: a history recovered from a
                // mid-turn crash may carry dangling tool calls (a no-op when
                // well-formed).
                let mut history =
                    repair_unanswered_tool_calls_except(state.messages.clone(), &answering);

                // Results that precede a user message belong to the history, not
                // to the input: the turn is started by what the user said.
                let mut events = Vec::new();
                let agent_input = match message {
                    Some(text) => {
                        if !results.is_empty() {
                            let recorded = AgentInput::tool_results(results).to_message(now_ms());
                            events.push(AgentDomainEvent::InputMessage {
                                message: recorded.clone(),
                            });
                            history.push(recorded);
                        }
                        AgentInput::user_message(new_message_id(), text)
                    }
                    None => AgentInput::tool_results(results),
                };
                // Persist the input message here (not via the streaming sink), so a
                // turn-restarting provider retry that re-emits it can never
                // double-persist it into two consecutive user messages.
                events.push(AgentDomainEvent::InputMessage {
                    message: agent_input.to_message(now_ms()),
                });
                self.start_run(agent_input, ctx, history);
                CommandEffect::persist(events)
            }
            AgentCommand::PersistProgress { events, ack } => {
                CommandEffect::persist(events).and_ack(ack)
            }
            AgentCommand::Cancel { ack } => {
                match (&self.running, ack) {
                    (Some(run), ack) => {
                        run.cancel.cancel();
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
                let now = now_ms();
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
                CommandEffect::persist(vec![AgentDomainEvent::TimerArmed {
                    record,
                    at_ms: now_ms(),
                }])
            }
            AgentCommand::ListTimers { reply } => {
                let now = now_ms();
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
                    CommandEffect::persist(vec![AgentDomainEvent::TimerCancelled {
                        ids,
                        at_ms: now_ms(),
                    }])
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
                            at_ms: now_ms(),
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
            AgentCommand::GetState { reply } => {
                let _ = reply.send(state.state_view());
                CommandEffect::none()
            }
            AgentCommand::Shutdown => CommandEffect::stop(),
        }
    }

    /// After recovery, repair whatever the crash left half-done, and re-drive an
    /// interrupted session. An empty history means nothing ran yet (the workflow
    /// will send `Run`); otherwise the process died mid-turn, so re-enter the
    /// loop with a synthetic continuation message. That continuation is
    /// intentionally not persisted as a new turn boundary: if we crash again
    /// before progress, recovery simply re-synthesizes it.
    /// Publish what just became durable. This is the whole reason a live stream
    /// no longer reads the journal: by the time this runs the events are written
    /// and folded, so `state` already contains the messages they appended.
    async fn on_events_persisted(&mut self, events: &[AgentDomainEvent], state: &AgentState) {
        let Some(observer) = &self.observer else {
            return;
        };
        for event in events {
            observer.publish(event, state);
        }
    }

    async fn on_recovery_complete(&mut self, state: &AgentState, ctx: &mut ActorContext<Self>) {
        // Re-arm every surviving timer with its remaining delay (fires immediately if
        // already due). Do this whether parked or mid-run, so timers keep firing.
        let now = now_ms();
        for t in &state.timers {
            spawn_timer_sleep(ctx.self_ref(), t.id.clone(), t.remaining(now));
        }
        // A tool call the dead process was running has no result and never will.
        // Record the repair once, here, where it still belongs at the end of the
        // transcript — recomputing it per turn instead is what let it drift into
        // the middle of a history nobody could then repair in place.
        let repairs = missing_tool_results(&state.messages, &self.params.handoff_tools());
        if !repairs.is_empty() {
            let (ack, _) = tokio::sync::oneshot::channel();
            let _ = ctx
                .self_ref()
                .tell(AgentCommand::PersistProgress {
                    events: repairs
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect(),
                    ack,
                })
                .await;
        }
        // Interactive sessions never self-continue: the user's next message is
        // the continuation.
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
        let history = repair_unanswered_tool_calls(state.messages.clone());
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
            // Carried on the streaming event, not re-read here: the in-memory
            // history already holds a message stamped with it.
            at_ms: ev.at_ms,
        }),
        AgentEvent::RunComplete(ev) => Some(AgentDomainEvent::RunComplete {
            usage: ev.usage.clone(),
            iterations: ev.iterations,
            context_tokens: ev.context_tokens,
            at_ms: ev.at_ms,
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

/// What a synthetic result says stands in for a tool call that never finished.
const INTERRUPTED_RESULT: &str = "interrupted, no result was recorded";

/// The synthetic results a history is missing, in call order — the repair as
/// *messages to journal*, where [`repair_unanswered_tool_calls`] returns the
/// repaired history to put on the wire.
///
/// Called at the two moments a call becomes permanently unanswerable — a cancel
/// and a recovery — so the repair is recorded where it belongs, at the end of
/// the transcript as it stands. Nothing else needs to journal it: a call that is
/// still in flight is not missing a result, it just does not have one yet.
///
/// A call to one of `handoff_tools` is exempt. Those park the agent — the run
/// ends on the call and the result comes later via `InjectToolResult` — so from
/// a journal alone a parked `ask_user` is indistinguishable from a call the dead
/// process was running, and recovery used to "repair" it. The user's answer was
/// then appended to a synthetic result already bearing the same `tool_use_id`,
/// and every later turn 400d on the duplicate. Idle offload made that routine:
/// any ask left unanswered past the idle timeout unloads and reloads.
///
/// Not journaling the repair is safe because [`repair_unanswered_tool_calls`]
/// still patches the history put on the wire, so an abandoned park can never
/// reach a provider dangling.
fn missing_tool_results(messages: &[Message], handoff_tools: &[String]) -> Vec<Message> {
    let answered: std::collections::HashSet<&str> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
            ContentPart::Text(_) | ContentPart::ToolCall(_) | ContentPart::Thinking(_) => None,
        })
        .collect();
    let dangling: Vec<String> = messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolCall(tc)
                if !answered.contains(tc.id.as_str()) && !handoff_tools.contains(&tc.name) =>
            {
                Some(tc.id.clone())
            }
            ContentPart::ToolCall(_)
            | ContentPart::Text(_)
            | ContentPart::ToolResult(_)
            | ContentPart::Thinking(_) => None,
        })
        .collect();
    if dangling.is_empty() {
        return Vec::new();
    }
    synthetic_results(dangling).collect()
}

/// Make a history well-formed for the provider: every `tool_use`, in *any*
/// assistant message, must have a matching `tool_result`. Any missing one (a
/// tool call interrupted by Stop or a crash) gets a synthetic error result so
/// the model can retry.
///
/// Repairing only the last assistant message is not enough. A Stop mid-turn
/// journals the assistant's tool call with no outcome (#45); once later turns
/// push that message off the end, a history rebuilt from the journal carries an
/// unanswered `tool_use` mid-history and the provider rejects *every* subsequent
/// turn with a 400 — the session is bricked until the journal is repaired.
///
/// Each repair is placed where the wire expects the result: directly after its
/// assistant message, joining any run of real results already following it —
/// never appended to the end of a history that has moved on to later turns.
///
/// Since [`missing_tool_results`] journals the repair at the moment a call
/// becomes unanswerable, this should now find nothing. It stays as the guard on
/// the one thing that must never reach a provider, and costs one pass over an
/// in-memory history.
fn repair_unanswered_tool_calls(messages: Vec<Message>) -> Vec<Message> {
    repair_dangling(messages, &std::collections::HashSet::new())
}

/// [`repair_unanswered_tool_calls`] for the resume-from-ask path, where
/// `answering` are the tool calls this very command is supplying results for
/// (e.g. every `ask_user` of a parked turn). They are about to be answered for
/// real, so they are not
/// dangling: repairing it too would put *two* results on one `tool_use_id` — the
/// duplicate shape stricter providers reject outright, and pure noise for the
/// ones that don't.
fn repair_unanswered_tool_calls_except(
    messages: Vec<Message>,
    answering: &std::collections::HashSet<String>,
) -> Vec<Message> {
    repair_dangling(messages, answering)
}

fn repair_dangling(
    messages: Vec<Message>,
    answering: &std::collections::HashSet<String>,
) -> Vec<Message> {
    let mut answered: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
            ContentPart::Text(_) | ContentPart::ToolCall(_) | ContentPart::Thinking(_) => None,
        })
        .collect();
    answered.extend(answering.iter().cloned());

    // Insertion index → the call ids needing a synthetic result there.
    let mut repairs: std::collections::BTreeMap<usize, Vec<String>> =
        std::collections::BTreeMap::new();
    for (i, m) in messages.iter().enumerate() {
        if m.role != Role::Assistant {
            continue;
        }
        let dangling: Vec<String> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall(tc) if !answered.contains(&tc.id) => Some(tc.id.clone()),
                ContentPart::ToolCall(_)
                | ContentPart::Text(_)
                | ContentPart::ToolResult(_)
                | ContentPart::Thinking(_) => None,
            })
            .collect();
        if dangling.is_empty() {
            continue;
        }
        // Past the results this turn *did* record, so a partially-answered
        // parallel batch stays one contiguous run.
        let mut at = i + 1;
        while messages.get(at).is_some_and(|next| next.role == Role::Tool) {
            at += 1;
        }
        repairs.entry(at).or_default().extend(dangling);
    }
    if repairs.is_empty() {
        return messages;
    }

    let mut out =
        Vec::with_capacity(messages.len() + repairs.values().map(Vec::len).sum::<usize>());
    for (i, m) in messages.into_iter().enumerate() {
        if let Some(ids) = repairs.remove(&i) {
            out.extend(synthetic_results(ids));
        }
        out.push(m);
    }
    // Calls left dangling by the final assistant message land past the end.
    for (_, ids) in repairs {
        out.extend(synthetic_results(ids));
    }
    out
}

fn synthetic_results(ids: Vec<String>) -> impl Iterator<Item = Message> {
    ids.into_iter()
        .map(|id| Message::tool_result(id, INTERRUPTED_RESULT, true, now_ms()))
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
    thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
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
            thinking_effort,
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
                    AgentResult::Handoff(h) => RunOutcome::Concluded { calls: h.calls },
                };
            }
            Err(AgentError::Cancelled) => return RunOutcome::Cancelled,
            Err(AgentError::Provider(e)) => {
                // Whether the failed attempt already wrote something durable.
                // `PersistSink` journals exactly the events `coarse_event` maps,
                // so this is the same test it applied — no proxy, no guessing.
                let journaled = captured.iter().any(|ev| coarse_event(ev).is_some());
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
                    // Report the classification rather than assuming recoverable:
                    // a permanent failure shown as transient invites the user to
                    // retry something that can never succeed.
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
    use horsie_models::agent::{TextPart, ToolCallPart, ToolResultPart};

    fn user_msg(text: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
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

    /// The observer replaces journal replay: it must see every durable event,
    /// after the fold, with the resulting message already in state.
    #[tokio::test]
    async fn an_observer_sees_durable_appends_with_folded_state() {
        use crate::{ContextError, ContextProvider, Contexts};
        use horsie_actor::{InMemoryJournal, Journal, spawn_root};

        struct NoContext;
        #[async_trait]
        impl ContextProvider for NoContext {
            async fn provide(&self) -> Result<Contexts, ContextError> {
                Err(ContextError::retryable("no context"))
            }
        }
        struct NoopSink;
        #[async_trait]
        impl EventSink for NoopSink {
            async fn emit(&self, _: horsie_agentcore::AgentEvent) -> Result<(), EventSinkError> {
                Ok(())
            }
        }
        struct DeafParent;
        #[async_trait]
        impl AgentOutcomeSink for DeafParent {
            async fn deliver(&self, _: AgentOutcome) {}
        }

        /// Records `(event, message-count-at-publish)` so the test can prove the
        /// fold already happened when the observer ran.
        #[derive(Default)]
        struct Recorder {
            seen: std::sync::Mutex<Vec<(String, usize)>>,
        }
        impl AgentObserver for Recorder {
            fn publish(&self, event: &AgentDomainEvent, state: &AgentState) {
                let label = match event {
                    AgentDomainEvent::InputMessage { message } => {
                        format!("input:{}", message.id)
                    }
                    AgentDomainEvent::MessageComplete { message } => {
                        format!("complete:{}", message.id)
                    }
                    other => format!("other:{other:?}"),
                };
                self.seen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((label, state.messages.len()));
            }
        }

        let session_id = uuid::Uuid::new_v4();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let recorder = Arc::new(Recorder::default());
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(NoContext),
            event_sink: Arc::new(NoopSink),
            parent: Arc::new(DeafParent),
            session_id,
        };
        let agent = spawn_root(
            AgentActor::with_observer(ctx, AgentParams::from_def(&def_fixture()), recorder.clone()),
            journal,
        );

        let one = user_msg("one");
        let two = user_msg("two");
        let (ack, ack_rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::PersistProgress {
                events: vec![
                    AgentDomainEvent::InputMessage {
                        message: one.clone(),
                    },
                    AgentDomainEvent::MessageComplete {
                        message: two.clone(),
                    },
                ],
                ack,
            })
            .await
            .unwrap();
        ack_rx.await.unwrap().unwrap();

        let seen = recorder.seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (format!("input:{}", one.id), 2),
                (format!("complete:{}", two.id), 2),
            ],
            "both events publish once, and state is already folded when they do"
        );
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
    fn a_replayed_tool_result_keeps_its_original_stamp() {
        // The stamp is journaled on the event rather than read from the clock
        // in `apply_event`; folding the same log twice must therefore produce
        // the same transcript, not one dated by whenever recovery happened.
        let fold = || {
            let mut state = AgentActor::initial_state();
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::ToolComplete {
                    at_ms: 1_700_000_000_123,
                    tool_call_id: "tc1".into(),
                    output: "result".into(),
                    is_error: false,
                },
            );
            state
        };
        let first = fold();
        let second = fold();
        assert_eq!(first.messages[0].created_at_ms, 1_700_000_000_123);
        assert_eq!(
            first.messages[0].created_at_ms,
            second.messages[0].created_at_ms
        );
    }

    #[test]
    fn coarse_events_carry_the_stamp_the_agent_recorded() {
        let tool = coarse_event(&AgentEvent::ToolComplete(
            horsie_models::events::ToolCompleteEvent {
                message_id: "result:tc1".into(),
                tool_call_id: "tc1".into(),
                output: "ok".into(),
                is_error: false,
                at_ms: 42,
            },
        ))
        .expect("ToolComplete is journaled");
        assert!(
            matches!(tool, AgentDomainEvent::ToolComplete { at_ms, .. } if at_ms == 42),
            "the streaming event's stamp must survive into the journal"
        );

        let run = coarse_event(&AgentEvent::RunComplete(
            horsie_models::events::RunCompleteEvent {
                message_id: "run".into(),
                usage: Usage::without_cache(1, 1),
                iterations: 1,
                context_tokens: 1,
                at_ms: 99,
            },
        ))
        .expect("RunComplete is journaled");
        assert!(matches!(run, AgentDomainEvent::RunComplete { at_ms, .. } if at_ms == 99));
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
                    created_at_ms: 0,
                    started_at_ms: None,
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
                at_ms: 0,
                tool_call_id: "tc1".into(),
                output: "result".into(),
                is_error: false,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                at_ms: 0,
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
        state = AgentActor::apply_event(state, AgentDomainEvent::RunCancelled { at_ms: 0 });
        assert_eq!(state.messages.len(), before);
    }

    #[test]
    fn repair_appends_error_results_for_dangling_tool_calls() {
        let history = vec![
            user_msg("do it"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
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
            Message::tool_result("tc1", "ok", false, 0),
        ];
        let fixed = repair_unanswered_tool_calls(history);
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
    fn answering_a_pending_ask_does_not_also_repair_it() {
        // The shape every ask_user answer resumes from: the call is dangling
        // *because* the user's answer is the result, arriving as this run's
        // input. Repairing it here would put a synthetic "interrupted" result
        // and the real answer on one tool_use_id.
        let history = vec![
            Message::user("m1", "pick a color", 0),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "m2".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "ask1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({ "question": "which?" }),
                })],
            },
        ];

        let answering = std::collections::HashSet::from(["ask1".to_string()]);
        let fixed = repair_unanswered_tool_calls_except(history.clone(), &answering);
        assert_eq!(fixed.len(), history.len(), "nothing is repaired: {fixed:?}");

        // Without the exclusion it *is* repaired — the bug this guards.
        assert_eq!(repair_unanswered_tool_calls(history).len(), 3);
    }

    /// The history an agent parked on an `ask_user` recovers from: the call is
    /// dangling because the user has not answered *yet*, not because anything
    /// died. Journaling a repair for it here is what put a synthetic
    /// "interrupted" result and the real answer on one `tool_use_id` — the
    /// duplicate every later turn then 400s on.
    fn parked_on_ask() -> Vec<Message> {
        vec![
            user_msg("what should I remove?"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a1".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "ask1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({ "question": "which?" }),
                })],
            },
        ]
    }

    #[test]
    fn recovery_does_not_repair_the_ask_the_session_is_parked_on() {
        let handoff = vec!["ask_user".to_string()];
        assert!(
            missing_tool_results(&parked_on_ask(), &handoff).is_empty(),
            "a parked ask is awaiting its answer, not interrupted"
        );
        // Without the exemption it *is* repaired — the bug this guards, which
        // bricked every session offloaded while awaiting an answer.
        assert_eq!(missing_tool_results(&parked_on_ask(), &[]).len(), 1);
    }

    #[test]
    fn an_interactive_sessions_ask_tool_is_a_handoff_tool() {
        // The wiring the recovery exemption depends on: the server sets
        // `ask_user` here, and nothing else tells the agent that call parks it.
        let mut params = AgentParams::from_def(&def_fixture());
        params.optional_handoff_tool = Some("ask_user".to_string());
        assert_eq!(params.handoff_tools(), vec!["ask_user".to_string()]);
    }

    #[test]
    fn a_timer_parked_agent_exempts_its_conclude_call() {
        let mut def = def_fixture();
        def.allow_timers = Some(true);
        assert_eq!(
            AgentParams::from_def(&def).handoff_tools(),
            vec![CONCLUDE_TOOL.to_string()]
        );
    }

    #[test]
    fn recovery_still_repairs_a_real_tool_call_left_dangling_beside_a_park() {
        let mut history = parked_on_ask();
        history.insert(1, assistant_call("a0", "died"));
        let repairs = missing_tool_results(&history, &["ask_user".to_string()]);
        let ids: Vec<String> = repairs
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolResult(r) => Some(r.tool_call_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            ids,
            vec!["died".to_string()],
            "only the dead call is repaired"
        );
    }

    #[test]
    fn a_park_is_never_journaled_as_interrupted_but_is_still_repaired_on_the_wire() {
        // The safety net that makes not journaling the repair safe: an ask that
        // really is abandoned still reaches the provider well-formed.
        let history = parked_on_ask();
        assert!(missing_tool_results(&history, &["ask_user".to_string()]).is_empty());
        assert!(
            unmatched_tool_uses(&repair_unanswered_tool_calls(history)).is_empty(),
            "the wire history must never carry a dangling tool_use"
        );
    }

    /// Every `tool_use` id in `messages` that has no matching `tool_result`
    /// anywhere — what the provider rejects a request for.
    fn unmatched_tool_uses(messages: &[Message]) -> Vec<String> {
        let answered: std::collections::HashSet<&str> = messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolResult(r) => Some(r.tool_call_id.as_str()),
                _ => None,
            })
            .collect();
        messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter_map(|p| match p {
                ContentPart::ToolCall(tc) if !answered.contains(tc.id.as_str()) => {
                    Some(tc.id.clone())
                }
                _ => None,
            })
            .collect()
    }

    fn assistant_call(id: &str, call_id: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: id.into(),
            role: Role::Assistant,
            parts: vec![ContentPart::ToolCall(ToolCallPart {
                id: call_id.into(),
                name: "read_file".into(),
                input: serde_json::json!({}),
            })],
        }
    }

    /// The session-bricking case: a Stop left a dangling call mid-history, and
    /// later turns pushed it off the end. Sanitizing only the last assistant
    /// message leaves it unrepaired, and the provider 400s on every later turn.
    #[test]
    fn repair_fixes_dangling_tool_calls_before_the_last_assistant_message() {
        let history = vec![
            user_msg("read it"),
            assistant_call("a1", "stopped"), // Stop landed here: no result ever journaled
            user_msg("never mind, do this instead"),
            assistant_call("a2", "tc2"),
            Message::tool_result("tc2", "ok", false, 0),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a3".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::Text(TextPart {
                    text: "done".into(),
                })],
            },
        ];
        let fixed = repair_unanswered_tool_calls(history);
        assert!(
            unmatched_tool_uses(&fixed).is_empty(),
            "dangling calls left in rebuilt history: {:?}",
            unmatched_tool_uses(&fixed)
        );
    }

    /// The repair must land where the wire expects a result — right after the
    /// assistant message that made the call — not appended to the end of a
    /// history that has moved on to later turns.
    #[test]
    fn repair_places_synthetic_result_next_to_its_assistant_message() {
        let history = vec![
            user_msg("read it"),
            assistant_call("a1", "stopped"),
            user_msg("never mind"),
            assistant_call("a2", "tc2"),
            Message::tool_result("tc2", "ok", false, 0),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        match &fixed[2].parts[0] {
            ContentPart::ToolResult(r) => {
                assert_eq!(r.tool_call_id, "stopped");
                assert!(r.is_error);
            }
            other => panic!("expected the synthetic result at index 2, got {other:?}"),
        }
        assert_eq!(fixed[2].role, Role::Tool);
    }

    /// A partially-answered parallel batch: the synthetic result joins the run
    /// of real results, still ahead of the next user turn.
    #[test]
    fn repair_appends_to_an_existing_run_of_tool_results() {
        let history = vec![
            user_msg("do both"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a1".into(),
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
            Message::tool_result("tc1", "ok", false, 0),
            user_msg("stop, do something else"),
        ];
        let fixed = repair_unanswered_tool_calls(history);
        match &fixed[3].parts[0] {
            ContentPart::ToolResult(r) => assert_eq!(r.tool_call_id, "tc2"),
            other => panic!("expected tc2's result after tc1's, got {other:?}"),
        }
        assert_eq!(fixed.last().unwrap().role, Role::User);
    }

    #[test]
    fn repair_leaves_well_formed_history_untouched() {
        let history = vec![
            user_msg("do it"),
            Message {
                created_at_ms: 0,
                started_at_ms: None,
                id: "a".into(),
                role: Role::Assistant,
                parts: vec![ContentPart::ToolCall(ToolCallPart {
                    id: "tc1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                })],
            },
            Message::tool_result("tc1", "ok", false, 0),
        ];
        let before = history.len();
        let fixed = repair_unanswered_tool_calls(history);
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

        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerArmed {
                at_ms: 0,
                record: rec,
            },
        );
        assert_eq!(state.timers.len(), 1);

        // Recurring fire re-arms in place with a carried next fire time and bumped count.
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerFired {
                at_ms: 0,
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
                at_ms: 0,
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
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot },
        );
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
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TaskListChanged { at_ms: 0, snapshot },
        );
        assert!(state.task_list.render().contains("Tasks (1/2 done)"));
    }

    fn with_messages(ids: &[&str]) -> AgentState {
        let mut state = AgentActor::initial_state();
        for id in ids {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::MessageComplete {
                    message: Message::user(*id, "x", 0),
                },
            );
        }
        state
    }

    fn page_ids(page: &AgentHistoryPage) -> Vec<String> {
        page.messages.iter().map(|m| m.id.clone()).collect()
    }

    #[test]
    fn history_tail_returns_the_last_window() {
        let state = with_messages(&["a", "b", "c", "d"]);
        let page = state.history_page(&HistoryQuery {
            before: None,
            after: None,
            limit: 2,
        });
        assert_eq!(page_ids(&page), ["c", "d"]);
        assert!(page.has_more_before, "a and b precede this window");
        assert!(!page.has_more_after, "d is the newest message");
    }

    #[test]
    fn history_before_cursor_pages_backward() {
        let state = with_messages(&["a", "b", "c", "d"]);
        let page = state.history_page(&HistoryQuery {
            before: Some("c".into()),
            after: None,
            limit: 2,
        });
        // Two messages immediately before "c": "a", "b".
        assert_eq!(page_ids(&page), ["a", "b"]);
        assert!(!page.has_more_before);
        assert!(page.has_more_after, "c and d follow this window");
    }

    #[test]
    fn history_after_cursor_pages_forward() {
        let state = with_messages(&["a", "b", "c", "d"]);
        let page = state.history_page(&HistoryQuery {
            before: None,
            after: Some("b".into()),
            limit: 2,
        });
        assert_eq!(page_ids(&page), ["c", "d"]);
        assert!(page.has_more_before);
        assert!(!page.has_more_after, "the window reaches the head");
    }

    #[test]
    fn history_after_cursor_reports_more_when_the_window_is_full() {
        let state = with_messages(&["a", "b", "c", "d"]);
        let page = state.history_page(&HistoryQuery {
            before: None,
            after: Some("a".into()),
            limit: 2,
        });
        assert_eq!(page_ids(&page), ["b", "c"]);
        assert!(
            page.has_more_after,
            "d is still owed — backfill must page on"
        );
    }

    /// A cursor naming a message this log does not have cannot be honoured, and
    /// guessing a window would hand the caller a silently wrong transcript.
    #[test]
    fn history_after_unknown_cursor_owes_nothing() {
        let state = with_messages(&["a", "b"]);
        let page = state.history_page(&HistoryQuery {
            before: None,
            after: Some("ghost".into()),
            limit: 10,
        });
        assert!(page.messages.is_empty());
        assert!(!page.has_more_after);
        assert!(!page.has_more_before);
    }

    #[test]
    fn history_tail_shorter_than_limit_has_no_more() {
        let state = with_messages(&["a", "b"]);
        let page = state.history_page(&HistoryQuery {
            before: None,
            after: None,
            limit: 10,
        });
        assert_eq!(page_ids(&page), ["a", "b"]);
        assert!(!page.has_more_before);
        assert!(!page.has_more_after);
    }

    #[test]
    fn state_view_carries_tasks_and_usage() {
        let mut state = with_messages(&["a"]);
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::RunComplete {
                at_ms: 0,
                usage: Usage::without_cache(4, 2),
                iterations: 1,
                context_tokens: 4,
            },
        );
        let view = state.state_view();
        assert_eq!(view.usage_total.input_tokens, 4);
        assert_eq!(view.context_tokens, 4);
        assert!(view.tasks.is_empty());
    }

    #[test]
    fn run_complete_accumulates_usage_total() {
        let mut state = AgentActor::initial_state();
        assert_eq!(state.usage_total, UsageTotal::default());
        for (input, output) in [(10u32, 5u32), (7, 3)] {
            state = AgentActor::apply_event(
                state,
                AgentDomainEvent::RunComplete {
                    at_ms: 0,
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
                at_ms: 0,
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
                at_ms: 0,
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
        state = AgentActor::apply_event(state, AgentDomainEvent::Parked { at_ms: 0 });
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
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerArmed {
                at_ms: 0,
                record: a,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerArmed {
                at_ms: 0,
                record: b,
            },
        );
        state = AgentActor::apply_event(
            state,
            AgentDomainEvent::TimerCancelled {
                at_ms: 0,
                ids: vec![ia],
            },
        );
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod retry_tests {
    use super::*;
    use horsie_agentcore::EventSinkError;
    use horsie_agentcore::testkit::{
        CollectingEventSink, FailingEventSink, MockProvider, MockToolbox, Script,
    };
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
            Arc::new(|_, input| Ok(input)),
        )
    }

    async fn run(
        provider: Arc<MockProvider>,
        toolbox: Arc<dyn Toolbox>,
        max_retries: u32,
    ) -> (RunOutcome, usize) {
        let sink: Arc<dyn EventSink> = Arc::new(CollectingEventSink::new());
        let outcome = run_with_retries(
            provider.clone(),
            toolbox,
            sink,
            "sys".into(),
            None,
            false,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
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
        let outcome = run_with_retries(
            provider.clone(),
            Arc::new(EmptyToolbox),
            sink,
            "sys".into(),
            None,
            false,
            Some(10),
            max_retries,
            None,
            vec![],
            AgentInput::user_message("m1", "go"),
            CancellationToken::new(),
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
        // Let the input message and the message-start through, so the provider is
        // genuinely engaged before the journal dies — the realistic shape.
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
        // The crux of #61 item 21: the retry rebuilds the turn from the ORIGINAL
        // history, which does not contain the events the failed attempt already
        // persisted. Retrying here would leave a phantom turn in the durable
        // transcript that the model never saw, replayed into every later turn.
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

/// The run-id fence: a report can only speak for the run it came from.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod fence_tests {
    use super::*;
    use crate::context::{ContextError, ContextProvider, Contexts};
    use horsie_actor::{InMemoryJournal, spawn_root};

    struct HangingProvider;
    #[async_trait]
    impl ContextProvider for HangingProvider {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            std::future::pending().await
        }
    }

    struct NoopSink;
    #[async_trait]
    impl EventSink for NoopSink {
        async fn emit(&self, _event: AgentEvent) -> Result<(), EventSinkError> {
            Ok(())
        }
    }

    struct OutcomeChannel(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
    #[async_trait]
    impl AgentOutcomeSink for OutcomeChannel {
        async fn deliver(&self, outcome: AgentOutcome) {
            let _ = self.0.send(outcome);
        }
    }

    /// A run that was superseded can still be unwinding, and its report must not
    /// be mistaken for the live run's. Taking its word for it would clear the
    /// live run's handle — leaving a turn nobody can stop and a parent told that
    /// a turn it never saw is over.
    #[tokio::test]
    async fn a_report_from_a_superseded_run_is_ignored() {
        let (tx, mut outcomes) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingProvider),
            event_sink: Arc::new(NoopSink),
            parent: Arc::new(OutcomeChannel(tx)),
            session_id: uuid::Uuid::new_v4(),
        };
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            output_schema: None,
            allow_ask_user: false,
            allow_timers: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.interactive = true;
        let journal = Arc::new(InMemoryJournal::new());
        let agent = spawn_root(AgentActor::new(ctx, params), journal);

        // Run 0 starts and hangs in `provide`, so it is genuinely in flight.
        agent
            .tell(AgentCommand::Resume {
                results: Vec::new(),
                message: Some("first".into()),
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // A report from some earlier run arrives late.
        agent
            .tell(AgentCommand::RunFinished(Box::new(RunReport {
                run_id: 99,
                outcome: RunOutcome::Completed {
                    text: "from a run that is over".into(),
                },
            })))
            .await
            .unwrap();

        // Run 0 is still in flight, so a second turn is refused — the fence
        // held. Without it, `running` would have been cleared and this would
        // start a second background loop against the same journal.
        agent
            .tell(AgentCommand::Resume {
                results: Vec::new(),
                message: Some("second".into()),
            })
            .await
            .unwrap();

        let (reply, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::GetHistory {
                query: HistoryQuery {
                    before: None,
                    after: None,
                    limit: 50,
                },
                reply,
            })
            .await
            .unwrap();
        let page = rx.await.unwrap();
        assert_eq!(
            page.messages.len(),
            1,
            "the refused turn must journal nothing: {:?}",
            page.messages
        );
        assert!(
            outcomes.try_recv().is_err(),
            "a superseded run's outcome must not reach the parent"
        );
    }
}
