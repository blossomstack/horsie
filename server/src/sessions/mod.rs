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
pub mod session_actor;
pub mod spawn_tool;
pub mod spec;
pub mod subagents;
pub mod supervisor;
pub mod title_tool;

/// Live broadcast frames for one session's SSE stream — session-scoped current
/// values only. A transcript belongs to an agent, not a session, so it is not
/// here; see [`AgentFrame`]. None of these carries a cursor: a client that
/// misses one re-reads the session document.
#[derive(Debug, Clone)]
pub enum SessionFrame {
    /// Live status transition (durable status lives in the registry).
    Status { status: spec::SessionStatus },
    /// A turn failed (also recorded as `last_error`).
    Error { message: String },
    /// The queue of accepted-but-unanswered messages, whole (the detail
    /// endpoint is the durable source, folded from the session journal).
    InboxChanged {
        queued: Vec<session_actor::InboxMessage>,
    },
    /// A resource-preparation progression (live signal; also journaled by the
    /// session for audit).
    Progression {
        stage: String,
        detail: Option<String>,
        at_ms: u64,
    },
    /// The session's agent roster changed — a subagent spawned or finished.
    AgentTreeChanged,
}

/// Live broadcast frames for one agent's SSE stream.
///
/// `Appended` is the only frame that belongs to a log, and it is published from
/// the agent actor's post-persist hook — so it is durable by the time it is
/// broadcast, and no journal read is needed to serve it. The rest are current
/// values or run-scoped noise.
#[derive(Debug, Clone)]
pub enum AgentFrame {
    /// One transcript append, durable. `message.id` is the stream's SSE id and
    /// the history cursor — one vocabulary for both.
    Appended { message: horsie_agentcore::Message },
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
}
