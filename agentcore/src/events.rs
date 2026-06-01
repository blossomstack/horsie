use async_trait::async_trait;
use models::events::AgentEvent;
use thiserror::Error;

/// Error returned by [`EventSink::emit`] when handling an event fails in a way that
/// should abort the agent run — e.g. a sink that durably persists events and whose
/// journal write failed. A best-effort observer that cannot meaningfully fail just
/// returns `Ok(())`.
#[derive(Debug, Error)]
#[error("event sink error: {0}")]
pub struct EventSinkError(pub String);

/// Async observer for agent events.
///
/// `emit` is `async` so a sink can apply real backpressure on the agent loop —
/// e.g. persisting an event and awaiting the durable write before the next
/// iteration runs. It returns a `Result` so a durability failure can abort the run
/// rather than let the loop proceed on a history that was never recorded; observers
/// that only watch return `Ok(())`.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: AgentEvent) -> Result<(), EventSinkError>;
}
