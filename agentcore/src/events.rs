use models::events::AgentEvent;

/// Sync observer for agent events.
/// `emit` is synchronous — callers who need async delivery should implement
/// this with an `mpsc::Sender::try_send` internally.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}
