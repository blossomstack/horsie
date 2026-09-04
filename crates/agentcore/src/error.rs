use crate::events::EventSinkError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimit {
        retry_after: Option<std::time::Duration>,
    },

    #[error("provider overloaded")]
    Overloaded,

    #[error("api error {status}: {message}")]
    ApiError { status: u16, message: String },

    #[error("network error: {0}")]
    Network(#[source] Box<dyn std::error::Error + Send + Sync>),

    /// An event sink failed while the provider was emitting a streaming event. The
    /// provider does not decide whether that is fatal — it propagates the sink's
    /// verdict, so a sink that returns `Err` aborts the completion (and thus the
    /// run); a best-effort sink returns `Ok(())` and is never seen here.
    #[error("event sink error: {0}")]
    EventSink(#[from] EventSinkError),
}

impl LlmError {
    /// Whether another attempt could plausibly succeed.
    ///
    /// The single definition of "transient" for the whole stack: providers use it
    /// to decide whether to re-stream, and the agent actor uses it to decide
    /// whether to re-run a turn. Two layers disagreeing about this is how a
    /// permanent 401 ends up retried seven times (#61 items 6 and 21).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            Self::RateLimit { .. } | Self::Overloaded | Self::Network(_) => true,
            // A classified API error is the server telling us the request itself
            // is wrong; repeating it verbatim cannot help. An event-sink failure
            // is a local durability problem, and retrying against the LLM would
            // burn tokens on a disk fault.
            Self::ApiError { .. } | Self::EventSink(_) => false,
        }
    }

    /// The provider-supplied delay before retrying, when the error carries one.
    #[must_use]
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            Self::RateLimit { retry_after } => *retry_after,
            Self::Overloaded | Self::Network(_) | Self::ApiError { .. } | Self::EventSink(_) => {
                None
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum ToolCallError {
    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("execution error: {0}")]
    Execution(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("execution failed: {0}")]
    ExecutionFailed(String),
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

    #[test]
    fn tool_call_error_invalid_input_display() {
        let e = ToolCallError::InvalidInput("bad json".into());
        assert!(e.to_string().contains("bad json"));
    }

    #[test]
    fn llm_error_api_error_display() {
        let e = LlmError::ApiError {
            status: 429,
            message: "rate limit".into(),
        };
        assert!(e.to_string().contains("429"));
    }
}
