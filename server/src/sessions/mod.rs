//! Interactive sessions: event-sourced actors on the shared `horsie-actor` core.
//!
//! `SessionSupervisor` (journal `session-supervisor/main`) owns the registry and
//! one `SessionActor` child per live session (journal `session/<id>`); each
//! session hosts a reused `AgentActor` (journal `agent/<id>`). Recovery is lazy:
//! journals replay at startup, runtimes respawn only on user action.

pub mod ask_tool;
pub mod clock;
pub mod events;
pub mod session_actor;
pub mod spec;
pub mod supervisor;
pub mod title_tool;

/// Live broadcast frames for one session's SSE stream.
#[derive(Debug, Clone)]
pub enum SessionFrame {
    /// Streaming text delta (id-less).
    Delta { text: String },
    /// A tool call started (id-less).
    ToolStart { tool_call_id: String, name: String },
    /// One or more coarse events were journaled — SSE handlers replay the
    /// journal after their cursor to pick them up with stable ids.
    Journaled,
    /// Live status transition (id-less; durable status lives in the registry).
    Status { status: spec::SessionStatus },
    /// A turn failed (id-less; also recorded as `last_error`).
    Error { message: String },
    /// A resource-preparation progression (id-less live signal; also journaled
    /// by the session for audit).
    Progression {
        stage: String,
        detail: Option<String>,
        at_ms: u64,
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
