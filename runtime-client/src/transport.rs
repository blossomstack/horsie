use async_trait::async_trait;
use horsie_models::runtime::{PluginSkill, ToolCall, ToolResult, WorkspaceScan};
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
