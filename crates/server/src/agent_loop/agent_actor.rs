use crate::agent_loop::capabilities::{self, Act, Msg, TurnEvent};
use crate::agent_loop::context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext,
};
use crate::agent_loop::inbox::Summarise;
use crate::agent_loop::repair::{
    missing_tool_results, parked_call_ids, repair_unanswered_tool_calls,
    repair_unanswered_tool_calls_except,
};
use crate::agent_loop::retries::run_with_retries;
use crate::agent_loop::state::{
    AgentDomainEvent, AgentState, AgentStateView, AgentUsageSnapshot, ReadOutcome,
    coarse_appends_an_entry, coarse_event,
};
use crate::agent_loop::toolbox::AgentMailbox;
use crate::sessions::workflow::SUBMIT_RESULT_TOOL;
use async_trait::async_trait;
use horsie_actor::{
    ActorContext, ActorRef, CommandEffect, EventSourcedActor, PersistenceId, ReplyTo,
};
use horsie_agentcore::ToolOutcome;
use horsie_agentcore::{
    AgentEvent, AgentInput, EventSink, EventSinkError, LifecycleEvent, Message, StoppedCall,
};
use horsie_models::now_ms;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// Per-agent configuration distilled from an [`AgentRunDef`]. Runtime only.
#[derive(Clone)]
pub struct AgentParams {
    pub system_prompt: Option<String>,
    /// Whether this agent owes a structured result — true for a workflow step,
    /// which ends only by calling `submit_result`. Everything else finishes a
    /// turn with plain text, and that text *is* its answer.
    ///
    /// The one thing this decides: what a turn ending with text means. For a
    /// step it is either a park (something will wake it) or a mistake (nothing
    /// will); for anyone else it is the answer.
    pub requires_result: bool,
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
    /// What this agent's runner equipped it with.
    ///
    /// Config, and only ever read once: the first time this agent loads it is
    /// journaled as [`AgentDomainEvent::Equipped`], and from then on the
    /// journal is the source. That is what makes a capability's folded state
    /// survive an offload — re-equipping from here on every load would hand the
    /// agent a fresh, empty park every time it woke up.
    pub capabilities: crate::agent_loop::capabilities::Capabilities,
}

impl AgentParams {
    pub fn from_def(def: &AgentRunDef) -> Self {
        Self {
            system_prompt: def.system_prompt.clone(),
            requires_result: false,
            max_iterations: def.max_iterations,
            max_retries: def.max_retries.unwrap_or(0),
            thinking_effort: None,
            interactive: false,
            capabilities: crate::agent_loop::capabilities::Capabilities::default(),
        }
    }
}

/// How many turns an agent that owes a result may end without one before the
/// step is failed. Two: the first nudge is a plain message, the second forces
/// `submit_result` in `tool_choice`, and a model that defeats both is not going
/// to be talked round by a third.
const MAX_RESULT_NUDGES: u32 = 2;

/// Commands accepted by an [`AgentActor`].
pub enum AgentCommand {
    /// Something addressed to this agent: a person's message, a subagent's
    /// report, a timer firing, a `Stop` hook's continuation.
    ///
    /// Durable *before* anything is done with it, and `ack` reports the write —
    /// so a caller that must know an accepted message will survive a crash
    /// (`POST /sessions/:id/messages`) can wait for that rather than trust a
    /// mailbox. Whether it becomes a turn is this agent's own decision, taken
    /// immediately afterwards; see [`crate::agent_loop::queued_turn`].
    Enqueue {
        item: crate::agent_loop::Incoming,
        ack: Option<ReplyTo<Result<(), horsie_actor::JournalError>>>,
    },
    /// Answer every question this agent is parked on, at once.
    ///
    /// All or nothing: a set that does not cover them exactly is refused and
    /// nothing is journaled. A half-answered park could not resume anyway — the
    /// next provider call would carry a `tool_use` with no result.
    Answer {
        answers: Vec<crate::agent_loop::AskAnswer>,
        reply: ReplyTo<Result<(), crate::agent_loop::AnswerError>>,
    },
    /// Internal: reconsider whether the queue may start a turn now. Sent after
    /// anything that could have changed the answer.
    Drain,
    /// Cancel an in-flight run. `ack`, if given, fires once the run has actually
    /// terminated — immediately when none is in flight — so a caller that must
    /// know this incarnation will write nothing more (e.g. a session about to
    /// spawn a replacement agent on the same journal) can wait for it rather
    /// than racing it.
    Cancel { ack: Option<ReplyTo<()>> },
    /// Internal: coarse events captured mid-run. `ack` lets the emitting loop await
    /// the durable write before continuing, so persistence applies backpressure on
    /// the agent loop, and reports the write outcome so a journal failure aborts the
    /// run instead of proceeding on an unrecorded history. Persistence still flows
    /// through this one mailbox.
    PersistProgress {
        events: Vec<AgentDomainEvent>,
        ack: ReplyTo<Result<(), horsie_actor::JournalError>>,
    },
    /// Plugin hooks ran against one of this agent's tool calls. A `tell` with no
    /// ack: nothing waits on an audit trail, and recording what a hook did must
    /// never be able to slow the call it describes.
    HooksRan {
        records: Vec<horsie_models::hooks::HookRecord>,
    },
    /// Internal: a turn's pre-start hooks finished. Journal their records, then
    /// start the turn — or abandon it. Boxed to keep the command enum small.
    StartPrepared(Box<PreparedStart>),
    /// Internal: a background run finished. Boxed to keep the command enum small.
    RunFinished(Box<RunReport>),
    /// Internal: a sleep a capability asked for with [`Act::Wake`] has elapsed.
    ///
    /// Not journaled anywhere, in either direction: the durable fact is
    /// whatever the capability holds — an armed timer's `fire_at_unix_ms` — and
    /// a sleep is only ever a consequence of it. A wake for something that has
    /// since been cancelled is claimed by nobody and dropped, which is why an
    /// un-cancellable sleep task is harmless.
    Woke { id: String },
    /// The session answered something a capability asked it for.
    ///
    /// Internal, and arriving on the mailbox rather than being awaited inline:
    /// the ask is sent from a detached task, so a session busy starting the
    /// child cannot block this agent's queue, and the reply is ordered against
    /// everything else this agent does.
    SessionReplied { reply: capabilities::SessionReply },
    /// Something one of this agent's capabilities was asked to do.
    ///
    /// Built by the toolbox layer that claimed the name, on the run's own task,
    /// so the capability decides on the mailbox — where its state is — while
    /// the run waits. That is what lets a tool journal something and *then*
    /// answer the model, which no toolbox layer can do on its own.
    ///
    /// **The actor sees no tool names.** Which capability answers is decided by
    /// which arm the layer constructed, so there is nothing to match here and
    /// nothing that two capabilities could both claim. Only names a capability
    /// advertised become a command at all; everything else the layer passes
    /// straight to the sandbox without touching the mailbox.
    Capability(capabilities::CapCommand),
    /// Read forward from a cursor: durable entries plus, when the caller has
    /// caught up to the tail, the deltas of the message still being written.
    ///
    /// Answered from in-memory state — no journal access, no run. `after` of
    /// `None` means "from the very beginning", which is what a client with no
    /// position at all asks for.
    ReadLog {
        after: Option<crate::agent_loop::agent_log::Cursor>,
        reply: ReplyTo<ReadOutcome>,
    },
    /// Read a window *backwards* from a cursor — scroll-back. Separate from
    /// [`Self::ReadLog`] because it answers a different question and never
    /// carries deltas: nothing is being typed in the past.
    PageLog {
        before: Option<u64>,
        max: usize,
        reply: ReplyTo<crate::agent_loop::agent_log::LogPage>,
    },
    /// Record something that happened to the session in this agent's log.
    ///
    /// Sent by the session actor, which still owns the fact — this only makes
    /// it visible in the one ordered thing a client reads. Journaled here
    /// because the agent is the sole writer of its own log, which is what makes
    /// the order deterministic with no merge anywhere.
    RecordLifecycle { event: LifecycleEvent, at_ms: u64 },
    /// One chunk of the message currently being written.
    ///
    /// Routed through the mailbox rather than straight to readers so it is
    /// ordered against the entries around it: a chunk cannot overtake the entry
    /// it precedes, and the entry that supersedes it cannot land first. That
    /// ordering is the only reason this is a command at all — nothing here is
    /// journaled.
    RecordDelta { text: String },
    /// Where this agent's log stands. A fork's branch point, read before
    /// anything is written so the number names the moment the fork was asked
    /// for rather than the moment its seed happened to be built.
    LogHead { reply: ReplyTo<u64> },
    /// This agent's state as a fork's starting point, cut at `at_seq` — see
    /// [`AgentState::scrub_for_fork`]. Read-only: forking changes nothing about
    /// the conversation being forked.
    ForkSeed {
        at_seq: u64,
        reply: ReplyTo<Box<AgentState>>,
    },
    /// Adopt `state` as this agent's whole history, append `seed` after it, and
    /// queue `message` — all in one write.
    ///
    /// Sent once, to a fork, before it has run anything, which is what makes
    /// replacing state wholesale safe. Journaled as one batch rather than a
    /// snapshot written behind the actor's back, so the fork's own log explains
    /// where its history came from.
    ///
    /// The message rides along rather than being enqueued separately for two
    /// reasons, both learned the hard way: enqueued first, the fork drains and
    /// answers it *before* it has a history; enqueued after, a crash in between
    /// leaves a seeded fork with nothing to do.
    SeedFrom {
        state: Box<AgentState>,
        seed: Box<Message>,
        message: crate::agent_loop::Incoming,
        reply: ReplyTo<Result<(), String>>,
    },
    /// Stop this actor. Sent when the session it belongs to unloads: the agent
    /// is resident for the session's *loaded* lifetime, not forever, and going
    /// cold must not leave a task behind holding a whole transcript in memory.
    Shutdown,
    /// Read this agent's own usage + context-size snapshot — no messages or
    /// tasks, cheaper than `GetHistory` when only the numbers are needed.
    /// Backs the session-level usage aggregation.
    GetUsage { reply: ReplyTo<AgentUsageSnapshot> },
    /// Read this agent's current values — task list plus usage — for the agent
    /// document. Distinct from `GetHistory`, which returns transcript appends:
    /// these are values a client re-reads rather than accumulates.
    GetState { reply: ReplyTo<AgentStateView> },
    /// The exact facts a compaction must carry across verbatim.
    ///
    /// Answered from state on the mailbox, and asked from a *running* agent's
    /// own task: a compaction can happen mid-turn, and the task list it must
    /// preserve may have been changed by a tool call earlier in that same turn.
    /// Reading a copy taken at run start would carry a stale one.
    CarriedState { reply: ReplyTo<String> },
}

/// Everything a run starts from, gathered on the mailbox.
///
/// A struct because the run's own task cannot read state: whatever it needs has
/// to be read here and travel with it. Grouping them also keeps the two callers
/// honest — a new thing a run starts from is a field, and both callers have to
/// say what they pass for it.
struct RunStart {
    input: AgentInput,
    history: Vec<Message>,
    /// The prompt size the previous turn left behind, from durable state.
    context_tokens: u32,
    /// What this agent is equipped with, cloned off the mailbox because the
    /// run's task cannot read state.
    ///
    /// The list rather than the specs it produces: an advertisement can depend
    /// on what the load found — `spawn_agent` lists the installed agent types —
    /// and the facts only exist after `provide` has run, which is on the task.
    /// So the specs are computed there, from these.
    capabilities: crate::agent_loop::capabilities::Capabilities,
    /// A summary this turn was asked for, and what becomes of it.
    summarise: Option<Summarise>,
    /// Whether that summary is all this turn is.
    summarise_only: bool,
    /// What [`AgentActor::propose_turn`] got back from the token budget
    /// capability — `(trigger_at_percent, retain_percent)` — or `None` if no
    /// capability answered. Combined with the model's own context window,
    /// which only the run's task learns once its provider is resolved, to
    /// build the [`horsie_agentcore::CompactionBudget`] this run compacts
    /// against.
    compaction_target: Option<(u32, u32)>,
}

/// A turn whose pre-start hooks have run, on its way back to the actor.
///
/// Carries the drained turn untouched apart from a rewritten prompt: the prepare
/// step decides nothing about what the turn consumes, it only learns what the
/// hooks said.
pub struct PreparedStart {
    pub turn: crate::agent_loop::Turn,
    /// Records to journal before the turn snapshots its history — which is the
    /// whole reason this round-trip exists. Empty when no hook fired.
    pub records: Vec<horsie_models::hooks::HookRecord>,
    /// `Some` abandons the turn.
    pub abandon: Option<AbandonedStart>,
}

/// Why a prepared turn never ran.
pub enum AbandonedStart {
    /// A `UserPromptSubmit` hook refused the prompt. Deterministic for that
    /// prompt, so retrying it unchanged would be refused again.
    Blocked(String),
    /// Preparation could not complete — no runtime, most likely. The same
    /// failure `provide` would have reported one step later.
    Failed(crate::agent_loop::ContextError),
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
    /// A summary this run was asked to take for forks waiting on it, and how
    /// that went.
    ///
    /// Beside the outcome rather than inside it because the two are independent:
    /// a turn that summarises for a fork can still go on to answer a message
    /// queued alongside it, exactly as a queued `/compact` does. `None` means
    /// nothing asked.
    fork_summary: Option<ForkSummary>,
}

/// What a run produced for the forks waiting on it.
#[derive(Debug, Clone)]
pub struct ForkSummary {
    /// Every fork seeded from this one summary. They share a branch point, so
    /// they are entitled to share the provider call.
    pub forks: Vec<Uuid>,
    pub result: Result<String, String>,
}

/// The in-flight run: its identity and the token that cancels it.
struct RunHandle {
    id: u64,
    cancel: CancellationToken,
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
    /// parent on the run task; the actor only needs to clear its `running` flag.
    AlreadyReported,
}

/// Events an agent may journal between snapshots before the next turn boundary
/// takes one.
///
/// An agent's state *is* its transcript, so a snapshot costs O(transcript) to
/// write — snapshotting every turn would be quadratic over a session. This
/// trades a bounded replay on recovery for a bounded write amplification.
const SNAPSHOT_EVERY_EVENTS: u64 = 200;

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
    /// Events journaled since a snapshot was last *requested*. Counting requests
    /// rather than confirmed writes means a failed snapshot simply waits another
    /// interval, which is the right instinct for an optimization: retrying hard
    /// against a failing journal helps nobody.
    events_since_snapshot: u64,
    /// Id of the next run to start. Monotonic for this actor's loaded lifetime,
    /// which is all the fence needs — a report can only be stale within it.
    next_run_id: u64,
    /// Whether this agent's session has a runtime to run on.
    ///
    /// Seeded at spawn and moved by the `Runtime` lifecycle records the owner
    /// already sends — so nothing carries this fact but the log entry a reader
    /// sees anyway. In-memory on purpose: an agent that does not exist cannot
    /// be holding a turn, and one that is respawned is built with the answer
    /// that was true at the time.
    ready: bool,
    /// What the next run should tell the provider about tool use. Taken when a
    /// run starts, so it applies to exactly one turn. Set only when re-running a
    /// turn that ended without the result it owed — see the nudge in
    /// `handle_finished`. In-memory: a process that died mid-nudge starts the
    /// turn again from the queue, and a fresh attempt is the right default.
    pending_tool_choice: Option<horsie_agentcore::ToolChoice>,
    /// What a capability concluded this turn, waiting for the run to report
    /// back so it can be delivered as the agent's result.
    ///
    /// In-memory and per-turn, like `pending_tool_choice` above and for the
    /// same reason: a process that died between the tool call and the run
    /// ending replays the turn from the queue, and a fresh attempt is the right
    /// default. The durable copy is the capability's own journaled event.
    ///
    /// This is what replaces `interpret` recognising `submit_result` by name:
    /// the actor no longer knows which tools finish a run, it knows what its
    /// capabilities asked it to do.
    pending_conclusion: Option<Value>,
    /// A prepare step is in flight. Gates a second `Resume` exactly as `running`
    /// does: between `Resume` and `StartPrepared` no run exists yet, so
    /// `running` alone would let two turns through and land two runs on one
    /// journal.
    preparing: bool,
    /// Whether this agent load has fired its start hook. Deliberately **not**
    /// journaled — a rehydrated agent fires again, which is precisely what
    /// `source: "resume"` means.
    start_hook_fired: bool,
    /// Callers waiting to hear that the in-flight run has terminated (see
    /// [`AgentCommand::Cancel`]). Drained the moment `RunFinished` is handled —
    /// the run task sends that as its very last act, so every journal write it
    /// could make has already happened by then.
    cancel_acks: Vec<ReplyTo<()>>,
    /// Chunks of the message currently being written, since the newest log
    /// entry. Cleared whenever an entry lands, because the entry supersedes
    /// them.
    ///
    /// Deliberately not journaled and not part of the fold. A delta's useful
    /// life ends when the finished message arrives — under a second — and
    /// persisting one would put a write transaction on the critical path of
    /// every token for data nothing will ever read again.
    deltas: Vec<String>,
    /// A counter, bumped whenever this agent moves, for readers to wait on.
    /// Only the fact that something happened travels through here; what
    /// happened is read from state, which is what leaves nothing to overflow.
    ///
    /// Held behind an `Arc` because the *owner* is whoever outlives this actor
    /// — for a session agent that is the supervisor, so an idle offload does
    /// not disconnect a reader and send it round the reconnect-reload loop. A
    /// standalone agent owns its own and the distinction costs nothing.
    revision: std::sync::Arc<tokio::sync::watch::Sender<crate::sessions::Revision>>,
}

impl AgentActor {
    pub fn new(ctx: AgentRuntimeContext, params: AgentParams) -> Self {
        let revision = ctx.revision.clone();
        let ready = ctx.ready;
        Self {
            ctx,
            params,
            running: None,
            observer: None,
            events_since_snapshot: 0,
            next_run_id: 0,
            ready,
            pending_tool_choice: None,
            pending_conclusion: None,
            preparing: false,
            start_hook_fired: false,
            cancel_acks: Vec::new(),
            deltas: Vec::new(),
            revision,
        }
    }

    /// Announce that this agent has moved, waking every reader waiting on it.
    ///
    /// Called after anything a reader could want to see — a new entry, another
    /// delta, a cleared delta buffer. Announcing twice for one change is
    /// harmless: a reader that finds nothing new simply waits again.
    fn publish_revision(&self) {
        self.revision.send_modify(|r| *r += 1);
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

    /// Snapshot at a turn boundary, but only once enough events have accrued.
    ///
    /// Without this an agent that only ever converses — no ask, no park, no
    /// cancel — would never snapshot, and every recovery would stay a full
    /// replay of the whole transcript.
    /// Counting requests rather than confirmed writes means a failed snapshot
    /// simply waits another interval, which is the right instinct for an
    /// optimization: retrying hard against a failing journal helps nobody.
    fn snapshot_due(&mut self) -> bool {
        if self.events_since_snapshot < SNAPSHOT_EVERY_EVENTS {
            return false;
        }
        self.events_since_snapshot = 0;
        true
    }

    /// Persist `events`, taking a snapshot too if enough have accrued. The
    /// shape of every turn boundary that also ends a run.
    fn persist_maybe_snapshot(
        &mut self,
        events: Vec<AgentDomainEvent>,
    ) -> CommandEffect<AgentDomainEvent> {
        let effect = CommandEffect::persist(events);
        match self.snapshot_due() {
            true => effect.and_snapshot(),
            false => effect,
        }
    }

    /// The journal identity of an agent: kind `"agent"`, id = the agent's own
    /// [`AgentRuntimeContext::journal_id`]. Centralizes the kind so the workflow
    /// (e.g. fork) and the actor agree.
    pub fn persistence_id_for(journal_id: uuid::Uuid) -> PersistenceId {
        PersistenceId::new("agent", journal_id.to_string())
    }

    /// Refuse to begin a turn while one is already in flight — running, or still
    /// in its prepare step.
    ///
    /// `start_run` overwrites `self.running` with a fresh token, so a second start
    /// orphans the first run's cancel token and leaves two background loops
    /// persisting interleaved events into one journal — including two
    /// `tool_result`s for the same `tool_call_id`, which makes the provider 400 on
    /// every later turn (#61 item 3). Callers gate on session status, but that is a
    /// different actor's state; this is the invariant enforced where it lives.
    ///
    /// `preparing` is part of it because a turn between the drain decision and
    /// `StartPrepared` has no run yet: gating on `running` alone would let a
    /// second drain straight through into the same collision.
    fn busy(&self) -> bool {
        self.running.is_some() || self.preparing
    }

    /// Reconsider whether the queue may start a turn, and start it if so.
    ///
    /// Called after everything that could have changed the answer: something
    /// arriving, a turn ending, a park, a readiness flip. Deliberately silent
    /// when it decides against — finding a run already in flight is the normal
    /// case, not a fault, and the queue simply waits for the next boundary.
    ///
    /// `state` must be the state as the caller's own events leave it, not the
    /// pre-command snapshot: an agent that has just journaled `ParkedOn` is
    /// parked as far as this decision is concerned, and asking against the
    /// snapshot would drain a report the park is supposed to hold.
    async fn try_drain(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> Vec<AgentDomainEvent> {
        if self.busy() || !self.ready {
            return Vec::new();
        }
        match crate::agent_loop::queued_turn(&state.inbox, &state.asks) {
            Some(turn) => self.begin_turn(turn, state, ctx).await,
            None => Vec::new(),
        }
    }

    /// Perform one turn decision: record what it consumes and answers, tell the
    /// owner the turn began, then run its pre-start hooks before the run itself.
    ///
    /// `TurnBegan` is journaled here, at the decision, rather than after the
    /// hooks: a crash in the hook window replays with the queue still owed,
    /// which redelivers the message — the same at-least-once the session's
    /// tell-then-persist has always had, and the direction to err in.
    async fn begin_turn(
        &mut self,
        turn: crate::agent_loop::Turn,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> Vec<AgentDomainEvent> {
        let mut events = vec![AgentDomainEvent::TurnBegan {
            consumed: turn.consumed.clone(),
            answered: turn.answered.clone(),
            at_ms: now_ms(),
        }];
        // Every capability hears the boundary, against the state as it stands —
        // so one holding a park sees it still open and can record that this
        // turn is what ended it. Whether the park was answered or abandoned was
        // decided before this, by whoever built the turn.
        if let Some(performed) = Self::consult(state, &Msg::Turn(TurnEvent::Began)) {
            events.extend(performed.events);
            debug_assert!(
                performed.answer.is_none() && performed.resume.is_empty(),
                "a turn boundary has no tool call to answer and nothing to resume from"
            );
            // Nothing asks for a sleep here today, but dropping one silently is
            // the failure this design cannot afford: a capability that did
            // would simply never be woken.
            Self::spawn_wakes(performed.wakes, ctx);
        }
        // The owner no longer learns a turn began by being the thing that began
        // it, so it is told. Before the work, not after: this is what moves a
        // session to `Running`.
        self.ctx
            .parent
            .deliver(AgentOutcome::Started {
                agent: self.ctx.journal_id,
            })
            .await;

        let start = crate::agent_loop::StartTurn {
            // An agent that has never spoken to a provider is starting up;
            // anything else was folded from a journal. Read off the *LLM*
            // entries rather than the log, which a queued message alone already
            // appends to.
            start_source: (!self.start_hook_fired).then_some(match state.has_run() {
                false => horsie_models::runtime::SessionStartSource::Startup,
                true => horsie_models::runtime::SessionStartSource::Resume,
            }),
            prompt: turn.message.clone(),
        };
        let nothing_due = start.start_source.is_none() && start.prompt.is_none();
        if nothing_due || !self.ctx.context_provider.has_start_hooks() {
            events.extend(
                self.start_prepared(
                    PreparedStart {
                        turn,
                        records: Vec::new(),
                        abandon: None,
                    },
                    state,
                    ctx,
                )
                .await,
            );
            return events;
        }
        self.preparing = true;
        // Set when the prepare task is *spawned*, not when it returns: a
        // failed prepare must not re-fire the start hook on the next turn,
        // which would inject its context a second time.
        self.start_hook_fired = true;
        let provider = self.ctx.context_provider.clone();
        let self_ref = ctx.self_ref();
        tokio::spawn(async move {
            let prepared = match provider.start_hooks(start).await {
                Ok(prep) => PreparedStart {
                    abandon: crate::agent_loop::start_blocked(&prep.records)
                        .map(AbandonedStart::Blocked),
                    records: prep.records,
                    // A rewritten prompt replaces the turn's input; an absent
                    // one leaves what the user actually sent.
                    turn: crate::agent_loop::Turn {
                        message: prep.message.or(turn.message),
                        ..turn
                    },
                },
                Err(error) => PreparedStart {
                    turn,
                    records: Vec::new(),
                    abandon: Some(AbandonedStart::Failed(error)),
                },
            };
            let _ = self_ref
                .tell(AgentCommand::StartPrepared(Box::new(prepared)))
                .await;
        });
        events
    }

    /// Journal a prepared turn's hook records, then start it — or abandon it.
    ///
    /// The records are folded into a local copy of state before the prompt is
    /// read, which is the whole point of the prepare step: `state` here is the
    /// pre-command snapshot, and a `SessionStart` record that is not folded in
    /// first would first reach the model on the *next* turn.
    async fn start_prepared(
        &mut self,
        prepared: PreparedStart,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> Vec<AgentDomainEvent> {
        let PreparedStart {
            turn,
            records,
            abandon,
        } = prepared;
        let crate::agent_loop::Turn {
            message,
            subagent_results,
            results,
            summarise,
            ..
        } = turn;
        // A turn that carries only a summarisation has nothing to say to the
        // model. Running it would spend a provider call answering a message
        // nobody sent, so the summary *is* the turn.
        let summarise_only = summarise.is_some()
            && message.is_none()
            && subagent_results.is_empty()
            && results.is_empty();

        let at_ms = now_ms();
        let mut events = Vec::new();
        let mut folded = state.clone();
        for (seq, record) in (state.hook_entry_count()..).zip(records) {
            let event = AgentDomainEvent::HookRan { record, seq, at_ms };
            folded = Self::apply_event(folded, event.clone());
            events.push(event);
        }

        if let Some(abandon) = abandon {
            // A preparation failure is reported exactly as the same failure
            // coming out of `provide` would be — `terminal` above all, which is
            // what tells the session its sandbox is gone for good rather than
            // merely unreachable. A refusal is neither: the prompt was read and
            // rejected, so retrying it unchanged would be rejected again.
            let (error, recoverable, terminal) = match abandon {
                AbandonedStart::Blocked(reason) => (reason, false, false),
                AbandonedStart::Failed(e) => (e.message, true, e.terminal),
            };
            self.ctx
                .parent
                .deliver(AgentOutcome::Failed {
                    agent: self.ctx.journal_id,
                    error,
                    recoverable,
                    terminal,
                })
                .await;
            // The records are still journaled: a user whose prompt was refused
            // must be able to see which plugin refused it and why.
            return events;
        }

        // The ids answered here are not dangling, whatever the recovered
        // history says: their results are in this very input.
        let answering: std::collections::HashSet<String> =
            results.iter().map(|r| r.tool_call_id.clone()).collect();
        // Sanitize on every turn start: a history recovered from a
        // mid-turn crash may carry dangling tool calls (a no-op when
        // well-formed).
        let mut history = repair_unanswered_tool_calls_except(folded.prompt_messages(), &answering);

        // Results that precede a user message belong to the history, not
        // to the input: the turn is started by what the user said.
        let starts_a_user_turn = message.is_some() || !subagent_results.is_empty();
        let agent_input = if starts_a_user_turn {
            if !results.is_empty() {
                let recorded = AgentInput::tool_results(results).to_message(now_ms());
                events.push(AgentDomainEvent::InputMessage {
                    message: recorded.clone(),
                });
                history.push(recorded);
            }
            AgentInput::user_message_with_results(
                new_message_id(),
                message.unwrap_or_default(),
                subagent_results,
            )
        } else {
            AgentInput::tool_results(results)
        };
        // Persist the input message here (not via the streaming sink), so a
        // turn-restarting provider retry that re-emits it can never
        // double-persist it into two consecutive user messages.
        //
        // A summarise-only turn is the one case with no input at all: nothing
        // was typed and nothing is owed, so this would journal the empty `Tool`
        // message `AgentInput::tool_results(vec![])` builds — which the run
        // below never reads, but which every *later* turn would then carry in
        // its prompt.
        if !summarise_only {
            events.push(AgentDomainEvent::InputMessage {
                message: agent_input.to_message(now_ms()),
            });
        }
        // Before the run is built, not after: `TurnProposed` is what the token
        // budget capability answers "should this turn compact, and to what
        // target?" on, and the answer has to be in hand by the time the run's
        // task builds its `CompactionBudget`.
        let compaction_target = Self::propose_turn(&folded, ctx);
        self.start_run(
            RunStart {
                input: agent_input,
                history,
                context_tokens: folded.context_tokens,
                capabilities: folded.capabilities.clone(),
                summarise: summarise.clone(),
                summarise_only,
                compaction_target,
            },
            ctx,
        );
        events
    }

    fn start_run(&mut self, start: RunStart, ctx: &ActorContext<AgentCommand>) {
        let RunStart {
            input,
            history,
            context_tokens,
            capabilities,
            summarise,
            summarise_only,
            compaction_target,
        } = start;
        let cancel = CancellationToken::new();
        let run_id = self.next_run_id;
        self.next_run_id += 1;
        self.running = Some(RunHandle {
            id: run_id,
            cancel: cancel.clone(),
        });

        let self_ref = ctx.self_ref();
        let context_provider = self.ctx.context_provider.clone();
        let configured_prompt = self.params.system_prompt.clone();
        // Normally `None`, meaning `Auto`: a turn may end with text, and which
        // tools end a run is the toolbox's business. Set only when this turn is
        // re-running one that ended without the result it owed.
        let tool_choice = self.pending_tool_choice.take();
        let max_iterations = self.params.max_iterations;
        let thinking_effort = self.params.thinking_effort;
        let max_retries = self.params.max_retries;
        let parent = self.ctx.parent.clone();
        let agent = self.ctx.journal_id;
        // The same value, named for the other job it does. `journal_id` is this
        // agent's own identity, and only a *main* agent's identity is a session
        // id — a subagent or a workflow step carries its own uuid. Each has its
        // own history, and so its own cacheable prefix, which is exactly the
        // granularity a provider grouping requests by conversation wants.
        let conversation_id = agent.to_string();

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
                            fork_summary: None,
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
                            agent,
                            error: error.message,
                            recoverable: true,
                            terminal: error.terminal,
                        })
                        .await;
                    let _ = self_ref
                        .tell(AgentCommand::RunFinished(Box::new(RunReport {
                            run_id,
                            outcome: RunOutcome::AlreadyReported,
                            fork_summary: None,
                        })))
                        .await;
                    return;
                }
            };
            // Each capability wraps the sandbox in its own layer, first one
            // outermost. That wrapping *is* the routing: the layer that claims a
            // name says which command a call to it becomes, so a name resolves
            // to one capability at advertisement and at execution by being
            // resolved once.
            //
            // Layered *here*, after `provide`, because that is the first moment
            // the facts exist: `sub_agent` lists the installed agent types, and
            // the scan that found them is the runtime capability's `setup`,
            // which `provide` has just run. Composed on the mailbox instead, the
            // model would be shown a `spawn_agent` that names no types at all —
            // and the layer captures what it advertised, so a refusal on the
            // mailbox names the same list.
            let facts = contexts.facts;
            let mailbox: Arc<dyn capabilities::Mailbox> = Arc::new(AgentMailbox {
                actor: self_ref.clone(),
            });
            let toolbox = capabilities.layer(contexts.toolbox, &facts, &mailbox);
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
                actor: self_ref.clone(),
            });
            // Auto-compaction needs both halves: a window from the context
            // layer, which is absent both when the session turned it off and
            // when the model's card declares none, and a target from the
            // token budget capability, absent when this runner equipped none —
            // see `AgentActor::propose_turn`. Either missing and this run does
            // not compact.
            let compaction = contexts.context_window.zip(compaction_target).map(
                |(context_window, (trigger_at_percent, retain_percent))| {
                    horsie_agentcore::CompactionBudget {
                        context_window,
                        trigger_at_percent,
                        retain_percent,
                    }
                },
            );
            let (outcome, fork_summary) = run_with_retries(
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
            )
            .await;
            // All coarse events were already persisted (each `emit` awaited its ack),
            // so `RunFinished` lands after them in mailbox order.
            let _ = self_ref
                .tell(AgentCommand::RunFinished(Box::new(RunReport {
                    run_id,
                    outcome,
                    fork_summary,
                })))
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
    fn aborted_message(&self) -> Option<Message> {
        let text = self.deltas.concat();
        (!text.trim().is_empty()).then(|| Message::assistant_text(new_message_id(), text, now_ms()))
    }

    /// Interpret what ended the run — a tool that stopped it, or a plain-text
    /// completion — and deliver the outcome to the parent. The conversation events were already persisted
    /// incrementally via [`AgentCommand::PersistProgress`], so this only records the
    /// terminal transition and decides the actor's lifecycle.
    async fn handle_finished(
        &mut self,
        report: RunReport,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
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
        let agent = self.ctx.journal_id;
        let parent = self.ctx.parent.clone();

        // Before the turn's own outcome, and unconditionally: the forks waiting
        // on this are a different conversation's business, and whether this turn
        // then went on to succeed, fail or be cancelled says nothing about
        // whether their summary was taken.
        if let Some(ForkSummary { forks, result }) = report.fork_summary {
            parent
                .deliver(AgentOutcome::ForkSummary {
                    agent,
                    forks,
                    result,
                })
                .await;
        }

        match report.outcome {
            RunOutcome::Completed { text } => {
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                    })
                    .await;
                if self.params.requires_result {
                    return self.ended_without_result(state, ctx, agent, parent).await;
                }
                parent
                    .deliver(AgentOutcome::Concluded {
                        agent,
                        output: Value::String(text),
                    })
                    .await;
                // Resident: the agent goes idle, it does not die. Its whole
                // transcript stays in memory for the next turn and for history
                // reads, and nothing has to replay a journal to answer either.
                //
                // A turn ending is a boundary, so whatever queued while it ran
                // starts the next one.
                let drained = self.try_drain(state, ctx).await;
                self.persist_maybe_snapshot(drained)
            }
            RunOutcome::Stopped { calls } => {
                match self.interpret(state, calls) {
                    Conclusion::Output(output) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                            })
                            .await;
                        parent
                            .deliver(AgentOutcome::Concluded { agent, output })
                            .await;
                        // The agent said its work is done, so whatever any
                        // capability is holding to wake it later is moot — an
                        // armed timer above all. Broadcast rather than decided
                        // here: which of them holds such a thing is not
                        // something the actor can know, and it is not a turn
                        // boundary, because a turn ends many times before the
                        // work does.
                        let concluded = Self::consult(state, &Msg::Concluded).unwrap_or_default();
                        Self::spawn_wakes(concluded.wakes, ctx);
                        let mut events = concluded.events;
                        let mut folded = state.clone();
                        for e in &events {
                            folded = Self::apply_event(folded, e.clone());
                        }
                        events.extend(self.try_drain(&folded, ctx).await);
                        self.persist_maybe_snapshot(events)
                    }
                    Conclusion::Parked => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                            })
                            .await;
                        // The park is already in the journal, put there by the
                        // capability that made it. All that is left is telling
                        // the owner, which is what moves the session to
                        // `AwaitingInput`.
                        parent
                            .deliver(AgentOutcome::Asked {
                                agent,
                                asks: state.asks.clone(),
                            })
                            .await;
                        let events = self.try_drain(state, ctx).await;
                        self.events_since_snapshot = 0;
                        CommandEffect::persist(events).and_snapshot()
                    }
                    Conclusion::Contradiction(calls) => {
                        parent
                            .deliver(AgentOutcome::UsageRecorded {
                                agent,
                                usage_total: state.usage_total,
                            })
                            .await;
                        self.correct_contradiction(calls, state, ctx).await
                    }
                }
            }
            RunOutcome::Cancelled => {
                // The tokens were spent whatever became of the turn that spent
                // them, and `RunAborted` has already landed — the sink awaits
                // each coarse write before `RunFinished` is told — so the total
                // read here is the one that includes them.
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
                    })
                    .await;
                // A cancelled tool call has no result and never will get one.
                // Journal the synthetic result now, where it belongs — directly
                // after the assistant message that made the call — rather than
                // recomputing it on a clone at the top of every later turn. The
                // journal is then a faithful record of what the model was shown,
                // and a mid-history dangle can no longer accumulate.
                let mut events: Vec<AgentDomainEvent> =
                    missing_tool_results(&state.prompt_messages(), &parked_call_ids(state))
                        .into_iter()
                        .map(|message| AgentDomainEvent::InputMessage { message })
                        .collect();
                // Whatever the model had already written is the only copy there
                // is: deltas are unjournaled by design, and the boundary entry
                // the stop is about to append clears them. Twenty-two minutes of
                // generation used to end here, with the transcript showing no
                // sign a turn had run at all.
                //
                // After the synthetic results, not before: a cancelled call's
                // result belongs directly under the message that made it, and
                // this text is a later message than that one.
                if let Some(salvaged) = self.aborted_message() {
                    events.push(AgentDomainEvent::MessageAborted { message: salvaged });
                }
                events.push(AgentDomainEvent::RunCancelled { at_ms: now_ms() });
                // Snapshot to compact the incrementally-persisted log on cancel.
                self.events_since_snapshot = 0;
                // A stop cancels the turn, not the promise: anything queued
                // while the cancelled turn ran starts the next one.
                let folded = events
                    .iter()
                    .cloned()
                    .fold(state.clone(), Self::apply_event);
                events.extend(self.try_drain(&folded, ctx).await);
                CommandEffect::persist(events).and_snapshot()
            }
            RunOutcome::Failed { error, recoverable } => {
                parent
                    .deliver(AgentOutcome::UsageRecorded {
                        agent,
                        usage_total: state.usage_total,
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

    /// What the tools that ended this run meant.
    ///
    /// No tool names at all: what a call meant is the capability that owns it to
    /// say, and it has already said so by the time the run reports back. A
    /// conclusion carries its output; a park is already journaled, so it is only
    /// reported.
    fn interpret(&mut self, state: &AgentState, calls: Vec<StoppedCall>) -> Conclusion {
        if calls.is_empty() {
            return Conclusion::Output(Value::Null);
        }
        if let Some(output) = self.pending_conclusion.take() {
            return Conclusion::Output(output);
        }
        let parked: std::collections::HashSet<&str> = state
            .asks
            .iter()
            .filter_map(|a| a.tool_call_id.as_deref())
            .collect();
        // Several questions in one turn is ordinary: they are asked together and
        // answered together, so the run is parked when every call that stopped
        // it is one a capability parked on.
        if calls
            .iter()
            .all(|c| parked.contains(c.tool_call_id.as_str()))
        {
            return Conclusion::Parked;
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
    /// would sit "running" for ever, so the model is nudged — first with a plain
    /// message, then with `submit_result` forced, and only then is the step
    /// failed.
    ///
    /// All three facts are this actor's own: the queue is in its state, its
    /// capabilities answer for the timers and the children, and its log carries
    /// every subagent lifecycle record the session wrote onto it. Nothing here
    /// asks the session anything.
    async fn ended_without_result(
        &mut self,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
        agent: uuid::Uuid,
        parent: Arc<dyn AgentOutcomeSink>,
    ) -> CommandEffect<AgentDomainEvent> {
        // The queue first: a subagent report that landed while the turn was
        // ending starts the next turn, and nothing needs classifying at all.
        let drained = self.try_drain(state, ctx).await;
        if !drained.is_empty() {
            return self.persist_maybe_snapshot(drained);
        }
        // Invariant 6: a capability holding something that will wake this agent
        // — a subagent still owing a report, a run still going, a timer armed —
        // says so here, and the turn ending is not the agent finishing.
        // Broadcast, because more than one of them may be holding something,
        // and merged, because any one of them is enough.
        let ended = Self::consult(state, &Msg::Turn(TurnEvent::Ended)).unwrap_or_default();
        Self::spawn_wakes(ended.wakes, ctx);
        let held = ended.hold;
        if !held.is_empty() {
            tracing::debug!(?held, "a turn ended with something still owed");
        }
        if !held.is_empty() || crate::agent_loop::carried_state::has_running_subagents(state) {
            parent.deliver(AgentOutcome::Parked { agent }).await;
            let parked = AgentDomainEvent::Parked { at_ms: now_ms() };
            self.events_since_snapshot = 0;
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
            self.pending_tool_choice = Some(horsie_agentcore::ToolChoice::Required(
                SUBMIT_RESULT_TOOL.to_string(),
            ));
        }
        let nudge = AgentDomainEvent::Received {
            item: crate::agent_loop::Incoming::User {
                id: format!("nudge-result:{}", state.nudges),
                text: format!(
                    "Your turn ended without calling `{SUBMIT_RESULT_TOOL}`, and nothing will \
                     wake you — you have no armed timers and no subagents still running. If \
                     the step's work is done, call `{SUBMIT_RESULT_TOOL}` now. If it is not, \
                     carry on working."
                ),
            },
            at_ms: now_ms(),
        };
        let nudged = AgentDomainEvent::Nudged { at_ms: now_ms() };
        let mut folded = Self::apply_event(state.clone(), nudge.clone());
        folded = Self::apply_event(folded, nudged.clone());
        let mut events = vec![nudge, nudged];
        events.extend(self.try_drain(&folded, ctx).await);
        CommandEffect::persist(events)
    }

    /// The model called two turn-enders at once. Tell each call why, and run the
    /// turn again.
    ///
    /// Error results rather than silence: every `tool_use` needs a
    /// `tool_result` for the conversation to stay valid, and a call left
    /// dangling is indistinguishable later from a question still waiting on the
    /// user.
    async fn correct_contradiction(
        &mut self,
        calls: Vec<StoppedCall>,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
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
                at_ms,
            })
            .collect();
        let nudged = AgentDomainEvent::Nudged { at_ms };
        events.push(nudged.clone());
        let mut folded = state.clone();
        for e in &events {
            folded = Self::apply_event(folded, e.clone());
        }
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
        folded = Self::apply_event(folded, resume.clone());
        events.push(resume);
        events.extend(self.try_drain(&folded, ctx).await);
        CommandEffect::persist(events)
    }
}

/// What a lifecycle record says about this agent's runtime, if anything.
///
/// Exhaustive on purpose: a variant added later has to state whether it bears
/// on whether this agent may run, rather than silently answering "no".
fn runtime_readiness(event: &LifecycleEvent) -> Option<bool> {
    match event {
        LifecycleEvent::Runtime(runtime) => Some(match runtime.status {
            horsie_agentcore::RuntimeStatus::Ready(_) => true,
            horsie_agentcore::RuntimeStatus::Acquiring(_)
            | horsie_agentcore::RuntimeStatus::Failed(_) => false,
        }),
        // Terminal: the runtime is gone for good and no later message brings it
        // back, so this agent must not start another turn.
        LifecycleEvent::SessionFailed(_) => Some(false),
        LifecycleEvent::Preparing(_)
        | LifecycleEvent::MessageQueued(_)
        | LifecycleEvent::TurnBegan(_)
        | LifecycleEvent::TurnEnded(_)
        | LifecycleEvent::AskRecorded(_)
        | LifecycleEvent::SubAgent(_)
        // A fork branching off says nothing about *this* agent's runtime:
        // they share the session's, and it was already up for the fork to
        // have been taken at all.
        | LifecycleEvent::Forked(_)
        // A compaction declining to fold anything is an answer to a typed
        // command. It touches neither the runtime nor the history.
        | LifecycleEvent::CompactionSkipped(_)
        | LifecycleEvent::Step(_)
        | LifecycleEvent::TaskList(_) => None,
    }
}

#[derive(Debug)]
enum Conclusion {
    Output(Value),
    /// A capability parked the run, and has already journaled the park. Nothing
    /// left to record — only the owner to tell.
    Parked,
    /// Two turn-enders at once. The calls are named so each can be told why.
    Contradiction(Vec<StoppedCall>),
}

#[async_trait]
impl EventSourcedActor for AgentActor {
    type Command = AgentCommand;
    type Event = AgentDomainEvent;
    type State = AgentState;

    fn persistence_id(&self) -> PersistenceId {
        Self::persistence_id_for(self.ctx.journal_id)
    }

    fn initial_state() -> AgentState {
        AgentState::default()
    }

    /// The fold lives on the state it folds into; see
    /// [`AgentState::apply`](crate::agent_loop::state::AgentState::apply).
    fn apply_event(state: AgentState, event: AgentDomainEvent) -> AgentState {
        state.apply(event)
    }

    async fn handle_command(
        &mut self,
        state: &AgentState,
        cmd: AgentCommand,
        ctx: &mut ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        match cmd {
            AgentCommand::Enqueue { item, ack } => {
                // Decided after the write, never before it: the queue a turn
                // drains has to be the durable one, so the drain arrives as its
                // own command and finds this event already folded in.
                let _ = ctx.self_ref().tell(AgentCommand::Drain).await;
                let effect = CommandEffect::persist(vec![AgentDomainEvent::Received {
                    item,
                    at_ms: now_ms(),
                }]);
                match ack {
                    Some(ack) => effect.and_ack(ack),
                    None => effect,
                }
            }
            AgentCommand::Drain => CommandEffect::persist(self.try_drain(state, ctx).await),
            AgentCommand::Answer { answers, reply } => {
                // A run in flight means the questions are already gone — a turn
                // beginning is what clears them — so there is nothing to answer.
                if self.busy() {
                    let _ = reply.send(Err(crate::agent_loop::AnswerError::NothingPending));
                    return CommandEffect::none();
                }
                // The capability holding the park answers for it, because the
                // park is its state. It claims the set only when it covers the
                // questions exactly, so `None` here means either no capability
                // holds a park or this set cannot resume one — and the old path
                // below still owns the diagnostic that says which.
                if let Some(performed) = Self::consult(state, &Msg::Answer(&answers)) {
                    let _ = reply.send(Ok(()));
                    // Folded first, so the `Began` broadcast inside `begin_turn`
                    // sees a park this answer has already closed and does not
                    // record it as abandoned. Both events clear the park, so
                    // this does not change what the agent *is* — it keeps the
                    // journal from claiming the person's answer was ignored on
                    // the very turn it was acted on.
                    let folded = performed
                        .events
                        .iter()
                        .fold(state.clone(), |s, e| Self::apply_event(s, e.clone()));
                    let turn = crate::agent_loop::resumed_turn(&folded.inbox, performed.resume);
                    let mut events = performed.events;
                    events.extend(self.begin_turn(turn, &folded, ctx).await);
                    return CommandEffect::persist(events);
                }
                match crate::agent_loop::answered_turn(&state.inbox, &state.asks, answers) {
                    Ok(turn) => {
                        let _ = reply.send(Ok(()));
                        CommandEffect::persist(self.begin_turn(turn, state, ctx).await)
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                        CommandEffect::none()
                    }
                }
            }
            AgentCommand::StartPrepared(prepared) => {
                self.preparing = false;
                CommandEffect::persist(self.start_prepared(*prepared, state, ctx).await)
            }
            AgentCommand::HooksRan { records } => {
                let at_ms = now_ms();
                // Counted here, against the state as it stands, and carried on
                // the event: `agent_frame` sees only the event, so deriving the
                // id at fold time would give the live stream different cursors
                // than `/history`.
                let mut seq = state.hook_entry_count();
                let events = records
                    .into_iter()
                    .map(|record| {
                        let event = AgentDomainEvent::HookRan { record, seq, at_ms };
                        seq += 1;
                        event
                    })
                    .collect();
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
            AgentCommand::Woke { id } => {
                let Some(performed) = Self::consult(state, &Msg::Woke { id: &id }) else {
                    // A sleep for something that has since been cancelled. The
                    // sleep task cannot be called back, so the drop happens
                    // here, and it is ordinary rather than a bug.
                    tracing::debug!(id, "a wake reached nothing still holding its id");
                    return CommandEffect::none();
                };
                self.finish_consult(performed, state, ctx).await
            }
            AgentCommand::RunFinished(report) => self.handle_finished(*report, state, ctx).await,
            AgentCommand::SessionReplied { reply } => {
                let Some(performed) = Self::consult(state, &Msg::Reply(&reply)) else {
                    // Every request carries the call that prompted it, and the
                    // capability that asked is the one that recognises it. So
                    // nothing claiming a reply means the capability is gone —
                    // an agent re-equipped without it, say — and the model is
                    // still parked on a call nobody will answer.
                    tracing::error!(
                        call = reply.call(),
                        "the session replied and no capability recognised it"
                    );
                    return CommandEffect::none();
                };
                self.finish_consult(performed, state, ctx).await
            }
            AgentCommand::Capability(cmd) => {
                let owner = cmd.owner();
                let Some(performed) = Self::consult_command(state, &cmd) else {
                    // A layer builds its own capability's arm, so nothing
                    // recognising one means that capability is not equipped at
                    // all — a bug, and the model is told rather than left
                    // waiting.
                    tracing::error!(
                        capability = owner,
                        "a command reached an agent that is not equipped with its capability"
                    );
                    if let Some(reply) = cmd.into_reply() {
                        let _ = reply.send(Err(horsie_agentcore::ToolCallError::ExecutionFailed(
                            format!("`{owner}` is advertised but nothing answers it"),
                        )));
                    }
                    return CommandEffect::none();
                };
                if !performed.resume.is_empty() {
                    // Resuming supplies results for calls left dangling by an
                    // earlier park. A call still in flight is not one of those,
                    // so this would pair a result with a `tool_use` that is
                    // about to get one anyway.
                    tracing::error!(
                        capability = owner,
                        "a capability asked to resume from a tool call still in flight"
                    );
                }
                self.dispatch_asks(performed.asks, ctx);
                Self::spawn_wakes(performed.wakes, ctx);
                let concluded = performed.conclusion.is_some();
                if let Some(output) = performed.conclusion {
                    // Held until the run reports back, which is the moment the
                    // owner can be told. Answering `StopRun` below is what ends
                    // the run that produces that report.
                    self.pending_conclusion = Some(output);
                }
                let answer = performed.answer.unwrap_or({
                    match concluded {
                        // A conclusion is the agent's work finishing, so the
                        // run has to stop for `interpret` to read it back. A
                        // result here instead would answer the call and carry
                        // on, and the model — having nothing left to do —
                        // submits the same result again until the loop
                        // detector ends the step.
                        true => Ok(ToolOutcome::StopRun),
                        // Claimed, but asked for nothing: the honest reply is
                        // an empty result rather than a hung call.
                        false => Ok(ToolOutcome::Result(Value::Null)),
                    }
                });
                let effect = self.persist_maybe_snapshot(performed.events);
                match cmd.into_reply() {
                    Some(reply) => answer_when_durable(effect, reply, answer),
                    // A person typing a built-in: nothing is waiting, so there
                    // is nothing to hold back.
                    None => effect,
                }
            }
            AgentCommand::RecordLifecycle { event, at_ms } => {
                // Almost every one of these is something a reader sees and this
                // agent does nothing about. The runtime arriving is the one
                // that changes what it may *do* — so it is read off the record
                // rather than announced separately, and a record that says
                // nothing about the runtime cannot start a turn. That is what
                // keeps recovery quiet: it journals a `TurnEnded(Interrupted)`,
                // which is not a runtime fact and drains nothing.
                let moved = runtime_readiness(&event).filter(|next| *next != self.ready);
                if let Some(next) = moved {
                    self.ready = next;
                }
                let recorded = AgentDomainEvent::LifecycleRecorded { event, at_ms };
                if moved != Some(true) {
                    return CommandEffect::persist(vec![recorded]);
                }
                let folded = Self::apply_event(state.clone(), recorded.clone());
                let mut events = vec![recorded];
                events.extend(self.try_drain(&folded, ctx).await);
                CommandEffect::persist(events)
            }
            AgentCommand::RecordDelta { text } => {
                self.deltas.push(text);
                self.publish_revision();
                CommandEffect::none()
            }
            AgentCommand::ReadLog { after, reply } => {
                let _ = reply.send(state.read_from(after, &self.deltas));
                CommandEffect::none()
            }
            AgentCommand::PageLog { before, max, reply } => {
                let _ = reply.send(crate::agent_loop::agent_log::page_before(
                    &state.log, before, max,
                ));
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
            AgentCommand::CarriedState { reply } => {
                let _ = reply.send(crate::agent_loop::carried_state::render_carried_state(
                    state,
                ));
                CommandEffect::none()
            }
            AgentCommand::LogHead { reply } => {
                let _ = reply.send(state.next_seq);
                CommandEffect::none()
            }
            AgentCommand::ForkSeed { at_seq, reply } => {
                let _ = reply.send(Box::new(state.scrub_for_fork(at_seq)));
                CommandEffect::none()
            }
            AgentCommand::SeedFrom {
                state: seeded,
                seed,
                message,
                reply,
            } => {
                // Already seeded. Not an error: a process that died between
                // this write and the session journaling `ForkSeeded` comes back
                // and re-seeds, and the honest answer is that the work is done.
                // Saying otherwise would fail a fork that is perfectly fine.
                if !state.log.is_empty() {
                    let _ = reply.send(Ok(()));
                    let _ = ctx.self_ref().tell(AgentCommand::Drain).await;
                    return CommandEffect::none();
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let answer = match rx.await {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(format!("persist the fork's history: {e}")),
                        Err(_) => Err("the fork's history was never written".to_string()),
                    };
                    let _ = reply.send(answer);
                });
                // Decided after the write, exactly as `Enqueue` does: the queue
                // a turn drains has to be the durable one.
                let _ = ctx.self_ref().tell(AgentCommand::Drain).await;
                CommandEffect::persist(vec![
                    AgentDomainEvent::Seeded {
                        state: seeded,
                        seed,
                    },
                    AgentDomainEvent::Received {
                        item: message,
                        at_ms: now_ms(),
                    },
                ])
                .and_ack(ReplyTo::from_sender(tx))
                // A whole conversation in one event is exactly the case a
                // snapshot exists for: without one, every later recovery
                // replays it.
                .and_snapshot()
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
        self.events_since_snapshot = self
            .events_since_snapshot
            .saturating_add(events.len() as u64);
        // An entry supersedes every chunk that preceded it — the finished
        // message says everything they were building towards — so the deltas
        // are dropped the moment one lands. This is also what keeps the delta
        // sub-sequence short and restartable: it counts within one entry, never
        // across the session.
        if events.iter().any(coarse_appends_an_entry) {
            self.deltas.clear();
        }
        self.publish_revision();
        let Some(observer) = &self.observer else {
            return;
        };
        for event in events {
            observer.publish(event, state);
        }
    }

    async fn on_recovery_complete(
        &mut self,
        state: &AgentState,
        ctx: &mut ActorContext<AgentCommand>,
    ) {
        // Announce where this incarnation starts. The channel outlives the
        // actor, so after an idle offload it still holds the position from
        // before — republishing costs nothing and keeps a reader that has been
        // waiting through the offload from having to guess.
        self.publish_revision();
        // Equip, once ever. An agent whose journal already says what it can do
        // keeps that, folded state and all — re-equipping from config here
        // would hand a parked agent an empty park and lose the questions it is
        // waiting on.
        if state.capabilities.is_empty() && !self.params.capabilities.is_empty() {
            let (ack, _) = tokio::sync::oneshot::channel();
            let _ = ctx
                .self_ref()
                .tell(AgentCommand::PersistProgress {
                    events: vec![AgentDomainEvent::Equipped {
                        capabilities: self.params.capabilities.clone(),
                        at_ms: now_ms(),
                    }],
                    ack: ReplyTo::from_sender(ack),
                })
                .await;
        }
        // Now tell them the fold is over, which closes the crash window a
        // request opens. A capability journals what it wants *before* asking
        // for it, so a `Requested` that survived to here may never have reached
        // the session at all — and the model is parked on a tool call nobody
        // will ever answer. Whoever is holding one asks again, with the ids it
        // already recorded, and the session recognises the repeat.
        //
        // After the `Equipped` above, and it only ever fires for a *recovered*
        // agent: a fresh one has nothing folded, so its capabilities are empty
        // here and the broadcast reaches nobody. Nothing is journaled either —
        // the `Requested` being re-asked is still the only record of it.
        let reloaded = Self::consult(state, &Msg::Loaded).unwrap_or_default();
        if !reloaded.events.is_empty() {
            tracing::error!("a capability journaled something on a load; discarded");
        }
        self.dispatch_asks(reloaded.asks, ctx);
        // And start the sleeps they asked for again. A sleep dies with the
        // process that spawned it, so every armed timer is re-armed here, with
        // its remaining delay — the same crash window an unanswered request
        // opens, closed the same way. Whether the agent is parked or mid-run,
        // so timers keep firing either way.
        Self::spawn_wakes(reloaded.wakes, ctx);
        // A tool call the dead process was running has no result and never will.
        // Record the repair once, here, where it still belongs at the end of the
        // transcript — recomputing it per turn instead is what let it drift into
        // the middle of a history nobody could then repair in place.
        let repairs = missing_tool_results(&state.prompt_messages(), &parked_call_ids(state));
        if !repairs.is_empty() {
            let (ack, _) = tokio::sync::oneshot::channel();
            let ack = ReplyTo::from_sender(ack);
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
            self.ctx
                .parent
                .deliver(AgentOutcome::Interrupted {
                    agent: self.ctx.journal_id,
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
        if state.log.is_empty() {
            return;
        }
        let history = repair_unanswered_tool_calls(state.prompt_messages());
        let compaction_target = Self::propose_turn(state, ctx);
        self.start_run(
            RunStart {
                input: AgentInput::user_message(new_message_id(), "continue the interrupted task"),
                history,
                context_tokens: state.context_tokens,
                capabilities: state.capabilities.clone(),
                summarise: None,
                summarise_only: false,
                compaction_target,
            },
            ctx,
        );
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Spawn one sleep a capability asked for.
///
/// Detached and un-cancellable, which is fine because the wake it sends is
/// claimed by whoever still holds the id: a capability that has since dropped
/// it answers `None` and the wake reaches nothing. Nothing is journaled here in
/// either direction — a wake is re-issued from durable state on a load, not
/// replayed from a log.
fn spawn_wake(self_ref: ActorRef<AgentCommand>, wake: Wake) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(wake.after_secs)).await;
        let _ = self_ref.tell(AgentCommand::Woke { id: wake.id }).await;
    });
}

/// What a capability's acts came to: events to journal, and — when the message
/// was a tool call — the answer to give the model.
///
/// A struct rather than a tuple because both halves are optional in different
/// ways, and a caller that swapped them would still compile.
#[derive(Default)]
struct Performed {
    events: Vec<AgentDomainEvent>,
    /// `Some` once some capability answered or parked the call in flight —
    /// which only a command has, so this is always `None` for a lifecycle
    /// message.
    answer: Option<Result<ToolOutcome, horsie_agentcore::ToolCallError>>,
    /// Tool results a capability supplied for calls it had parked, which start
    /// the next turn.
    resume: Vec<horsie_models::agent::ToolResultInput>,
    /// A capability said this agent's work is finished, and this is its result.
    conclusion: Option<Value>,
    /// Things a capability wants from the session. Sent off the mailbox, and
    /// their replies come back as [`Msg::Reply`].
    asks: Vec<capabilities::SessionRequest>,
    /// Why this turn's end is not the agent finishing, from every capability
    /// holding something. Non-empty parks the agent.
    hold: Vec<String>,
    /// Sleeps capabilities asked for. Spawned off the mailbox; each one comes
    /// back as [`AgentCommand::Woke`] with the id its capability minted.
    wakes: Vec<Wake>,
    /// What the token budget capability said on [`Msg::TurnProposed`], if one
    /// is equipped. `None` either way — no such capability, or one that had
    /// nothing to say — is indistinguishable, and both mean the same thing:
    /// this run gets no [`Act::CompactionBudget`] and does not compact.
    compaction: Option<(u32, u32)>,
}

/// One sleep a capability asked for.
#[derive(Debug, Clone)]
struct Wake {
    id: String,
    after_secs: u64,
}

impl AgentActor {
    /// Route a message to this agent's capabilities and work out what they
    /// asked for.
    ///
    /// Pure apart from the logging: it decides, and the caller journals and
    /// starts turns. [`Msg::routing`] picks offer or broadcast, so there is no
    /// table here to keep in step with the message type.
    fn consult(state: &AgentState, msg: &Msg<'_>) -> Option<Performed> {
        let decision = match msg.routing() {
            capabilities::Routing::Offer => state.capabilities.offer(msg)?,
            capabilities::Routing::Broadcast => state.capabilities.broadcast(msg),
        };
        // Nothing is in flight: a lifecycle message is news about something
        // that already happened, and no run is waiting on it.
        Some(Self::performed(decision, None, &msg.describe()))
    }

    /// Give a command to the capability that owns it, and work out what it
    /// asked for.
    ///
    /// Separate from [`Self::consult`] because only this one has a run waiting:
    /// a command names the call it came from, which is what an answer is paired
    /// against.
    fn consult_command(state: &AgentState, cmd: &capabilities::CapCommand) -> Option<Performed> {
        let decision = state.capabilities.dispatch(cmd)?;
        Some(Self::performed(decision, cmd.call(), cmd.owner()))
    }

    /// Ask what this turn's compaction budget should be, before its run
    /// exists to read one.
    ///
    /// [`Msg::TurnProposed`] is broadcast rather than offered — same as a turn
    /// boundary — because nothing is being answered to; the actor merely
    /// collects an opinion. Only the token budget capability has one today, and
    /// its whole answer is config it was equipped with, never anything read off
    /// `state`, which is why `state` here is only ever the capability list. A
    /// runner that equipped none gets `None` back, and its runs never compact —
    /// see [`Act::CompactionBudget`].
    ///
    /// Events are discarded rather than journaled, the same as a load's: a
    /// policy-only capability owns no state, so one producing an event here
    /// would be a bug worth seeing rather than a fact worth keeping.
    fn propose_turn(state: &AgentState, ctx: &ActorContext<AgentCommand>) -> Option<(u32, u32)> {
        let performed = Self::consult(state, &Msg::TurnProposed)?;
        if !performed.events.is_empty() {
            tracing::error!("a capability journaled something on a turn proposal; discarded");
        }
        debug_assert!(
            performed.answer.is_none()
                && performed.resume.is_empty()
                && performed.hold.is_empty()
                && performed.conclusion.is_none()
                && performed.asks.is_empty(),
            "a turn proposal is not a tool call and holds nothing open"
        );
        Self::spawn_wakes(performed.wakes, ctx);
        performed.compaction
    }

    /// Turn a decision into what the actor has to do about it.
    ///
    /// `in_flight` is the call a run is waiting on, when one is. `what` names
    /// what was being handled, for the diagnostics.
    fn performed(
        decision: capabilities::Decision,
        in_flight: Option<&str>,
        what: &str,
    ) -> Performed {
        let mut out = Performed {
            events: decision
                .events
                .into_iter()
                .map(AgentDomainEvent::Capability)
                .collect(),
            ..Performed::default()
        };
        for act in decision.acts {
            Self::perform(&mut out, act, in_flight, what);
        }
        out
    }

    /// Send what capabilities asked the session for, off the mailbox.
    ///
    /// One detached task per request: a session busy starting the child must
    /// not block this agent's queue, and a capability that asked for two things
    /// gets two answers rather than one that waited for the other.
    fn dispatch_asks(
        &self,
        asks: Vec<capabilities::SessionRequest>,
        ctx: &ActorContext<AgentCommand>,
    ) {
        for request in asks {
            let parent = Arc::clone(&self.ctx.parent);
            let me = ctx.self_ref();
            tokio::spawn(async move {
                let reply = parent.request(request).await;
                let _ = me.tell(AgentCommand::SessionReplied { reply }).await;
            });
        }
    }

    /// Start the sleeps capabilities asked for, off the mailbox.
    ///
    /// The one thing a capability cannot do for itself, and the whole of what
    /// the actor adds: time passing. Nothing is journaled — the durable fact is
    /// whatever the capability holds, and a load re-issues the wake from it.
    fn spawn_wakes(wakes: Vec<Wake>, ctx: &ActorContext<AgentCommand>) {
        for wake in wakes {
            spawn_wake(ctx.self_ref(), wake);
        }
    }

    /// Journal what capabilities decided, send what they asked for, and start
    /// the turn they resumed, if any.
    ///
    /// The tail every consultation shares except a tool call's, which has a
    /// reply channel to answer as well.
    async fn finish_consult(
        &mut self,
        performed: Performed,
        state: &AgentState,
        ctx: &ActorContext<AgentCommand>,
    ) -> CommandEffect<AgentDomainEvent> {
        let Performed {
            mut events,
            // Never set here: an answer is only kept when it names the call in
            // flight, and a lifecycle message has none. A capability answering
            // one anyway is reported by `answer_call`, where the mismatch is.
            answer: _,
            resume,
            conclusion,
            asks,
            hold,
            wakes,
            compaction,
        } = performed;
        if !hold.is_empty() {
            // Only a turn boundary can be held, and this is not one.
            tracing::error!(
                ?hold,
                "a capability held a boundary that is not a turn ending"
            );
        }
        if conclusion.is_some() {
            tracing::error!("a capability concluded outside a turn");
        }
        if compaction.is_some() {
            // Only `Msg::TurnProposed` is answered with one, and `AgentActor`
            // reads that answer itself through `propose_turn` rather than
            // through this general tail — so a value here means some
            // capability answered a message that was not a turn proposal.
            tracing::error!("a capability proposed a compaction budget outside a turn proposal");
        }
        self.dispatch_asks(asks, ctx);
        Self::spawn_wakes(wakes, ctx);
        // Folded first, so whatever comes next sees what these events closed
        // rather than the state before them.
        let folded = events
            .iter()
            .fold(state.clone(), |s, e| Self::apply_event(s, e.clone()));
        if resume.is_empty() {
            // A capability that queued something has not started anything: an
            // agent parked on a timer stays parked until the queue is
            // reconsidered, and nothing else was going to reconsider it. Silent
            // when it decides against, which is the ordinary case.
            events.extend(self.try_drain(&folded, ctx).await);
        } else {
            let turn = crate::agent_loop::resumed_turn(&folded.inbox, resume);
            events.extend(self.begin_turn(turn, &folded, ctx).await);
        }
        self.persist_maybe_snapshot(events)
    }

    /// Turn one act into events, an answer, or queued work.
    fn perform(out: &mut Performed, act: Act, in_flight: Option<&str>, what: &str) {
        match act {
            Act::Answer { call, text } => {
                Self::answer_call(
                    out,
                    in_flight,
                    what,
                    &call,
                    Ok(ToolOutcome::Result(Value::String(text))),
                );
            }
            Act::Park { call, note } => {
                // Journaled by the actor, because *being* parked governs things
                // no capability can see: whether the queue may start a turn,
                // and which dangling calls recovery must leave alone.
                out.events.push(AgentDomainEvent::ParkedOn {
                    call: call.clone(),
                    note,
                    at_ms: now_ms(),
                });
                Self::answer_call(out, in_flight, what, &call, Ok(ToolOutcome::StopRun));
            }
            // `InvalidInput`, not a plain result: `is_error` is read by
            // agentcore's loop detector and the nudge budget, and a step
            // submitting the same invalid outcome five times is exactly where
            // the difference shows.
            Act::Refuse { call, reason } => Self::answer_call(
                out,
                in_flight,
                what,
                &call,
                Err(horsie_agentcore::ToolCallError::InvalidInput(reason)),
            ),
            Act::Resume { results } => out.resume.extend(results),
            Act::Conclude { output } => out.conclusion = Some(output),
            Act::Hold { note } => out.hold.push(note),
            // Collected rather than spawned here, for the same reason an ask
            // is: `perform` is a pure decision, and a `tokio::spawn` is not.
            Act::Wake { id, after_secs } => out.wakes.push(Wake { id, after_secs }),
            Act::Enqueue { item } => out.events.push(AgentDomainEvent::Received {
                item,
                at_ms: now_ms(),
            }),
            Act::Record(event) => out.events.push(AgentDomainEvent::LifecycleRecorded {
                event: *event,
                at_ms: now_ms(),
            }),
            // Collected rather than sent here: `perform` is a pure decision,
            // and asking the session is I/O that must not happen on the
            // mailbox. The caller sends them once it has journaled.
            Act::Ask(request) => out.asks.push(request),
            Act::CompactionBudget {
                trigger_at_percent,
                retain_percent,
            } => out.compaction = Some((trigger_at_percent, retain_percent)),
        }
    }

    /// Deliver an answer to the call in flight, or complain.
    ///
    /// A capability may answer a call from something that is not that call — a
    /// session reply arriving turns later, say — and there is no run waiting on
    /// it then. Task 5 gives that case a path; here it is a bug worth hearing
    /// about rather than a silent drop.
    fn answer_call(
        out: &mut Performed,
        in_flight: Option<&str>,
        what: &str,
        call: &str,
        answer: Result<ToolOutcome, horsie_agentcore::ToolCallError>,
    ) {
        if in_flight == Some(call) {
            out.answer = Some(answer);
            return;
        }
        tracing::error!(
            call,
            handling = what,
            "a capability answered a tool call that nothing is waiting on"
        );
    }
}

/// Answer the run once the events behind the answer are durable.
///
/// The blanket rule, in the one place that can enforce it: a command that
/// answered first would report success for work a crash loses, and no test
/// could fail for it. So the answer waits on the write's own acknowledgement,
/// and takes its outcome from it — a journal failure reaches the model as the
/// call failing rather than as a success the log does not contain.
///
/// Off the mailbox, because the ack lands after this actor has moved on: the
/// effect carries the channel, and a detached task turns "the write landed"
/// into "here is your result".
fn answer_when_durable(
    effect: CommandEffect<AgentDomainEvent>,
    reply: ReplyTo<Result<ToolOutcome, horsie_agentcore::ToolCallError>>,
    answer: Result<ToolOutcome, horsie_agentcore::ToolCallError>,
) -> CommandEffect<AgentDomainEvent> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let answered = match rx.await {
            Ok(Ok(())) => answer,
            Ok(Err(e)) => Err(horsie_agentcore::ToolCallError::ExecutionFailed(format!(
                "the agent could not journal what this call did: {e}"
            ))),
            // The actor stopped before the write was reported. The call cannot
            // be said to have happened, so it is not answered as though it had.
            Err(_) => Err(horsie_agentcore::ToolCallError::ExecutionFailed(
                "the agent stopped before this call was journaled".to_string(),
            )),
        };
        let _ = reply.send(answered);
    });
    effect.and_ack(ReplyTo::from_sender(tx))
}

/// Captures coarse agent events while forwarding every event to the inner sink.
/// Used only inside [`run_with_retries`] to locate the handoff tool-call id;
/// persistence (with backpressure) happens in the inner [`PersistSink`].
pub(super) struct CapturingSink {
    inner: Arc<dyn EventSink>,
    captured: Mutex<Vec<AgentEvent>>,
}

impl CapturingSink {
    pub(super) fn new(inner: Arc<dyn EventSink>) -> Self {
        Self {
            inner,
            captured: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn take(&self) -> Vec<AgentEvent> {
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
/// ([`AgentCommand::PersistProgress`]), never the journal directly.
///
/// This is the only sink. There used to be a second one forwarding every event
/// to a broadcast so a live stream could accumulate its own copy of the
/// transcript; readers now read the agent's state instead, so the copy — and
/// the ordering problem between it and the original — is gone.
///
/// `InputMessage` is intentionally NOT persisted here: the actor persists the input
/// itself when handling `Run`/`InjectToolResult`, so a turn-restarting retry that
/// re-emits the input can never double-persist it into two consecutive user
/// messages.
struct PersistSink {
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
        // Text chunks go through the same mailbox, unjournaled. `tell` rather
        // than `ask`: nothing durable happens, so there is nothing to wait for
        // — but it still travels the mailbox, which is what keeps a chunk from
        // overtaking the entry it precedes.
        if let AgentEvent::TextChunk(chunk) = &event {
            let _ = self
                .actor
                .tell(AgentCommand::RecordDelta {
                    text: chunk.text.clone(),
                })
                .await;
        }
        Ok(())
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
    // Shared no-op collaborators for tests that only exercise the actor's own
    // bookkeeping and never start a run.
    struct StubContext;
    #[async_trait]
    impl crate::agent_loop::ContextProvider for StubContext {
        async fn provide(
            &self,
        ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError> {
            Err(crate::agent_loop::ContextError::retryable("no context"))
        }
    }
    struct StubParent;
    #[async_trait]
    impl AgentOutcomeSink for StubParent {
        async fn deliver(&self, _: AgentOutcome) {}
    }

    use super::*;
    use crate::agent_loop::AskedQuestion;
    use horsie_agentcore::{AgentLogBody, ContentPart, Role};
    use horsie_models::agent::TextPart;

    fn user_msg(text: &str) -> Message {
        Message {
            created_at_ms: 0,
            started_at_ms: None,
            id: "u".into(),
            role: Role::User,
            parts: vec![ContentPart::Text(TextPart { text: text.into() })],
        }
    }

    pub(super) fn def_fixture() -> AgentRunDef {
        AgentRunDef {
            system_prompt: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        }
    }

    #[test]
    fn from_def_defaults_to_non_interactive() {
        assert!(!AgentParams::from_def(&def_fixture()).interactive);
    }

    /// Only a step owes a result. For everyone else a turn ending with plain
    /// text *is* the answer, and nudging one would be nonsense.
    #[test]
    fn from_def_owes_no_result() {
        assert!(!AgentParams::from_def(&def_fixture()).requires_result);
    }

    fn stopped(calls: &[(&str, serde_json::Value)]) -> Vec<StoppedCall> {
        calls
            .iter()
            .enumerate()
            .map(|(i, (tool, input))| StoppedCall {
                tool: (*tool).to_string(),
                tool_call_id: format!("toolu_{i}"),
                input: input.clone(),
            })
            .collect()
    }

    /// A bare actor, for the decisions that need one but no journal and no run.
    fn bare_actor() -> AgentActor {
        AgentActor::new(
            AgentRuntimeContext {
                context_provider: Arc::new(StubContext),
                revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
                parent: Arc::new(StubParent),
                journal_id: uuid::Uuid::new_v4(),
                ready: true,
            },
            AgentParams::from_def(&def_fixture()),
        )
    }

    /// A turn may park on several calls at once — questions are asked together
    /// and answered together — and the run is parked when every call that
    /// stopped it is one a capability parked on. No tool name is consulted:
    /// which calls those are is already in the agent's own state.
    #[test]
    fn a_run_stopped_only_by_parked_calls_is_parked() {
        let state = AgentState {
            asks: vec![
                AskedQuestion {
                    tool_call_id: Some("toolu_0".into()),
                    question: "first?".into(),
                },
                AskedQuestion {
                    tool_call_id: Some("toolu_1".into()),
                    question: "second?".into(),
                },
            ],
            ..AgentState::default()
        };
        let calls = stopped(&[
            ("ask_user", serde_json::json!({"question": "first?"})),
            ("ask_user", serde_json::json!({"question": "second?"})),
        ]);
        assert!(matches!(
            bare_actor().interpret(&state, calls),
            Conclusion::Parked
        ));
    }

    /// A call that stopped the run and that nothing accounted for — no
    /// capability concluded, no park holds it — has no honest reading, and only
    /// the model can resolve it. Every call is told why and the turn runs again.
    #[test]
    fn stopped_calls_nothing_accounted_for_are_a_contradiction() {
        let calls = stopped(&[
            (SUBMIT_RESULT_TOOL, serde_json::json!({"outcome": "p0"})),
            (SUBMIT_RESULT_TOOL, serde_json::json!({"outcome": "p2"})),
        ]);
        assert!(matches!(
            bare_actor().interpret(&AgentState::default(), calls),
            Conclusion::Contradiction(c) if c.len() == 2
        ));
    }

    /// Without a turn-boundary snapshot an agent that only converses — no ask,
    /// no park, no cancel — never snapshots, and every recovery stays a full
    /// replay of the whole transcript.
    #[test]
    fn a_turn_boundary_snapshots_only_once_enough_events_have_accrued() {
        let session_id = uuid::Uuid::new_v4();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(StubContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(StubParent),
            journal_id: session_id,
            ready: true,
        };
        let mut agent = AgentActor::new(ctx, AgentParams::from_def(&def_fixture()));

        assert!(
            !agent.snapshot_due(),
            "a fresh agent has nothing worth snapshotting"
        );

        agent.events_since_snapshot = SNAPSHOT_EVERY_EVENTS - 1;
        assert!(
            !agent.snapshot_due(),
            "one event short of the interval must not snapshot"
        );

        agent.events_since_snapshot = SNAPSHOT_EVERY_EVENTS;
        assert!(
            agent.snapshot_due(),
            "reaching the interval snapshots at the turn boundary"
        );
        assert_eq!(
            agent.events_since_snapshot, 0,
            "the counter resets on request, so a failed write waits one interval"
        );
        assert!(
            !agent.snapshot_due(),
            "and the very next turn does not snapshot again"
        );
    }

    /// The observer replaces journal replay: it must see every durable event,
    /// after the fold, with the resulting message already in state.
    #[tokio::test]
    async fn an_observer_sees_durable_appends_with_folded_state() {
        use crate::agent_loop::{ContextError, ContextProvider, Contexts};
        use horsie_actor::{ActorSystem, InMemoryJournal, Journal};

        struct NoContext;
        #[async_trait]
        impl ContextProvider for NoContext {
            async fn provide(&self) -> Result<Contexts, ContextError> {
                Err(ContextError::retryable("no context"))
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
                    .push((label, state.log.len()));
            }
        }

        let session_id = uuid::Uuid::new_v4();
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let recorder = Arc::new(Recorder::default());
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(NoContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(DeafParent),
            journal_id: session_id,
            ready: true,
        };
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::with_observer(ctx, AgentParams::from_def(&def_fixture()), recorder.clone()),
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
                ack: ReplyTo::from_sender(ack),
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

    /// The one seam the conversation id can regress at silently. Everything
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
                    system_prompt: None,
                    facts: crate::sessions::runners::loading::AgentFacts::default(),
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
            .tell(AgentCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m1".into(),
                    text: "hi".into(),
                },
                ack: None,
            })
            .await
            .unwrap();

        // `Started` precedes the work and `UsageRecorded` rides alongside the
        // terminal outcome, so read past both until the run itself reports.
        loop {
            match rx.recv().await.expect("the run must report an outcome") {
                AgentOutcome::Started { .. }
                | AgentOutcome::UsageRecorded { .. }
                | AgentOutcome::ForkSummary { .. } => continue,
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

    // --- The pre-run hook seam ---
    //
    // `SessionStart` used to fire inside `provide()`, which runs on the run's
    // own task *after* the history snapshot — so a record journaled there first
    // reached the model on the following turn. These pin the seam that moved it
    // ahead of the snapshot, and the once-per-load bookkeeping that came with
    // it.

    mod start_hooks {
        use super::*;
        use horsie_actor::{ActorRef, ActorSystem, InMemoryJournal, Journal};
        use horsie_agentcore::EmptyToolbox;
        use horsie_agentcore::testkit::MockProvider;
        use horsie_models::hooks::{
            ContextInjected, HookAction, HookBlocked, HookRecord, SessionStartOutcome,
            SessionStartRecord, UserPromptSubmitOutcome, UserPromptSubmitRecord,
        };
        use std::sync::Mutex;

        /// A provider that answers `start_hooks` from a script and records every
        /// `StartTurn` it was asked about.
        struct HookingContext {
            llm: Arc<MockProvider>,
            records: Vec<HookRecord>,
            enabled: bool,
            seen: Mutex<Vec<crate::agent_loop::StartTurn>>,
        }

        impl HookingContext {
            fn new(llm: Arc<MockProvider>, records: Vec<HookRecord>) -> Arc<Self> {
                Arc::new(Self {
                    llm,
                    records,
                    enabled: true,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn disabled(llm: Arc<MockProvider>) -> Arc<Self> {
                Arc::new(Self {
                    llm,
                    records: Vec::new(),
                    enabled: false,
                    seen: Mutex::new(Vec::new()),
                })
            }

            fn sources(&self) -> Vec<Option<String>> {
                self.seen
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|t| t.start_source.as_ref().map(|s| s.as_wire().to_string()))
                    .collect()
            }
        }

        #[async_trait]
        impl crate::agent_loop::ContextProvider for HookingContext {
            async fn provide(
                &self,
            ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError> {
                Ok(crate::agent_loop::Contexts {
                    provider: self.llm.clone(),
                    toolbox: Arc::new(EmptyToolbox),
                    system_prompt: None,
                    facts: crate::sessions::runners::loading::AgentFacts::default(),
                    context_window: None,
                })
            }

            fn has_start_hooks(&self) -> bool {
                self.enabled
            }

            async fn start_hooks(
                &self,
                turn: crate::agent_loop::StartTurn,
            ) -> Result<crate::agent_loop::TurnPreparation, crate::agent_loop::ContextError>
            {
                self.seen.lock().unwrap().push(turn);
                Ok(crate::agent_loop::TurnPreparation {
                    records: self.records.clone(),
                    message: None,
                })
            }
        }

        struct ReportingParent(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
        #[async_trait]
        impl AgentOutcomeSink for ReportingParent {
            async fn deliver(&self, outcome: AgentOutcome) {
                let _ = self.0.send(outcome);
            }
        }

        type Outcomes = tokio::sync::mpsc::UnboundedReceiver<AgentOutcome>;

        fn spawn(provider: Arc<HookingContext>) -> (ActorRef<AgentCommand>, Outcomes) {
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = AgentRuntimeContext {
                context_provider: provider,
                revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
                parent: Arc::new(ReportingParent(tx)),
                journal_id: uuid::Uuid::new_v4(),
                ready: true,
            };
            let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
            let agent = crate::testing::spawn_detached(
                &ActorSystem::new(journal),
                AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
            );
            (agent, rx)
        }

        async fn prompt(agent: &ActorRef<AgentCommand>, text: &str, rx: &mut Outcomes) {
            agent
                .tell(AgentCommand::Enqueue {
                    item: crate::agent_loop::Incoming::User {
                        id: "m2".into(),
                        text: text.into(),
                    },
                    ack: None,
                })
                .await
                .unwrap();
            terminal_outcome(rx).await;
        }

        /// Read past the outcomes that are not how a turn *ended*: `Started`
        /// precedes the work, and `UsageRecorded` rides alongside the terminal
        /// one.
        async fn terminal_outcome(rx: &mut Outcomes) -> AgentOutcome {
            loop {
                match rx.recv().await.expect("the turn must report an outcome") {
                    AgentOutcome::Started { .. }
                    | AgentOutcome::UsageRecorded { .. }
                    | AgentOutcome::ForkSummary { .. } => continue,
                    outcome => return outcome,
                }
            }
        }

        fn session_start(context: &str) -> HookRecord {
            HookRecord {
                plugin: "boot".into(),
                duration_ms: 1,
                halt: None,
                action: HookAction::SessionStart(SessionStartRecord {
                    source: "startup".into(),
                    system_message: None,
                    outcome: SessionStartOutcome::Ran(ContextInjected {
                        additional_context: Some(context.into()),
                    }),
                }),
            }
        }

        /// The regression the whole seam exists to prevent: `provide()` runs
        /// after the run has already snapshotted its history, so a record
        /// journaled there would first appear on turn two — leaving every
        /// session's opening turn unhooked.
        #[tokio::test]
        async fn session_start_context_reaches_the_very_first_prompt() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![session_start("pins node 22")]);
            let (agent, mut rx) = spawn(provider);

            prompt(&agent, "hi", &mut rx).await;

            let first = llm
                .requests()
                .into_iter()
                .next()
                .expect("one provider call");
            assert!(
                first.texts.iter().any(|t| t.contains("pins node 22")),
                "the first prompt must carry the start hook's context, got {:?}",
                first.texts
            );
        }

        /// `SessionStart` fired on every turn before this: `provide()` is
        /// per-run and its call had no guard, so every message re-ran every
        /// start hook and always reported `source: "startup"`.
        #[tokio::test]
        async fn a_second_turn_does_not_fire_the_start_hook_again() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![session_start("pins node 22")]);
            let (agent, mut rx) = spawn(provider.clone());

            prompt(&agent, "hi", &mut rx).await;
            prompt(&agent, "again", &mut rx).await;

            assert_eq!(
                provider.sources(),
                vec![Some("startup".to_string()), None],
                "the start hook is due once per load; the prompt hook every turn"
            );
        }

        /// A rehydrated agent is a `resume`, and it is the only other lifecycle
        /// transition horsie has. Detected from the transcript rather than a
        /// framework flag: a fresh agent has nothing in it.
        #[tokio::test]
        async fn an_agent_with_recovered_history_reports_source_resume() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(llm.clone(), vec![]);
            let (agent, mut rx) = spawn(provider.clone());
            // Stand in for a recovered load: a transcript that predates this
            // actor's first command, which is exactly what folding a journal
            // leaves behind.
            let (ack, done) = tokio::sync::oneshot::channel();
            agent
                .tell(AgentCommand::PersistProgress {
                    events: vec![AgentDomainEvent::InputMessage {
                        message: user_msg("from a previous load"),
                    }],
                    ack: ReplyTo::from_sender(ack),
                })
                .await
                .unwrap();
            done.await.unwrap().unwrap();

            prompt(&agent, "carry on", &mut rx).await;

            assert_eq!(
                provider.sources(),
                vec![Some("resume".to_string())],
                "a transcript that predates this load means the agent was recovered"
            );
        }

        /// A blocked prompt never becomes a turn: nothing is journaled as input
        /// and no run starts. The record still lands, so the user can see which
        /// plugin refused it.
        #[tokio::test]
        async fn a_blocked_prompt_journals_no_input_and_starts_no_run() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::new(
                llm.clone(),
                vec![HookRecord {
                    plugin: "guard".into(),
                    duration_ms: 1,
                    halt: None,
                    action: HookAction::UserPromptSubmit(UserPromptSubmitRecord {
                        system_message: None,
                        outcome: UserPromptSubmitOutcome::Blocked(HookBlocked {
                            reason: Some("secrets in the prompt".into()),
                        }),
                    }),
                }],
            );
            let (agent, mut rx) = spawn(provider);

            agent
                .tell(AgentCommand::Enqueue {
                    item: crate::agent_loop::Incoming::User {
                        id: "m3".into(),
                        text: "my password is hunter2".into(),
                    },
                    ack: None,
                })
                .await
                .unwrap();

            match terminal_outcome(&mut rx).await {
                AgentOutcome::Failed { error, .. } => {
                    assert_eq!(error, "secrets in the prompt");
                }
                other => panic!("expected the turn to be refused, got {other:?}"),
            }
            assert_eq!(llm.calls(), 0, "the model must never be reached");

            let page = agent
                .ask(|reply| AgentCommand::PageLog {
                    before: None,
                    max: 50,
                    reply,
                })
                .await
                .unwrap();
            // The queued message, the turn that took it, and the record that
            // refused it — but no input message, because no run began.
            assert!(
                !page
                    .entries
                    .iter()
                    .any(|e| matches!(e.body, AgentLogBody::Llm(_))),
                "a refused prompt must never reach the transcript: {:?}",
                page.entries
            );
            assert!(
                page.entries
                    .iter()
                    .any(|e| matches!(e.body, AgentLogBody::Hook(_))),
                "the refusal is auditable: {:?}",
                page.entries
            );
        }

        /// A preparation failure must classify itself exactly as the same
        /// failure out of `provide` would. Flattening `terminal` here leaves a
        /// session whose sandbox is gone for good reporting a retryable error,
        /// so it never reaches `Unrecoverable` and invites the user to try
        /// again forever.
        #[tokio::test]
        async fn a_terminal_preparation_failure_stays_terminal() {
            struct GoneContext;
            #[async_trait]
            impl crate::agent_loop::ContextProvider for GoneContext {
                async fn provide(
                    &self,
                ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError>
                {
                    Err(crate::agent_loop::ContextError::terminal("runtime is gone"))
                }
                fn has_start_hooks(&self) -> bool {
                    true
                }
                async fn start_hooks(
                    &self,
                    _: crate::agent_loop::StartTurn,
                ) -> Result<crate::agent_loop::TurnPreparation, crate::agent_loop::ContextError>
                {
                    Err(crate::agent_loop::ContextError::terminal("runtime is gone"))
                }
            }

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = AgentRuntimeContext {
                context_provider: Arc::new(GoneContext),
                revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
                parent: Arc::new(ReportingParent(tx)),
                journal_id: uuid::Uuid::new_v4(),
                ready: true,
            };
            let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
            let agent = crate::testing::spawn_detached(
                &ActorSystem::new(journal),
                AgentActor::new(ctx, AgentParams::from_def(&def_fixture())),
            );
            agent
                .tell(AgentCommand::Enqueue {
                    item: crate::agent_loop::Incoming::User {
                        id: "m4".into(),
                        text: "hi".into(),
                    },
                    ack: None,
                })
                .await
                .unwrap();

            match terminal_outcome(&mut rx).await {
                AgentOutcome::Failed { terminal, .. } => {
                    assert!(terminal, "a gone sandbox is terminal wherever it surfaces");
                }
                other => panic!("expected the turn to fail, got {other:?}"),
            }
        }

        /// A session with no plugins pays nothing for a seam it cannot use.
        #[tokio::test]
        async fn a_provider_without_start_hooks_makes_no_prepare_round_trip() {
            let llm = MockProvider::text("done");
            let provider = HookingContext::disabled(llm.clone());
            let (agent, mut rx) = spawn(provider.clone());

            prompt(&agent, "hi", &mut rx).await;

            assert!(
                provider.sources().is_empty(),
                "`has_start_hooks() == false` must skip the round-trip entirely"
            );
            assert_eq!(llm.calls(), 1, "the turn still runs");
        }
    }
}

/// The run-id fence: a report can only speak for the run it came from.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod fence_tests {
    use super::*;
    use crate::agent_loop::context::{ContextError, ContextProvider, Contexts};
    use horsie_actor::{ActorSystem, InMemoryJournal};
    use horsie_agentcore::{AgentLogBody, ContentPart, Role};

    struct HangingProvider;
    #[async_trait]
    impl ContextProvider for HangingProvider {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            std::future::pending().await
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
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: uuid::Uuid::new_v4(),
            ready: true,
        };
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.interactive = true;
        let journal = Arc::new(InMemoryJournal::new());
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );

        // Run 0 starts and hangs in `provide`, so it is genuinely in flight.
        agent
            .tell(AgentCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m5".into(),
                    text: "first".into(),
                },
                ack: None,
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
                fork_summary: None,
            })))
            .await
            .unwrap();

        // Run 0 is still in flight, so a second turn is refused — the fence
        // held. Without it, `running` would have been cleared and this would
        // start a second background loop against the same journal.
        agent
            .tell(AgentCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m6".into(),
                    text: "second".into(),
                },
                ack: None,
            })
            .await
            .unwrap();

        let (reply, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::PageLog {
                before: None,
                max: 50,
                reply: ReplyTo::from_sender(reply),
            })
            .await
            .unwrap();
        let page = rx.await.unwrap();
        // The second message is *queued* — that much is its whole point — but
        // no second turn took it: one `TurnBegan`, one input message. Without
        // the fence, the stale report would have cleared `running` and the
        // second message would have started a run against the same journal.
        let began = page
            .entries
            .iter()
            .filter(|e| {
                matches!(
                    e.body,
                    AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(_))
                )
            })
            .count();
        assert_eq!(
            began, 1,
            "the refused turn must not begin: {:?}",
            page.entries
        );
        assert!(
            outcomes
                .try_recv()
                .is_ok_and(|o| matches!(o, AgentOutcome::Started { .. })),
            "the first turn's own start, and nothing from the superseded run"
        );
        assert!(
            outcomes.try_recv().is_err(),
            "a superseded run's outcome must not reach the parent"
        );
    }

    /// Stopping a turn keeps what it had already written.
    ///
    /// Streamed text lives only in the deltas — unjournaled by design, since a
    /// finished message supersedes them within the second — and a cancelled
    /// call never produces that finished message. The boundary entry the stop
    /// appends then cleared them, so twenty-two minutes of generation ended
    /// with a transcript showing no sign a turn had run.
    #[tokio::test]
    async fn a_stopped_turn_keeps_the_text_it_had_already_written() {
        let (tx, _outcomes) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingProvider),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: uuid::Uuid::new_v4(),
            ready: true,
        };
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.interactive = true;
        let journal = Arc::new(InMemoryJournal::new());
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );

        agent
            .tell(AgentCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: "m1".into(),
                    text: "write me an essay".into(),
                },
                ack: None,
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The same road a streamed chunk takes: the sink tells the actor.
        for chunk in ["Once upon ", "a time"] {
            agent
                .tell(AgentCommand::RecordDelta {
                    text: chunk.to_string(),
                })
                .await
                .unwrap();
        }

        let (ack, cancelled) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Cancel {
                ack: Some(ReplyTo::from_sender(ack)),
            })
            .await
            .unwrap();
        cancelled.await.unwrap();

        let (reply, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::PageLog {
                before: None,
                max: 50,
                reply: ReplyTo::from_sender(reply),
            })
            .await
            .unwrap();
        let page = rx.await.unwrap();
        let kept: Vec<String> = page
            .entries
            .iter()
            .filter_map(|e| {
                let AgentLogBody::Llm(m) = &e.body else {
                    return None;
                };
                if m.role != Role::Assistant {
                    return None;
                }
                let text: String = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(t.text.clone()),
                        ContentPart::Thinking(_)
                        | ContentPart::ToolCall(_)
                        | ContentPart::ToolResult(_)
                        | ContentPart::SubAgentResult(_) => None,
                    })
                    .collect();
                (!text.is_empty()).then_some(text)
            })
            .collect();
        assert_eq!(
            kept,
            vec!["Once upon a time"],
            "the stopped turn's generation is gone: {:?}",
            page.entries
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod queue_tests {
    //! The queue as the agent actually runs it: what a not-ready agent does
    //! with a message, what a boundary drains, and what an answer resumes.
    //!
    //! The *rule* is pure and tested in [`crate::agent_loop::inbox`]. These are about the
    //! actor around it — the gates it holds, and the events it journals.
    use super::*;
    use crate::agent_loop::context::{ContextError, ContextProvider, Contexts};
    use horsie_actor::{ActorSystem, InMemoryJournal, Journal};
    use horsie_agentcore::testkit::MockProvider;
    use horsie_agentcore::{AgentLogBody, LlmProvider};

    struct OutcomeChannel(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
    #[async_trait]
    impl AgentOutcomeSink for OutcomeChannel {
        async fn deliver(&self, outcome: AgentOutcome) {
            let _ = self.0.send(outcome);
        }
    }

    /// Hands the agent a provider that always ends the turn with plain text.
    struct TextContext(Arc<dyn LlmProvider>);
    #[async_trait]
    impl ContextProvider for TextContext {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            Ok(Contexts {
                provider: self.0.clone(),
                toolbox: Arc::new(horsie_agentcore::ToolboxImpl::new()),
                system_prompt: None,
                facts: crate::sessions::runners::loading::AgentFacts::default(),
                context_window: None,
            })
        }
    }

    /// A context that never returns, so a run stays genuinely in flight.
    struct HangingContext;
    #[async_trait]
    impl ContextProvider for HangingContext {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            std::future::pending().await
        }
    }

    type Outcomes = tokio::sync::mpsc::UnboundedReceiver<AgentOutcome>;

    fn spawn_with(
        provider: Arc<dyn ContextProvider>,
        ready: bool,
    ) -> (ActorRef<AgentCommand>, Outcomes) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: provider,
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: uuid::Uuid::new_v4(),
            ready,
        };
        let mut params = AgentParams::from_def(&AgentRunDef::default());
        params.interactive = true;
        let journal: Arc<dyn Journal> = Arc::new(InMemoryJournal::new());
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );
        (agent, rx)
    }

    fn text_agent(ready: bool) -> (ActorRef<AgentCommand>, Outcomes) {
        spawn_with(Arc::new(TextContext(MockProvider::text("done"))), ready)
    }

    /// Exactly what a session sends when its sandbox lands or goes away: the
    /// same `Runtime` record a reader sees in the log, and nothing else.
    async fn set_ready(agent: &ActorRef<AgentCommand>, ready: bool) {
        let status = match ready {
            true => horsie_agentcore::RuntimeStatus::Ready(horsie_agentcore::EmptyOutcome {}),
            false => horsie_agentcore::RuntimeStatus::Acquiring(horsie_agentcore::EmptyOutcome {}),
        };
        agent
            .tell(AgentCommand::RecordLifecycle {
                event: LifecycleEvent::Runtime(horsie_agentcore::RuntimeLifecycle {
                    status,
                    detail: None,
                }),
                at_ms: 0,
            })
            .await
            .unwrap();
    }

    async fn send(agent: &ActorRef<AgentCommand>, id: &str, text: &str) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Enqueue {
                item: crate::agent_loop::Incoming::User {
                    id: id.into(),
                    text: text.into(),
                },
                ack: Some(ReplyTo::from_sender(tx)),
            })
            .await
            .unwrap();
        rx.await.unwrap().expect("the message must be durable");
    }

    /// Every lifecycle entry kind in the agent's log, in order.
    async fn lifecycle(agent: &ActorRef<AgentCommand>) -> Vec<String> {
        let page = agent
            .ask(|reply| AgentCommand::PageLog {
                before: None,
                max: 100,
                reply,
            })
            .await
            .unwrap();
        page.entries
            .iter()
            .filter_map(|e| match &e.body {
                AgentLogBody::Lifecycle(LifecycleEvent::MessageQueued(_)) => {
                    Some("MessageQueued".to_string())
                }
                AgentLogBody::Lifecycle(LifecycleEvent::TurnBegan(_)) => {
                    Some("TurnBegan".to_string())
                }
                AgentLogBody::Lifecycle(LifecycleEvent::AskRecorded(_)) => {
                    Some("AskRecorded".to_string())
                }
                AgentLogBody::Llm(_)
                | AgentLogBody::Hook(_)
                | AgentLogBody::Lifecycle(_)
                | AgentLogBody::Compaction(_) => None,
            })
            .collect()
    }

    /// Wait for `pred` to hold of the agent's lifecycle entries.
    async fn wait_lifecycle(
        agent: &ActorRef<AgentCommand>,
        what: &str,
        pred: impl Fn(&[String]) -> bool,
    ) {
        for _ in 0..200 {
            let kinds = lifecycle(agent).await;
            if pred(&kinds) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!(
            "{what} not reached within 2s; entries: {:?}",
            lifecycle(agent).await
        );
    }

    /// The ack is the promise. It resolves only once the message is written, so
    /// a caller holding it holds something that survives a crash.
    #[tokio::test]
    async fn a_message_is_acked_only_once_it_is_durable() {
        let (agent, _rx) = text_agent(true);
        // `send` awaits the ack, so by the time it returns the write has
        // happened — and the entry is already there to read.
        send(&agent, "m1", "hello").await;
        assert_eq!(
            lifecycle(&agent).await.first().map(String::as_str),
            Some("MessageQueued"),
            "the ack lands after the write, not before it"
        );
    }

    /// The one gate an agent cannot answer for itself. A message under a
    /// session still building its runtime waits — the whole of the fix for a
    /// first turn outrunning its own create — and the readiness that arrives
    /// when the create lands is what releases it.
    #[tokio::test]
    async fn a_message_waits_for_readiness_and_the_flip_releases_it() {
        let (agent, _rx) = text_agent(false);
        send(&agent, "m1", "hello").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            lifecycle(&agent).await,
            vec!["MessageQueued".to_string()],
            "a message with nowhere to run must not begin a turn"
        );

        set_ready(&agent, true).await;
        wait_lifecycle(&agent, "the released turn", |k| {
            k.contains(&"TurnBegan".to_string())
        })
        .await;
    }

    /// Losing readiness starts nothing; it only stops the next drain.
    #[tokio::test]
    async fn losing_readiness_starts_nothing() {
        let (agent, _rx) = text_agent(true);
        set_ready(&agent, false).await;
        send(&agent, "m1", "hello").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(lifecycle(&agent).await, vec!["MessageQueued".to_string()]);
    }

    /// A run in flight is not a reason to refuse a message — it is a reason to
    /// hold it. Two arrive under one hanging run and neither starts a second.
    #[tokio::test]
    async fn messages_arriving_mid_run_queue_rather_than_starting_a_second_turn() {
        let (agent, _rx) = spawn_with(Arc::new(HangingContext), true);
        send(&agent, "m1", "one").await;
        // The first drains immediately and hangs inside `provide`.
        wait_lifecycle(&agent, "the first turn", |k| {
            k.contains(&"TurnBegan".to_string())
        })
        .await;
        send(&agent, "m2", "two").await;
        send(&agent, "m3", "three").await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let kinds = lifecycle(&agent).await;
        assert_eq!(
            kinds.iter().filter(|k| *k == "TurnBegan").count(),
            1,
            "a run in flight must never be drained into a second one: {kinds:?}"
        );
        assert_eq!(kinds.iter().filter(|k| *k == "MessageQueued").count(), 3);
    }

    /// `Started` precedes the work and is how the owner learns a turn began at
    /// all — it is no longer the thing that began it.
    #[tokio::test]
    async fn the_owner_is_told_the_turn_began_before_it_runs() {
        let (agent, mut rx) = spawn_with(Arc::new(HangingContext), true);
        send(&agent, "m1", "one").await;
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("the owner must be told")
            .expect("an outcome");
        assert!(
            matches!(first, AgentOutcome::Started { .. }),
            "the first report of a turn is that it started, got {first:?}"
        );
    }

    /// Answering is refused unless it covers the park exactly, and the refusal
    /// journals nothing — which is what makes retrying it free.
    #[tokio::test]
    async fn a_partial_answer_is_refused_and_journals_nothing() {
        let (agent, _rx) = text_agent(true);
        let (tx, rx) = tokio::sync::oneshot::channel();
        agent
            .tell(AgentCommand::Answer {
                answers: vec![crate::agent_loop::AskAnswer {
                    tool_call_id: "call-1".into(),
                    text: "main".into(),
                }],
                reply: ReplyTo::from_sender(tx),
            })
            .await
            .unwrap();
        assert_eq!(
            rx.await.unwrap(),
            Err(crate::agent_loop::AnswerError::NothingPending)
        );
        assert!(lifecycle(&agent).await.is_empty());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod interruption_tests {
    //! What an agent says about the turn its process died inside.
    //!
    //! The fact lives here and nowhere else. An owner holds a *status*, which
    //! cannot say which turn produced it — so recovery used to ask "is the
    //! session running?" and got yes about a turn that had begun since. These
    //! are about the agent answering for itself instead.
    use super::*;
    use crate::agent_loop::context::{ContextError, ContextProvider, Contexts};
    use horsie_actor::{ActorSystem, InMemoryJournal, Journal};
    use horsie_agentcore::Usage;

    struct OutcomeChannel(tokio::sync::mpsc::UnboundedSender<AgentOutcome>);
    #[async_trait]
    impl AgentOutcomeSink for OutcomeChannel {
        async fn deliver(&self, outcome: AgentOutcome) {
            let _ = self.0.send(outcome);
        }
    }

    /// A session that records what its agent asked for and answers nothing.
    ///
    /// Never answering is the point: a re-ask is judged by what the session
    /// *hears*, and an answer would fold the request away and hide whether it
    /// was ever sent.
    struct RequestChannel(
        tokio::sync::mpsc::UnboundedSender<crate::agent_loop::capabilities::SessionRequest>,
    );
    #[async_trait]
    impl AgentOutcomeSink for RequestChannel {
        async fn deliver(&self, _outcome: AgentOutcome) {}

        async fn request(
            &self,
            request: crate::agent_loop::capabilities::SessionRequest,
        ) -> crate::agent_loop::capabilities::SessionReply {
            let _ = self.0.send(request);
            std::future::pending().await
        }
    }

    /// Never asked: these agents recover and report, they do not run.
    struct HangingContext;
    #[async_trait]
    impl ContextProvider for HangingContext {
        async fn provide(&self) -> Result<Contexts, ContextError> {
            std::future::pending().await
        }
    }

    /// Spawn an agent over a journal that already holds `events`, and hand back
    /// whatever it reports while recovering.
    async fn recover_with(
        events: &[AgentDomainEvent],
    ) -> tokio::sync::mpsc::UnboundedReceiver<AgentOutcome> {
        let id = uuid::Uuid::new_v4();
        let journal = Arc::new(InMemoryJournal::new());
        let encoded: Vec<Vec<u8>> = events
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap())
            .collect();
        journal
            .persist(&AgentActor::persistence_id_for(id), &encoded, 0)
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(OutcomeChannel(tx)),
            journal_id: id,
            ready: true,
        };
        let mut params =
            AgentParams::from_def(&crate::agent_loop::agent_actor::tests::def_fixture());
        // Every agent a session spawns is interactive, so this is the only
        // configuration that matters — and it is the one that returns from
        // `on_recovery_complete` early, so the report has to precede that.
        params.interactive = true;
        let _agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );
        // Recovery runs before the first command, so anything reported is
        // already on its way by the time the spawn returns.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        rx
    }

    /// The same, for an agent whose owner records requests instead of outcomes.
    async fn recover_asking(
        events: &[AgentDomainEvent],
    ) -> tokio::sync::mpsc::UnboundedReceiver<crate::agent_loop::capabilities::SessionRequest> {
        let id = uuid::Uuid::new_v4();
        let journal = Arc::new(InMemoryJournal::new());
        let encoded: Vec<Vec<u8>> = events
            .iter()
            .map(|e| serde_json::to_vec(e).unwrap())
            .collect();
        journal
            .persist(&AgentActor::persistence_id_for(id), &encoded, 0)
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(HangingContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(RequestChannel(tx)),
            journal_id: id,
            ready: true,
        };
        let mut params =
            AgentParams::from_def(&crate::agent_loop::agent_actor::tests::def_fixture());
        params.interactive = true;
        let _agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        rx
    }

    fn began() -> AgentDomainEvent {
        AgentDomainEvent::TurnBegan {
            consumed: Vec::new(),
            answered: Vec::new(),
            at_ms: 0,
        }
    }

    /// **The re-ask is wired, or none of the capability work matters.**
    ///
    /// A journal cut between `Requested` and the session's answer, loaded from
    /// scratch: the actor has to broadcast the load and send what comes back.
    /// Without this test the whole mechanism could be unreachable and every
    /// capability's own test would still pass — the capability decides, and
    /// only the actor can act on it.
    #[tokio::test]
    async fn a_request_the_session_never_answered_is_re_asked_when_the_agent_loads() {
        use crate::agent_loop::capabilities::{CapEvent, Capabilities, SessionRequest, sub_agent};
        use crate::sessions::runners::action::RunnerArgs;

        let pending = sub_agent::Pending {
            child: crate::sessions::runners::ids::RunnerId::new_v4(),
            agent: crate::sessions::runners::ids::AgentId::new_v4(),
            label: "research".into(),
            task: "dig into the flake".into(),
            agent_type: None,
        };
        let mut requests = recover_asking(&[
            AgentDomainEvent::Equipped {
                capabilities: Capabilities::new(vec![Box::new(
                    sub_agent::SubAgentCapability::new(
                        crate::sessions::runners::empty_settings(),
                        0,
                    ),
                )]),
                at_ms: 0,
            },
            AgentDomainEvent::Capability(CapEvent::SubAgent(sub_agent::Event::Requested {
                call: "call-1".into(),
                pending: pending.clone(),
            })),
            // The park the spawn made. What the dead process left behind is a
            // dangling `tool_use` and a request nobody may have heard.
            AgentDomainEvent::ParkedOn {
                call: "call-1".into(),
                note: "spawning subagent research".into(),
                at_ms: 1,
            },
        ])
        .await;

        let Ok(SessionRequest::StartRunner { call, id, args, .. }) = requests.try_recv() else {
            panic!("the load re-asked for nothing; the model is parked for ever");
        };
        assert_eq!(call, "call-1", "the answer would reach nobody");
        assert_eq!(id, pending.child);
        let RunnerArgs::SubAgent { agent, .. } = args.as_ref() else {
            panic!("expected subagent args, got {args:?}");
        };
        assert_eq!(
            *agent, pending.agent,
            "a re-ask with a fresh worker id is a second child"
        );
        assert!(
            requests.try_recv().is_err(),
            "one dangling request, one re-ask"
        );
    }

    /// A journal ending at `TurnBegan` is what a process killed mid-run leaves
    /// behind, and the agent is the only thing that can say so: its owner sees
    /// a status, and a status cannot name a turn.
    #[tokio::test]
    async fn a_turn_the_process_died_in_is_reported_at_recovery() {
        let mut outcomes = recover_with(&[began()]).await;
        assert!(
            matches!(outcomes.try_recv(), Ok(AgentOutcome::Interrupted { .. })),
            "an agent recovering mid-turn must tell its owner the turn is over"
        );
    }

    /// The other half. A turn that reached a boundary under its own power is
    /// not an interruption, and reporting one would end a turn that had already
    /// ended properly.
    #[tokio::test]
    async fn a_turn_that_reached_a_boundary_is_not_reported() {
        let mut outcomes = recover_with(&[
            began(),
            AgentDomainEvent::RunComplete {
                usage: Usage::without_cache(1, 1),
                iterations: 1,
                context_tokens: 0,
                at_ms: 1,
            },
        ])
        .await;
        assert!(
            outcomes.try_recv().is_err(),
            "a completed turn is not an interruption"
        );
    }

    /// A park is a boundary too: the agent is waiting for an answer, not
    /// stranded mid-run. Reporting it would move the session off
    /// `AwaitingInput` and lose the question.
    #[tokio::test]
    async fn a_parked_turn_is_not_reported() {
        let mut outcomes = recover_with(&[
            began(),
            AgentDomainEvent::ParkedOn {
                call: "call-1".into(),
                note: "which one?".into(),
                at_ms: 1,
            },
        ])
        .await;
        assert!(
            outcomes.try_recv().is_err(),
            "an agent parked on a question has no interrupted turn to report"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod capability_tests {
    use super::*;
    use crate::agent_loop::capabilities::testing::{FakeCapability, call};
    use crate::agent_loop::capabilities::{CapEvent, Capabilities};
    use horsie_actor::{ActorSystem, InMemoryJournal};
    use horsie_agentcore::{ToolCallError, ToolSpec};

    fn equipped(tool: &str) -> AgentState {
        AgentState {
            capabilities: Capabilities::new(vec![Box::new(FakeCapability::new(tool))]),
            ..AgentState::default()
        }
    }

    /// A command comes back as its capability's events, wrapped so the agent's
    /// journal stays a list of things the actor did with one arm for things its
    /// capabilities did.
    #[test]
    fn a_command_journals_the_capabilitys_own_events() {
        let state = equipped("fake_tool");
        let performed = AgentActor::consult_command(&state, &call("fake_tool"))
            .expect("the capability owns its own command");
        assert_eq!(performed.events.len(), 1);
        assert!(matches!(
            performed.events.first(),
            Some(AgentDomainEvent::Capability(CapEvent::Fake(_)))
        ));
    }

    /// A command whose capability is not equipped is `None`, which the command
    /// handler turns into an error the model can see. A silent drop would hang
    /// the call for ever.
    #[test]
    fn a_command_nobody_owns_is_none() {
        let state = equipped("fake_tool");
        assert!(AgentActor::consult_command(&state, &call("bash")).is_none());
    }

    /// The fold reaches the capability, and replaying lands exactly where the
    /// live fold did — which is the whole of how a recovered agent comes back
    /// with the capabilities it had.
    #[test]
    fn folding_a_capability_event_reaches_it_and_replays_identically() {
        let state = equipped("fake_tool");
        let event = AgentDomainEvent::Capability(CapEvent::Fake(
            crate::agent_loop::capabilities::testing::FakeEvent {
                tool: "fake_tool".into(),
                what: "tool:fake_tool".into(),
            },
        ));

        let seen = |s: &AgentState| {
            serde_json::to_string(&s.capabilities).expect("capabilities serialise")
        };
        let live = AgentActor::apply_event(state.clone(), event.clone());
        let recovered = AgentActor::apply_event(state, event);

        assert!(
            seen(&live).contains("tool:fake_tool"),
            "the fold never reached the capability, so a recovered agent would \
             come back with its capabilities blank"
        );
        assert_eq!(
            seen(&live),
            seen(&recovered),
            "replaying the journal must land where the live fold did"
        );
    }

    /// **A refusal reaches the model as an error, not as a result.** `is_error`
    /// is read by agentcore's loop detector and the nudge budget, and a model
    /// repeating one bad call is exactly the case they exist for — so the
    /// distinction has to survive the whole path from `Act::Refuse` to the
    /// answer the run gets.
    #[test]
    fn a_refusal_answers_the_call_with_an_error_and_an_answer_with_a_result() {
        use crate::agent_loop::capabilities::task_list::{Command, TaskListCapability};
        use crate::agent_loop::capabilities::{Answering, CapCommand};

        let state = AgentState {
            capabilities: Capabilities::new(vec![Box::new(TaskListCapability::new())]),
            ..AgentState::default()
        };
        let commanded = |input: serde_json::Value| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            drop(rx);
            AgentActor::consult_command(
                &state,
                &CapCommand::TaskList(
                    Command::Change { input },
                    Answering {
                        call: "t1".to_string(),
                        reply: ReplyTo::from_sender(tx),
                    },
                ),
            )
            .expect("the task list owns its command")
            .answer
        };

        let refused = commanded(serde_json::json!({"action": "delete_everything"}));
        assert!(
            matches!(
                refused,
                Some(Err(horsie_agentcore::ToolCallError::InvalidInput(_)))
            ),
            "a refusal reached the model as an ordinary result: {refused:?}"
        );
        let answered = commanded(serde_json::json!({"action": "create", "tasks": ["a"]}));
        assert!(
            matches!(answered, Some(Ok(ToolOutcome::Result(_)))),
            "a successful call did not answer with a result: {answered:?}"
        );
    }

    /// **The answer waits for the write.** A tool a capability answers for
    /// journals what it did, and the model is told only once that is durable —
    /// otherwise a crash in the window leaves a run that was told a thing
    /// happened and a log that never recorded it.
    #[tokio::test]
    async fn a_claimed_call_is_answered_only_after_its_events_are_durable() {
        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let journal = Arc::new(SlowJournal {
            inner: InMemoryJournal::new(),
            order: Arc::clone(&order),
        });
        let ctx = AgentRuntimeContext {
            context_provider: Arc::new(NoContext),
            revision: std::sync::Arc::new(tokio::sync::watch::Sender::new(0)),
            parent: Arc::new(NoParent),
            journal_id: uuid::Uuid::new_v4(),
            ready: true,
        };
        let mut params = AgentParams::from_def(&AgentRunDef {
            system_prompt: None,
            max_iterations: None,
            max_retries: None,
            allowed_tools: None,
        });
        params.capabilities = Capabilities::new(vec![Box::new(FakeCapability::new("fake_tool"))]);
        let agent = crate::testing::spawn_detached(
            &ActorSystem::new(journal),
            AgentActor::new(ctx, params),
        );
        // The equipment is journaled on recovery, so wait for it: a call that
        // arrives first reaches an agent with no capabilities at all.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let outcome = agent
            .ask(|reply| {
                AgentCommand::Capability(capabilities::CapCommand::Fake(
                    capabilities::testing::FakeCommand {
                        tool: "fake_tool".to_string(),
                    },
                    capabilities::Answering {
                        call: "t1".to_string(),
                        reply,
                    },
                ))
            })
            .await
            .expect("the mailbox answers");
        order.lock().unwrap().push("answered");
        assert!(outcome.is_ok(), "the call was refused: {outcome:?}");
        assert_eq!(
            *order.lock().unwrap(),
            vec!["persisted", "answered"],
            "the model was told before the capability's own events were durable"
        );
    }

    /// A journal that takes its time, so "before the write" and "after the
    /// write" are tellable apart rather than a race.
    struct SlowJournal {
        inner: InMemoryJournal,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl SlowJournal {
        /// Whether this batch is the capability's own event, rather than the
        /// `Equipped` record that precedes it.
        fn is_the_capabilitys(events: &[Vec<u8>]) -> bool {
            events
                .iter()
                .any(|e| String::from_utf8_lossy(e).contains("tool:fake_tool"))
        }
    }

    #[async_trait]
    impl horsie_actor::Journal for SlowJournal {
        async fn persist(
            &self,
            pid: &PersistenceId,
            events: &[Vec<u8>],
            expected_last_seq: u64,
        ) -> horsie_actor::JournalResult<()> {
            let mine = Self::is_the_capabilitys(events);
            if mine {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let out = self.inner.persist(pid, events, expected_last_seq).await;
            if mine {
                self.order.lock().unwrap().push("persisted");
            }
            out
        }

        #[expect(
            clippy::disallowed_methods,
            reason = "this decorator has to hand the inner journal's replay straight back"
        )]
        async fn replay(
            &self,
            pid: &PersistenceId,
            after_seq: u64,
        ) -> futures_util::stream::BoxStream<'_, horsie_actor::JournalResult<(u64, Vec<u8>)>>
        {
            self.inner.replay(pid, after_seq).await
        }

        async fn save_snapshot(
            &self,
            pid: &PersistenceId,
            state: Vec<u8>,
            seq_nr: u64,
        ) -> horsie_actor::JournalResult<()> {
            self.inner.save_snapshot(pid, state, seq_nr).await
        }

        async fn latest_snapshot(
            &self,
            pid: &PersistenceId,
        ) -> horsie_actor::JournalResult<Option<(Vec<u8>, u64)>> {
            self.inner.latest_snapshot(pid).await
        }

        async fn delete_events_before(
            &self,
            pid: &PersistenceId,
            seq_nr: u64,
        ) -> horsie_actor::JournalResult<()> {
            self.inner.delete_events_before(pid, seq_nr).await
        }

        async fn copy_snapshot(
            &self,
            from: &PersistenceId,
            to: &PersistenceId,
        ) -> horsie_actor::JournalResult<()> {
            self.inner.copy_snapshot(from, to).await
        }

        async fn last_seq(&self, pid: &PersistenceId) -> horsie_actor::JournalResult<u64> {
            self.inner.last_seq(pid).await
        }

        async fn clear(&self, pid: &PersistenceId) -> horsie_actor::JournalResult<()> {
            self.inner.clear(pid).await
        }
    }

    /// Neither of these is reached: the test never starts a turn.
    struct NoContext;

    #[async_trait]
    impl crate::agent_loop::ContextProvider for NoContext {
        async fn provide(
            &self,
        ) -> Result<crate::agent_loop::Contexts, crate::agent_loop::ContextError> {
            Err(crate::agent_loop::ContextError::retryable("no context"))
        }
    }

    struct NoParent;

    #[async_trait]
    impl AgentOutcomeSink for NoParent {
        async fn deliver(&self, _: AgentOutcome) {}
    }

    /// An actor that fails the test if anything reaches its mailbox.
    struct NeverAsked;

    #[async_trait]
    impl EventSourcedActor for NeverAsked {
        type Command = AgentCommand;
        type Event = ();
        type State = ();

        fn persistence_id(&self) -> PersistenceId {
            PersistenceId::new("capability-test", "never-asked")
        }

        fn initial_state() {}

        fn apply_event((): (), (): ()) {}

        async fn handle_command(
            &mut self,
            (): &(),
            _cmd: AgentCommand,
            _ctx: &mut ActorContext<AgentCommand>,
        ) -> CommandEffect<()> {
            panic!("an ordinary tool call must never reach the mailbox");
        }
    }

    struct Sandbox;

    #[async_trait]
    impl horsie_agentcore::Toolbox for Sandbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "bash".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        async fn execute(
            &self,
            name: &str,
            _input: Value,
            _tool_call_id: &str,
        ) -> Result<ToolOutcome, ToolCallError> {
            Ok(ToolOutcome::Result(Value::String(format!(
                "sandbox ran {name}"
            ))))
        }
    }

    /// One capability's layer over the sandbox, dispatching through a real
    /// mailbox — the actor behind it fails the test if it is ever reached.
    fn layer(specs: Vec<ToolSpec>) -> Arc<dyn horsie_agentcore::Toolbox> {
        let mailbox: Arc<dyn capabilities::Mailbox> = Arc::new(AgentMailbox {
            actor: crate::testing::spawn_detached(
                &ActorSystem::new(Arc::new(InMemoryJournal::new())),
                NeverAsked,
            ),
        });
        let claims = specs
            .into_iter()
            .map(|spec| {
                let tool = spec.name.clone();
                crate::agent_loop::toolbox::ClaimedTool::new(spec, move |_input, to| {
                    capabilities::CapCommand::Fake(
                        capabilities::testing::FakeCommand { tool: tool.clone() },
                        to,
                    )
                })
            })
            .collect();
        crate::agent_loop::toolbox::claiming(Arc::new(Sandbox), claims, &mailbox)
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    /// The layer advertises what the capability answers for *alongside* the
    /// sandbox's own tools, rather than replacing them — and its own first,
    /// because it is the outer one and would win a name against them.
    #[tokio::test]
    async fn the_layer_advertises_capabilities_beside_the_sandbox() {
        let names: Vec<String> = layer(vec![spec("ask_user")])
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, vec!["ask_user", "bash"]);
    }

    /// An ordinary sandbox call goes straight through. The mailbox is not a
    /// cheap place to send every `bash` call, and this is what keeps it out --
    /// the stub actor panics if the round trip happens.
    #[tokio::test]
    async fn an_ordinary_call_never_touches_the_mailbox() {
        let outcome = layer(vec![spec("ask_user")])
            .execute("bash", Value::Null, "tc1")
            .await
            .expect("the sandbox answers");
        assert_eq!(
            outcome,
            ToolOutcome::Result(Value::String("sandbox ran bash".into()))
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod ask_wiring_tests {
    //! The park, driven through the actor's own routing rather than the
    //! capability's `handle` directly — which is where the two used to be
    //! unable to meet.

    use super::*;
    use crate::agent_loop::capabilities::Capabilities;
    use crate::agent_loop::capabilities::ask_user::{AskUserCapability, Command};
    use crate::agent_loop::capabilities::testing::answering;
    use crate::agent_loop::capabilities::{CapCommand, Msg};

    fn attended() -> AgentState {
        AgentState {
            capabilities: Capabilities::new(vec![Box::new(AskUserCapability::new())]),
            ..AgentState::default()
        }
    }

    /// The command `ask_user`'s own layer builds for a question.
    fn ask(id: &str, question: &str) -> CapCommand {
        CapCommand::AskUser(
            Command::Ask {
                input: serde_json::json!({ "question": question }),
            },
            answering(id),
        )
    }

    fn fold(state: &AgentState, events: &[AgentDomainEvent]) -> AgentState {
        events
            .iter()
            .fold(state.clone(), |s, e| AgentActor::apply_event(s, e.clone()))
    }

    /// The whole loop: a call parks the run, the park survives as folded state,
    /// and an answer resumes it with a result paired to the dangling call.
    #[test]
    fn an_ask_parks_and_its_answer_resumes_the_dangling_call() {
        let state = attended();
        let parked = AgentActor::consult_command(&state, &ask("call-1", "which?"))
            .expect("ask_user owns its own command");
        assert_eq!(
            parked
                .answer
                .as_ref()
                .map(|a| a.as_ref().expect("no error")),
            Some(&ToolOutcome::StopRun),
            "the run has to stop, or the tool_use never dangles"
        );

        let state = fold(&state, &parked.events);
        let answers = vec![crate::agent_loop::AskAnswer {
            tool_call_id: "call-1".into(),
            text: "the second one".into(),
        }];
        let resumed = AgentActor::consult(&state, &Msg::Answer(&answers))
            .expect("the capability holding the park claims the answer");
        assert_eq!(
            resumed
                .resume
                .iter()
                .map(|r| (r.tool_call_id.as_str(), r.output.as_str(), r.is_error))
                .collect::<Vec<_>>(),
            vec![("call-1", "the second one", false)],
            "the result must pair with the call the model is parked on"
        );
    }

    /// A turn that begins while the park is still open is one the queue
    /// abandoned, and the capability stops holding it. `queued_turn` already
    /// recorded a result for every call, so nothing is left dangling.
    #[test]
    fn a_turn_beginning_on_an_open_park_abandons_it() {
        let state = attended();
        let parked = AgentActor::consult_command(&state, &ask("call-1", "which?")).expect("mine");
        let state = fold(&state, &parked.events);

        let began = AgentActor::consult(&state, &Msg::Turn(TurnEvent::Began)).expect("broadcast");
        assert_eq!(began.events.len(), 1, "the park should be given up");
        let after = fold(&state, &began.events);
        assert!(
            !serde_json::to_string(&after.capabilities)
                .expect("serialise")
                .contains("which?"),
            "the park outlived the turn that abandoned it"
        );
    }

    /// And an answered park is *not* abandoned by the turn its own answer
    /// starts — the ordering the command handler exists to get right.
    /// The same broadcast against the state the answer has *not* been folded
    /// into records the park as abandoned — which is why the command handler
    /// folds first.
    ///
    /// Worth being exact about what this protects: both events clear `pending`,
    /// so the state is the same either way. What the ordering keeps honest is
    /// the *record* — an `Abandoned` here would tell every later reader that the
    /// person's answer was ignored, on the very turn it was acted on.
    #[test]
    fn broadcasting_before_folding_the_answer_would_record_a_false_abandonment() {
        let state = attended();
        let parked = AgentActor::consult_command(&state, &ask("call-1", "which?")).expect("mine");
        let stale = fold(&state, &parked.events);

        let answers = vec![crate::agent_loop::AskAnswer {
            tool_call_id: "call-1".into(),
            text: "this one".into(),
        }];
        let resumed = AgentActor::consult(&stale, &Msg::Answer(&answers)).expect("mine");

        // Deliberately *not* folded — the mistake the handler must not make.
        let began = AgentActor::consult(&stale, &Msg::Turn(TurnEvent::Began)).expect("broadcast");
        assert_eq!(
            began.events.len(),
            1,
            "against stale state the park still looks open, so it is given up"
        );
        // And folded, as the handler does it, the same broadcast records nothing.
        let folded = fold(&stale, &resumed.events);
        assert!(
            AgentActor::consult(&folded, &Msg::Turn(TurnEvent::Began))
                .expect("broadcast")
                .events
                .is_empty()
        );
    }

    #[test]
    fn the_turn_an_answer_starts_does_not_abandon_the_park_it_just_closed() {
        let state = attended();
        let parked = AgentActor::consult_command(&state, &ask("call-1", "which?")).expect("mine");
        let state = fold(&state, &parked.events);

        let answers = vec![crate::agent_loop::AskAnswer {
            tool_call_id: "call-1".into(),
            text: "this one".into(),
        }];
        let resumed = AgentActor::consult(&state, &Msg::Answer(&answers)).expect("mine");
        let state = fold(&state, &resumed.events);

        let began = AgentActor::consult(&state, &Msg::Turn(TurnEvent::Began)).expect("broadcast");
        assert!(
            began.events.is_empty(),
            "the answer already closed the park; recording it abandoned would \
             tell a reader the person was ignored"
        );
    }

    /// The queue rides along with a resume, so a subagent that finished while
    /// the person was typing is not stranded until something else happens.
    #[test]
    fn a_resume_carries_whatever_queued_behind_it() {
        let inbox = vec![crate::agent_loop::Incoming::User {
            id: "m1".into(),
            text: "also do this".into(),
        }];
        let turn = crate::agent_loop::resumed_turn(
            &inbox,
            vec![horsie_models::agent::ToolResultInput {
                tool_call_id: "call-1".into(),
                output: "answered".into(),
                is_error: false,
            }],
        );
        assert_eq!(turn.answered, vec!["call-1"]);
        assert_eq!(turn.consumed, vec!["m1"]);
        assert_eq!(turn.message.as_deref(), Some("also do this"));
    }
}
