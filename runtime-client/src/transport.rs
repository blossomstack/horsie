use async_trait::async_trait;
use models::runtime::{ToolCall, ToolOutput, ToolResult};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("send failed: {0}")]
    SendFailed(String),
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("disconnected")]
    Disconnected,
}

#[async_trait]
pub trait RuntimeTransport: Send + Sync {
    async fn invoke(&self, call_id: &str, call: ToolCall) -> Result<ToolResult, TransportError>;

    async fn cancel(&self, call_id: &str) -> Result<(), TransportError>;
}

/// Mock transport for tests — returns a configurable canned result.
pub struct MockTransport {
    result: ToolResult,
}

impl MockTransport {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            result: ToolResult::Ok(ToolOutput {
                stdout: stdout.into(),
                stderr: String::new(),
                exit_code: 0,
            }),
        }
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            result: ToolResult::Err(models::runtime::ToolError {
                reason: reason.into(),
            }),
        }
    }
}

#[async_trait]
impl RuntimeTransport for MockTransport {
    async fn invoke(&self, _call_id: &str, _call: ToolCall) -> Result<ToolResult, TransportError> {
        Ok(self.result.clone())
    }

    async fn cancel(&self, _call_id: &str) -> Result<(), TransportError> {
        Ok(())
    }
}
