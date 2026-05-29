use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentBuildError {
    #[error("nudge_threshold ({nudge}) must be less than stuck_threshold ({stuck})")]
    InvalidConfig { nudge: usize, stuck: usize },
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("max iterations exceeded (max={max})")]
    MaxIterationsExceeded { max: u32 },

    #[error("stuck in loop: tool '{tool_name}' called identically {count} times")]
    StuckInLoop { tool_name: String, count: usize },

    #[error("provider error: {0}")]
    Provider(#[from] LlmError),

    #[error("cancelled")]
    Cancelled,
}

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
    fn agent_error_cancelled_display() {
        assert_eq!(AgentError::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn agent_error_max_iterations_display() {
        let e = AgentError::MaxIterationsExceeded { max: 50 };
        assert!(e.to_string().contains("50"));
    }

    #[test]
    fn agent_error_stuck_display() {
        let e = AgentError::StuckInLoop {
            tool_name: "search".into(),
            count: 5,
        };
        assert!(e.to_string().contains("search"));
        assert!(e.to_string().contains("5"));
    }

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
