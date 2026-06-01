use async_trait::async_trait;
use models::runtime::{ToolCall, ToolOutput, ToolResult, WorkspaceScan};
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

    /// Scan the workspace: read the first existing instruction candidate (in order)
    /// and every file matching `skills_glob`, returning raw contents.
    async fn scan_workspace(
        &self,
        call_id: &str,
        instruction_candidates: Vec<String>,
        skills_glob: String,
    ) -> Result<WorkspaceScan, TransportError>;
}

/// Mock transport for tests — returns a configurable canned result.
pub struct MockTransport {
    result: ToolResult,
    scan: WorkspaceScan,
}

impl MockTransport {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            result: ToolResult::Ok(ToolOutput {
                stdout: stdout.into(),
                stderr: String::new(),
                exit_code: 0,
            }),
            scan: empty_scan(),
        }
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            result: ToolResult::Err(models::runtime::ToolError {
                reason: reason.into(),
            }),
            scan: empty_scan(),
        }
    }

    /// Override the canned scan returned by `scan_workspace`.
    pub fn with_scan(mut self, scan: WorkspaceScan) -> Self {
        self.scan = scan;
        self
    }
}

fn empty_scan() -> WorkspaceScan {
    WorkspaceScan {
        instructions: None,
        skills: Vec::new(),
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

    async fn scan_workspace(
        &self,
        _call_id: &str,
        _instruction_candidates: Vec<String>,
        _skills_glob: String,
    ) -> Result<WorkspaceScan, TransportError> {
        Ok(self.scan.clone())
    }
}
