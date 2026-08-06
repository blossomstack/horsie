//! Interactive sessions: event-sourced actors on the shared `horsie-actor` core.
//!
//! `SessionSupervisor` (journal `session-supervisor/main`) owns the registry and
//! one `SessionActor` child per live session (journal `session/<id>`); each
//! session hosts a reused `AgentActor` (journal `agent/<id>`). Recovery is lazy:
//! journals replay at startup, runtimes respawn only on user action.

pub mod ask_tool;
pub mod builder;
pub mod clock;
pub mod events;
pub mod lifecycle_routing;
pub mod mode;
pub mod orchestrator;
pub mod session_actor;
pub mod spawn_tool;
pub mod spec;
pub mod subagents;
pub mod supervisor;
pub mod title_tool;
pub mod workflow;

/// Live broadcast frames for one agent's SSE stream.
///
/// `Appended` is the only frame that belongs to a log, and it is published from
/// the agent actor's post-persist hook — so it is durable by the time it is
/// broadcast, and no journal read is needed to serve it. The rest are current
/// values or run-scoped noise.
#[derive(Debug, Clone)]
/// `large_enum_variant`: `Appended` carries a `HistoryEntry`, whose `Hook` arm
/// holds a whole `HookRecord`. The type is fluorite-generated so the variant
/// cannot be boxed, and a frame is moved once per append.
#[allow(clippy::large_enum_variant)]
pub enum AgentFrame {
    /// One transcript append, durable. `entry.id()` is the stream's SSE id and
    /// the history cursor — one vocabulary for both. Not every append is a
    /// message the model saw; see `HistoryEntry`.
    Appended {
        entry: horsie_agentcore::HistoryEntry,
    },
    /// Streaming text delta. Ephemeral: never journaled, never replayed.
    Delta { text: String },
    /// A tool call started. Ephemeral.
    ToolStart { tool_call_id: String, name: String },
    /// A turn finished, with its own usage.
    TurnCompleted {
        iterations: u32,
        usage: horsie_agentcore::Usage,
        at_ms: u64,
    },
    /// The agent's task list, whole — never a delta.
    TaskListChanged {
        tasks: Vec<horsie_workflow::TaskRecord>,
    },
}

/// Why a message could not be accepted. There is no "busy" here by design: a
/// turn in flight queues the message rather than rejecting it.
#[derive(Debug, thiserror::Error)]
pub enum UserMessageError {
    #[error("session not found")]
    NotFound,
    #[error("session is unrecoverable: {0}")]
    Unrecoverable(String),
    /// This session kind takes no messages — a workflow run works from its
    /// definition. Comes from `Orchestrator::accepts`, so the rule lives in one
    /// place rather than in a handler guard.
    #[error("{0}")]
    Rejected(String),
}
