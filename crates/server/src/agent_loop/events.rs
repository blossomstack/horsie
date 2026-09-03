//! What has happened to an agent: the events, and nothing else.
//!
//! Every change to [`AgentState`](crate::agent_loop::AgentState) is one of
//! these, journaled before it is believed and folded by exactly one component
//! — see [`AgentLoop::apply`](crate::agent_loop::components::AgentLoop).
//!
//! This is a durability contract. A variant that fails to deserialize takes
//! down recovery for every session that ever journaled one, so fields are
//! added with `#[serde(default)]` and never renamed or repurposed.

use crate::agent_loop::AgentState;
use horsie_agentcore::{LifecycleEvent, Message, Usage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SystemPromptSource {
    Configured,
    InitialContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepKind {
    Initialize,
    Connect,
    Agent,
    StopHook,
    Compaction,
    SeedSummary { request_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepFailure {
    Interrupted,
    Provider(String),
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopHookOutcome {
    Allow,
    Continue { message: String },
    Failed { reason: String },
    Interrupted,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RunEnd {
    Complete {
        output: serde_json::Value,
    },
    AwaitingInput {
        asks: Vec<crate::agent_loop::AskedQuestion>,
    },
    Parked,
    Cancelled,
    Interrupted,
    Failed {
        error: String,
        recoverable: bool,
        terminal: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHistoryEntry {
    pub seq: u64,
    pub record: AgentDomainEvent,
}

/// Coarse events that alter persisted agent state. Streaming observation events
/// (text/tool-input deltas) are emitted to the event sink but never journaled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentDomainEvent {
    SystemPromptRecorded {
        source: SystemPromptSource,
        content: String,
    },
    AgentInitialized {
        manifest: crate::agent_loop::ContextManifest,
    },
    ConnectionCompleted,
    /// Its assigned history sequence is both step identity and callback fence.
    StepStarted {
        kind: StepKind,
    },
    StepFailed {
        reason: StepFailure,
    },
    StopHookCompleted {
        outcome: StopHookOutcome,
    },
    RunEnded {
        reason: RunEnd,
        at_ms: u64,
    },
    /// This agent was seeded from another session: `state` is the history it
    /// adopts, `seed` a synthetic message appended after it.
    ///
    /// One event rather than a snapshot written behind the actor's back, so a
    /// sub session's own journal explains where its history came from. Only
    /// ever the *first* event an agent has — replacing state wholesale is safe
    /// precisely because nothing has run.
    ///
    /// `seed` is `None` for every mode but a summary. A copy's history *is* the
    /// context and a fresh sub session's brief is queued behind this event, so
    /// only a summary has something to say that is nowhere else.
    ///
    /// Boxed: a whole session is far larger than any other variant here,
    /// and an enum is as big as its widest arm.
    Seeded {
        state: Box<AgentState>,
        #[serde(default)]
        seed: Option<Box<Message>>,
    },
    InputMessage {
        message: Message,
    },
    /// Queue items taken by work that is not a turn — a `/compact`, a summary
    /// for branching sub sessions. Journaled when the work *lands*, not when
    /// it starts: a crash in between replays the item, and doing it twice is
    /// cheaper than a sub session waiting for ever on a summary nobody will
    /// take again.
    Consumed {
        ids: Vec<String>,
        at_ms: u64,
    },
    /// One provider step completed. Usage is part of the same durable fact as
    /// the assistant message, so no later failure can lose what this call
    /// spent.
    MessageComplete {
        message: Message,
        usage: Usage,
    },
    /// An assistant message the run never got to finish, rebuilt from the text
    /// it had already streamed when the turn was cancelled.
    ///
    /// A separate variant from `MessageComplete` because it is a different
    /// claim: the provider never said this message was done, and one day the
    /// history sent back may want to say so. It folds into the same log entry,
    /// because what was generated happened.
    MessageAborted {
        message: Message,
    },
    ToolComplete {
        tool_call_id: String,
        output: String,
        is_error: bool,
        /// What the tool produced beyond text — already-stored references.
        /// Journaled so the prompt the fold rebuilds carries them: the model's
        /// next call within the same turn must be able to see a screenshot a
        /// tool just took, and the fold is now the only source of history.
        #[serde(default)]
        artifacts: Vec<horsie_models::agent::ArtifactRef>,
        /// When the tool finished. Journaled rather than re-read at fold time:
        /// this variant rebuilds its `Message` in `apply_event`, so a recovered
        /// transcript would otherwise stamp every past tool result with the
        /// moment of recovery.
        at_ms: u64,
    },
    /// A plugin hook ran against a tool call this agent made. Journaled beside
    /// the call's own `ToolComplete` because a hook changes what the agent did,
    /// and that must be auditable rather than invisible.
    HookRan {
        record: horsie_models::hooks::HookRecord,
        /// How many records were already recorded against this same tool call.
        /// Journaled rather than recomputed at fold time so the id is a fact of
        /// the log: the live broadcast derives the entry from the event alone
        /// and would otherwise have to guess, giving the stream different
        /// cursors than `/history` for a call with more than one hook.
        seq: usize,
        at_ms: u64,
    },
    /// A run loop reached its normal boundary. Provider usage was already
    /// banked by each `MessageComplete`; this event carries no bill.
    RunComplete {
        iterations: u32,
        at_ms: u64,
    },
    /// A run loop ended badly. Completed provider steps already banked their
    /// own usage, so cancellation or failure cannot lose or double-count it.
    RunAborted {
        at_ms: u64,
    },
    RunCancelled {
        at_ms: u64,
    },
    /// A timer was armed.
    TimerArmed {
        record: crate::agent_loop::components::timers::domain::TimerRecord,
        at_ms: u64,
    },
    /// One or more timers were cancelled.
    TimerCancelled {
        ids: Vec<crate::agent_loop::components::timers::domain::TimerId>,
        at_ms: u64,
    },
    /// A timer fired. `next_fire_at_unix_ms` carries the re-armed fire time
    /// for a recurring timer (so the fold stays pure); `None` removes a
    /// one-shot.
    TimerFired {
        id: crate::agent_loop::components::timers::domain::TimerId,
        next_fire_at_unix_ms: Option<u64>,
        at_ms: u64,
    },
    /// The agent parked, awaiting a timer or a subagent still working.
    Parked {
        at_ms: u64,
    },
    /// A turn ended without the result this agent owed, and nothing would have
    /// woken it. Journaled so the budget behind the nudge survives a restart —
    /// otherwise a crash loop hands the model a fresh nudge for ever.
    Nudged {
        at_ms: u64,
    },
    /// The task list changed (create/insert/update_status). Carries the full
    /// resulting state, not a delta — mirrors `MessageComplete`/`ToolComplete`,
    /// so replay never needs to re-derive or re-validate a past mutation.
    TaskListChanged {
        snapshot: crate::agent_loop::components::task_list::domain::TaskListState,
        at_ms: u64,
    },
    /// Something that happened to the session, recorded here so there is one
    /// ordered record for a client to read rather than a second stream to
    /// reconcile against this one.
    ///
    /// The session actor still owns every one of these and remains the only
    /// thing that decides them; it tells the agent to record it. Journaled by
    /// the agent because the agent is the sole writer of its own log, which is
    /// what makes the order deterministic without any merge.
    LifecycleRecorded {
        event: LifecycleEvent,
        at_ms: u64,
    },
    /// Older history stopped being shown to the model.
    ///
    /// Journaled like any other append: the log keeps everything it held, and
    /// this only records where the prompt now starts. Folding it is therefore
    /// deterministic under replay, and a recovered agent prompts from exactly
    /// the boundary the live one did.
    ///
    /// Carries the *message id* the retained window starts at, not a seq. The
    /// run that produced it was holding a `Vec<Message>` in prompt order,
    /// which is not log order; resolving the two is the fold's job because the
    /// fold is the only thing holding the log.
    Compacted {
        summary: String,
        carried_state: String,
        retained_from_message_id: Option<String>,
        trigger: horsie_agentcore::CompactionTrigger,
        instructions: Option<String>,
        tokens_before: u32,
        tokens_after: u32,
        /// What the summarising call spent, aggregated into `usage_total` by
        /// the fold. Optional and defaulted: older boundaries carry none.
        #[serde(default)]
        usage: Option<Usage>,
        at_ms: u64,
    },
    /// Something was accepted into this agent's queue.
    ///
    /// Journaled before anything is done with it, which is what makes an
    /// accepted message a promise: it survives a crash and is still owed an
    /// answer.
    Received {
        item: crate::agent_loop::Incoming,
        at_ms: u64,
    },
    /// A turn began, consuming these queue items — and, if the agent was
    /// parked, answering these questions. One event so a crash anywhere in the
    /// window replays to the same place.
    TurnBegan {
        consumed: Vec<String>,
        /// Every question this turn *answered*. Empty when the turn abandoned
        /// them instead, which is what a plain message does.
        answered: Vec<String>,
        at_ms: u64,
    },
    /// The agent parked on these questions. One event for the whole park rather
    /// than one per question: they are asked together and answered together, so
    /// there is never a moment when only some of them are pending.
    AskRecorded {
        asks: Vec<crate::agent_loop::AskedQuestion>,
        at_ms: u64,
    },
    /// A summary was taken for sub sessions branching off this agent. Nothing
    /// about this agent's history changed; what the summarising call spent is
    /// aggregated into `usage_total` by the fold.
    SeedSummaryTaken {
        request_id: String,
        sub_sessions: Vec<uuid::Uuid>,
        result: Result<String, String>,
        usage: Option<Usage>,
        at_ms: u64,
    },
}
