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

    /// Scan the selected workspaces (`workspace`: `None` = all, `Some(name)` = one),
    /// reading the first existing instruction candidate (in order) and every file
    /// matching `skills_glob` per root, returning raw contents. Name→path resolution
    /// happens runtime-side against its workspace registry.
    async fn scan_workspace(
        &self,
        call_id: &str,
        workspace: Option<String>,
        instruction_candidates: Vec<String>,
        skills_glob: String,
    ) -> Result<Vec<WorkspaceScan>, TransportError>;
}

/// Mock transport for tests — returns a configurable canned result.
pub struct MockTransport {
    result: ToolResult,
    scan: Vec<WorkspaceScan>,
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

    /// Return a specific [`ToolOutput`] (lets tests exercise stderr / exit codes).
    pub fn output(output: ToolOutput) -> Self {
        Self {
            result: ToolResult::Ok(output),
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
    pub fn with_scan(mut self, scan: Vec<WorkspaceScan>) -> Self {
        self.scan = scan;
        self
    }
}

fn empty_scan() -> Vec<WorkspaceScan> {
    Vec::new()
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
        _workspace: Option<String>,
        _instruction_candidates: Vec<String>,
        _skills_glob: String,
    ) -> Result<Vec<WorkspaceScan>, TransportError> {
        Ok(self.scan.clone())
    }
}
