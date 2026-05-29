use crate::transport::{RuntimeTransport, TransportError};
use models::runtime::{ToolCall, ToolError, ToolOutput, ToolResult};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug)]
pub enum RuntimeCallError {
    Transport(TransportError),
    ToolFailed(String),
}

impl std::fmt::Display for RuntimeCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::ToolFailed(r) => write!(f, "tool failed: {r}"),
        }
    }
}

impl std::error::Error for RuntimeCallError {}

/// Client handle for invoking tools on a remote runtime. Cheap to clone — Arc-backed.
#[derive(Clone)]
pub struct RuntimeClient {
    inner: Arc<dyn RuntimeTransport>,
}

impl RuntimeClient {
    pub fn new(transport: impl RuntimeTransport + 'static) -> Self {
        Self {
            inner: Arc::new(transport),
        }
    }

    pub async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        match self.inner.invoke(&call_id, call).await {
            Ok(ToolResult::Ok(output)) => Ok(output),
            Ok(ToolResult::Err(ToolError { reason })) => Err(RuntimeCallError::ToolFailed(reason)),
            Err(e) => Err(RuntimeCallError::Transport(e)),
        }
    }

    pub async fn cancel(&self, call_id: &str) {
        let _ = self.inner.cancel(call_id).await;
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
    use super::*;
    use crate::transport::MockTransport;
    use models::runtime::BashInput;

    #[tokio::test]
    async fn client_returns_ok_output() {
        let client = RuntimeClient::new(MockTransport::ok("hello"));
        let output = client
            .invoke(ToolCall::Bash(BashInput {
                command: "echo hello".into(),
            }))
            .await
            .unwrap();
        assert_eq!(output.stdout, "hello");
    }

    #[tokio::test]
    async fn client_returns_err_on_tool_failure() {
        let client = RuntimeClient::new(MockTransport::err("oops"));
        let err = client
            .invoke(ToolCall::Bash(BashInput {
                command: "bad".into(),
            }))
            .await
            .unwrap_err();
        assert!(matches!(err, RuntimeCallError::ToolFailed(_)));
    }
}
