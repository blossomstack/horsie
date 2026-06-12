use async_trait::async_trait;
use horsie_models::runtime::{PluginSkill, ToolCall, ToolOutput, ToolResult, WorkspaceScan};
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
    /// happens runtime-side against its workspace registry. When `include_shared` is
    /// set, the shared plugin library's skills are returned as the second tuple element.
    async fn scan_workspace(
        &self,
        call_id: &str,
        workspace: Option<String>,
        instruction_candidates: Vec<String>,
        skills_glob: String,
        include_shared: bool,
    ) -> Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), TransportError>;

    /// Run the shared plugin library's `SessionStart` hooks in the sandbox and return
    /// their concatenated injected context (empty when there are none).
    async fn run_session_start(&self, call_id: &str) -> Result<String, TransportError>;
}

/// Mock transport for tests — returns a configurable canned result.
pub struct MockTransport {
    result: ToolResult,
    scan: Vec<WorkspaceScan>,
    shared: Vec<PluginSkill>,
    session_context: String,
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
            shared: Vec::new(),
            session_context: String::new(),
        }
    }

    /// Return a specific [`ToolOutput`] (lets tests exercise stderr / exit codes).
    pub fn output(output: ToolOutput) -> Self {
        Self {
            result: ToolResult::Ok(output),
            scan: empty_scan(),
            shared: Vec::new(),
            session_context: String::new(),
        }
    }

    pub fn err(reason: impl Into<String>) -> Self {
        Self {
            result: ToolResult::Err(horsie_models::runtime::ToolError {
                reason: reason.into(),
            }),
            scan: empty_scan(),
            shared: Vec::new(),
            session_context: String::new(),
        }
    }

    /// Override the canned scan returned by `scan_workspace`.
    pub fn with_scan(mut self, scan: Vec<WorkspaceScan>) -> Self {
        self.scan = scan;
        self
    }

    /// Override the canned shared-plugin skills returned when `include_shared` is set.
    pub fn with_shared_skills(mut self, shared: Vec<PluginSkill>) -> Self {
        self.shared = shared;
        self
    }

    /// Override the canned `SessionStart` context.
    pub fn with_session_context(mut self, context: impl Into<String>) -> Self {
        self.session_context = context.into();
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
        include_shared: bool,
    ) -> Result<(Vec<WorkspaceScan>, Vec<PluginSkill>), TransportError> {
        let shared = if include_shared {
            self.shared.clone()
        } else {
            Vec::new()
        };
        Ok((self.scan.clone(), shared))
    }

    async fn run_session_start(&self, _call_id: &str) -> Result<String, TransportError> {
        Ok(self.session_context.clone())
    }
}
