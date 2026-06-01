use crate::transport::{RuntimeTransport, TransportError};
use models::runtime::{ToolCall, ToolError, ToolOutput, ToolResult, WorkspaceScan};
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

    /// Build a client from an already-type-erased transport — e.g. the one handed
    /// back by `ExecutorClient::runtime_transport`, which cannot be re-boxed by
    /// [`RuntimeClient::new`]'s `impl RuntimeTransport` bound.
    pub fn from_arc(transport: Arc<dyn RuntimeTransport>) -> Self {
        Self { inner: transport }
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

    /// Scan the workspace over the runtime. `instruction_candidates` are tried in
    /// order (first existing wins); `skills_glob` locates skill files. Raw contents
    /// come back for the caller to interpret.
    pub async fn scan_workspace(
        &self,
        instruction_candidates: Vec<String>,
        skills_glob: String,
    ) -> Result<WorkspaceScan, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.inner
            .scan_workspace(&call_id, instruction_candidates, skills_glob)
            .await
            .map_err(RuntimeCallError::Transport)
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

    #[tokio::test]
    async fn client_scan_returns_mock_scan() {
        use models::runtime::{ScannedFile, WorkspaceScan};
        let scan = WorkspaceScan {
            instructions: Some(ScannedFile {
                path: "AGENTS.md".into(),
                content: "hi".into(),
            }),
            skills: vec![],
        };
        let client = RuntimeClient::new(MockTransport::ok("").with_scan(scan));
        let out = client
            .scan_workspace(vec!["AGENTS.md".into()], ".claude/skills/*/SKILL.md".into())
            .await
            .unwrap();
        assert_eq!(out.instructions.unwrap().content, "hi");
    }
}
