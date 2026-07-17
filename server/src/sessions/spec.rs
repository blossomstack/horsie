//! STORAGE types for sessions (journal-owned). Distinct from the fluorite wire
//! types in `horsie_models::session` — wire formats evolve at the speed of the
//! API contract, these evolve at the speed of data migrations.

use crate::vendor::RuntimeVendor;
use horsie_agentcore::LlmProvider;
use horsie_models::capabilities::CapabilitySpec;
use horsie_models::session::SessionStatusKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// LLM providers keyed by model alias, behind a shared lock so the settings API
/// can swap the whole set live. Read once per turn in
/// [`crate::sessions::session_actor::SessionActor::ensure_agent`]; the guard is
/// never held across an `.await`.
pub type SharedProviderRegistry = Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>;

/// Runtime vendors keyed by name, behind a shared lock so a settings-API vendor
/// edit can activate/reconfigure/retire a vendor without a restart. Read once
/// per provision call in [`crate::sessions::session_actor::SessionActor::vendor`].
pub type SharedVendors = Arc<RwLock<HashMap<String, Arc<dyn RuntimeVendor>>>>;

/// A session's unique id (a UUID string). Equals the agent session uuid, so
/// `session/<id>` and `agent/<id>` journals share the same `<id>`.
pub type SessionId = String;

/// Agent settings supplied at session creation. Storage copy of the wire
/// `horsie_models::session::AgentSettings`, with defaults applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSettings {
    pub model: String,
    pub system_prompt: Option<String>,
    pub allowed_tools: Option<Vec<String>>,
    pub allow_ask_user: bool,
    pub use_plugins: Option<bool>,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
    /// Enabled MCP servers this session may call (by name); tools appear as
    /// `mcp__<name>__<tool>`. Empty → none. `#[serde(default)]` so pre-MCP
    /// journal rows deserialize.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

/// One session workspace as persisted: a host path (bring-your-own) or `None`
/// (vendor-allocated). Storage twin of the vendor layer's `WorkspaceSpec`;
/// old journal rows (`{name, path}`) deserialize as `path: Some(_)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceDef {
    pub name: String,
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// One provision step as persisted (storage twin of the wire `ProvisionStep`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionStepSpec {
    pub name: String,
    pub uses: String,
    pub with: Vec<(String, String)>,
}

/// Persisted, self-contained description of one session (lives in the
/// supervisor journal, like the daemon's `JobSpec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub name: Option<String>,
    pub agent: AgentSettings,
    pub workspaces: Vec<WorkspaceDef>,
    /// Setup steps run by the runtime at every create/attach (idempotent).
    #[serde(default)]
    pub provision: Vec<ProvisionStepSpec>,
    /// Already resolved (paths expanded, plugin grants + seatbelt rules applied)
    /// at creation.
    pub capabilities: CapabilitySpec,
    /// Runtime vendor name (key into [`ServerDeps::vendors`]).
    pub vendor: String,
    pub plugins_dir: Option<PathBuf>,
    pub hook_path: Vec<PathBuf>,
    /// Selected plugin-bundle names to provision for this session. Resolved to
    /// current artifact hashes at each create/attach (latest-at-start); the
    /// runtime fetches them into its plugins dir before scanning.
    #[serde(default)]
    pub plugins: Vec<String>,
}

/// User-visible lifecycle state. Failure reasons ride inside the variants;
/// [`status_kind`]/[`status_reason`] project them onto the wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionStatus {
    Provisioning,
    Idle,
    Running,
    AwaitingInput,
    Interrupted,
    Stopped,
    RecoveryFailed { reason: String },
    Failed { reason: String },
}

/// Project a storage status onto its wire discriminant.
pub fn status_kind(s: &SessionStatus) -> SessionStatusKind {
    match s {
        SessionStatus::Provisioning => SessionStatusKind::Provisioning,
        SessionStatus::Idle => SessionStatusKind::Idle,
        SessionStatus::Running => SessionStatusKind::Running,
        SessionStatus::AwaitingInput => SessionStatusKind::AwaitingInput,
        SessionStatus::Interrupted => SessionStatusKind::Interrupted,
        SessionStatus::Stopped => SessionStatusKind::Stopped,
        SessionStatus::RecoveryFailed { .. } => SessionStatusKind::RecoveryFailed,
        SessionStatus::Failed { .. } => SessionStatusKind::Failed,
    }
}

/// The failure reason a status carries, if any.
pub fn status_reason(s: &SessionStatus) -> Option<String> {
    match s {
        SessionStatus::RecoveryFailed { reason } | SessionStatus::Failed { reason } => {
            Some(reason.clone())
        }
        SessionStatus::Provisioning
        | SessionStatus::Idle
        | SessionStatus::Running
        | SessionStatus::AwaitingInput
        | SessionStatus::Interrupted
        | SessionStatus::Stopped => None,
    }
}

/// Process-wide dependencies injected into every [`crate::sessions::session_actor::SessionActor`].
#[derive(Clone)]
pub struct ServerDeps {
    /// LLM providers keyed by the session's `model`, swappable at runtime.
    pub provider_registry: SharedProviderRegistry,
    /// Runtime vendors keyed by the session spec's `vendor` name.
    pub vendors: SharedVendors,
    /// Per-session server state (capability files) under `<state_dir>/sessions/<id>/`.
    pub state_dir: PathBuf,
    /// Mints short-lived GitHub tokens for repo provisioning; `None` when the
    /// deployment has no GitHub integration wired.
    pub github_tokens: Option<Arc<dyn crate::github::GithubTokenMinter>>,
    /// Builds per-session MCP toolboxes for the agent; `None` when the
    /// deployment has no MCP integration wired (tests). A session that names an
    /// MCP server with no service configured connects to nothing.
    pub mcp: Option<Arc<crate::mcp::McpService>>,
    /// Resolves selected plugin bundles to fetchable refs and mints capability
    /// tokens at provisioning; `None` when no plugin library is wired.
    pub plugins: Option<Arc<dyn crate::plugins::PluginProvisioner>>,
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
    fn workspace_def_reads_old_journal_shape() {
        let old = r#"{"name":"api","path":"/home/u/api"}"#;
        let w: WorkspaceDef = serde_json::from_str(old).unwrap();
        assert_eq!(w.path.as_deref(), Some(std::path::Path::new("/home/u/api")));
        let managed = r#"{"name":"main"}"#;
        let w: WorkspaceDef = serde_json::from_str(managed).unwrap();
        assert_eq!(w.path, None);
    }

    #[test]
    fn status_kind_and_reason_project_failures() {
        let s = SessionStatus::RecoveryFailed {
            reason: "gone".into(),
        };
        assert_eq!(status_kind(&s), SessionStatusKind::RecoveryFailed);
        assert_eq!(status_reason(&s).as_deref(), Some("gone"));
        assert_eq!(status_reason(&SessionStatus::Idle), None);
    }
}
