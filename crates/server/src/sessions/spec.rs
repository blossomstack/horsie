//! STORAGE types for sessions (journal-owned). Distinct from the fluorite wire
//! types in `horsie_models::session` — wire formats evolve at the speed of the
//! API contract, these evolve at the speed of data migrations.

use crate::runtime_vendor::WebsocketRuntimeVendor;
use horsie_agentcore::LlmProvider;
use horsie_models::session::SessionStatusKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// LLM providers keyed by model alias, behind a shared lock so the settings API
/// can swap the whole set live. Read once per turn in
/// [`crate::sessions::session_actor::SessionActor::ensure_agent`]; the guard is
/// never held across an `.await`.
pub type SharedProviderRegistry = Arc<RwLock<HashMap<String, Arc<dyn LlmProvider>>>>;

/// Runtime vendors keyed by name, behind a shared lock so a settings-API vendor
/// edit can activate/reconfigure/retire a vendor without a restart. Read once
/// per provision call in [`crate::sessions::session_actor::SessionActor::vendor`].
pub type SharedVendors = Arc<RwLock<HashMap<String, Arc<WebsocketRuntimeVendor>>>>;

/// A session's unique id (a UUID string). Equals the agent session uuid, so
/// `session/<id>` and `agent/<id>` journals share the same `<id>`.
pub type SessionId = String;

/// Agent settings supplied at session creation. Storage copy of the wire
/// `horsie_models::session::AgentSettings`, with defaults applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSettings {
    pub model: String,
    pub allowed_tools: Option<Vec<String>>,
    pub use_plugins: Option<bool>,
    pub max_iterations: Option<u32>,
    pub max_retries: u32,
    /// Enabled MCP servers this session may call (by name); tools appear as
    /// `mcp__<name>__<tool>`. Empty → none. `#[serde(default)]` so pre-MCP
    /// journal rows deserialize.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// Memory spaces this session may read and write. Empty → the memory tools
    /// are not offered and no index is injected. `#[serde(default)]` so
    /// pre-memory journal rows deserialize.
    #[serde(default)]
    pub memory_spaces: Vec<String>,
    /// Canonical thinking effort chosen at session creation. `#[serde(default)]`
    /// so pre-thinking journal rows deserialize.
    #[serde(default)]
    pub thinking_effort: Option<String>,
    /// Cap on concurrently-active subagents. `#[serde(default)]` so
    /// pre-subagent journal rows deserialize; `None` resolves to
    /// [`crate::sessions::subagents::DEFAULT_MAX_CONCURRENT_SUBAGENTS`].
    #[serde(default)]
    pub max_concurrent_subagents: Option<u32>,
}

impl AgentSettings {
    /// The session's effective concurrency cap.
    pub fn max_subagents(&self) -> u32 {
        self.max_concurrent_subagents
            .unwrap_or(crate::sessions::subagents::DEFAULT_MAX_CONCURRENT_SUBAGENTS)
    }
}

/// One session workspace as persisted: just a name — the directory is always
/// vendor-allocated. Storage twin of the vendor layer's `WorkspaceSpec`. Old
/// journal rows carrying a `path` field still deserialize (the extra field is
/// ignored), so recovered sessions load without migration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceDef {
    pub name: String,
}

/// One provision step as persisted (storage twin of the wire `ProvisionStep`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvisionStepSpec {
    pub name: String,
    pub uses: String,
    pub with: Vec<(String, String)>,
}

/// One environment variable as persisted (storage twin of the wire `EnvVar`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvVarSpec {
    pub name: String,
    pub value: String,
}

/// What asked for a session to exist. More than a label: it decides whether
/// the session appears in the session list, whose run list it appears in
/// instead, and — because a routine's runs have nobody watching them — whether
/// the agent is offered the `ask_user` tool at all. Keeping all three answers
/// on one value is what stops them disagreeing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionOrigin {
    /// Created by a person, through the UI or the sessions API.
    #[default]
    User,
    /// Created by a routine trigger — timer, run endpoint, or the UI button.
    Routine { routine: String },
    /// A run of a workflow. Unlike a routine's, these sessions stay in the
    /// ordinary session list, annotated with the workflow they came from.
    Workflow { workflow: String },
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
    /// Runtime vendor name (key into [`ServerDeps::vendors`]).
    pub vendor: String,
    /// Selected plugin-bundle names to provision for this session. Resolved to
    /// current artifact hashes at each create/attach (latest-at-start); the
    /// runtime fetches them into its plugins dir before scanning.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// What asked for this session. `#[serde(default)]` so every pre-routines
    /// journal row loads as [`SessionOrigin::User`].
    #[serde(default)]
    pub origin: SessionOrigin,
    /// The workflow this session is a run of, snapshotted at creation:
    /// the graph and each step's resolved preset. `None` for every ordinary
    /// session, which is what makes the field additive.
    #[serde(default)]
    pub workflow: Option<Arc<crate::sessions::workflow::WorkflowRunSpec>>,
    /// The predefined environment this session was created from. Provenance
    /// only — everything it contributed is resolved into the fields above, so
    /// nothing re-reads it. `None` for an ad-hoc environment.
    #[serde(default)]
    pub environment: Option<String>,
    /// Environment variables injected into the runtime child, from the
    /// environment. Snapshotted with the rest.
    #[serde(default)]
    pub env_vars: Vec<EnvVarSpec>,
}

impl SessionSpec {
    /// The routine this session is a run of, if any.
    pub fn routine(&self) -> Option<&str> {
        match &self.origin {
            SessionOrigin::User | SessionOrigin::Workflow { .. } => None,
            SessionOrigin::Routine { routine } => Some(routine),
        }
    }

    /// The workflow this session is a run of, if any.
    pub fn workflow_name(&self) -> Option<&str> {
        match &self.origin {
            SessionOrigin::User | SessionOrigin::Routine { .. } => None,
            SessionOrigin::Workflow { workflow } => Some(workflow),
        }
    }

    /// Whether nobody is watching this session. An unattended session is not
    /// offered `ask_user`: a question it asked would park the run forever.
    pub fn is_unattended(&self) -> bool {
        self.routine().is_some()
    }
}

/// User-visible lifecycle state. Failure reasons ride inside the variants;
/// [`status_kind`]/[`status_reason`] project them onto the wire shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub enum SessionStatus {
    /// The runtime is being built and nothing may run yet. Where a session
    /// starts its life, and the one status that is *not* reached by a turn: it
    /// is journaled by the session's own `ProvisioningStarted` and left by the
    /// event that records how the create ended.
    ///
    /// A session found in this state at load was interrupted mid-provision, so
    /// it is safe to re-attempt — no turn can have run under it.
    Provisioning,
    /// Loaded and not working. The resting state, and where a session lands
    /// after a turn ends, is stopped, or is found interrupted at load.
    #[default]
    Idle,
    Running,
    /// Parked on one or more questions.
    ///
    /// Carries none of them: the questions belong to the agent that asked, are
    /// entries in that agent's log, and are answered through it. A status is
    /// what badges a session in a list, and a list does not render questions.
    AwaitingInput,
    /// The last turn failed. Sticky so the UI can badge it, but fully
    /// recoverable: the next turn moves it back to `Running`.
    Failed {
        reason: String,
    },
    /// The create failed on something retryable — an offline vendor, a token
    /// that could not be minted. Distinct from [`SessionStatus::Failed`], which
    /// looks the same to a reader and means the opposite to the session: a
    /// failed turn *has* a runtime and can simply run again, while this one has
    /// none and must build one first.
    ///
    /// Safe to re-attempt for the same reason `Provisioning` is: a session whose
    /// create never succeeded has never run a turn, so there is no work in a
    /// workspace for a rebuild to destroy.
    ProvisioningFailed {
        reason: String,
    },
    /// Terminal. The session can never run again — today only because its
    /// runtime is gone and re-provisioning would silently destroy work.
    Unrecoverable {
        reason: String,
    },
}

/// Project a storage status onto its wire discriminant.
pub fn status_kind(s: &SessionStatus) -> SessionStatusKind {
    match s {
        SessionStatus::Provisioning => SessionStatusKind::Provisioning,
        SessionStatus::Idle => SessionStatusKind::Idle,
        SessionStatus::Running => SessionStatusKind::Running,
        SessionStatus::AwaitingInput => SessionStatusKind::AwaitingInput,
        // Deliberately the same wire discriminant as a failed turn: to a
        // reader both are "it did not work, the reason is in `last_error`, send
        // again". What differs is what *sending again* does, and that is the
        // session's business, not the client's.
        SessionStatus::Failed { .. } | SessionStatus::ProvisioningFailed { .. } => {
            SessionStatusKind::Failed
        }
        SessionStatus::Unrecoverable { .. } => SessionStatusKind::Unrecoverable,
    }
}

/// The failure reason a status carries, if any.
pub fn status_reason(s: &SessionStatus) -> Option<String> {
    match s {
        SessionStatus::Unrecoverable { reason }
        | SessionStatus::Failed { reason }
        | SessionStatus::ProvisioningFailed { reason } => Some(reason.clone()),
        SessionStatus::Provisioning
        | SessionStatus::Idle
        | SessionStatus::Running
        | SessionStatus::AwaitingInput => None,
    }
}

/// Process-wide dependencies injected into every [`crate::sessions::session_actor::SessionActor`].
#[derive(Clone)]
pub struct ServerDeps {
    /// Runtime lifecycle, owned server-side. The actors ask it for a client
    /// and never learn how one comes to exist.
    pub runtimes: Arc<crate::runtime_manager::RuntimeManager>,
    /// LLM providers keyed by the session's `model`, swappable at runtime.
    pub provider_registry: SharedProviderRegistry,
    /// Runtime vendors keyed by the session spec's `vendor` name.
    pub vendors: SharedVendors,
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
    /// Reads and writes the agent's long-term memories, and renders the index
    /// injected into the system prompt; `None` when no memory service is wired
    /// (tests). A session that names spaces with no service configured gets no
    /// memory tools.
    pub memory: Option<Arc<crate::memory::MemoryService>>,
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
        // Old rows carried a now-removed `path`; it must still load (ignored).
        let old = r#"{"name":"api","path":"/home/u/api"}"#;
        let w: WorkspaceDef = serde_json::from_str(old).unwrap();
        assert_eq!(w.name, "api");
        let managed = r#"{"name":"main"}"#;
        let w: WorkspaceDef = serde_json::from_str(managed).unwrap();
        assert_eq!(w.name, "main");
    }

    #[test]
    fn session_spec_reads_old_journal_shape() {
        // Old rows carried now-removed `plugins_dir`/`hook_path` (the legacy
        // filesystem plugin library) and `capabilities` (the server-authored
        // sandbox spec); they must still load (ignored).
        let spec = SessionSpec {
            name: None,
            agent: AgentSettings {
                model: "m".into(),
                allowed_tools: None,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
                memory_spaces: vec![],
                thinking_effort: None,
                max_concurrent_subagents: None,
            },
            workspaces: vec![],
            provision: vec![],
            vendor: "mock".into(),
            plugins: vec![],
            origin: SessionOrigin::User,
            workflow: None,
            environment: None,
            env_vars: vec![],
        };
        let mut row = serde_json::to_value(&spec).unwrap();
        row["plugins_dir"] = serde_json::json!("/home/u/.local/share/horsie/plugins");
        row["hook_path"] = serde_json::json!(["/usr/local/bin"]);
        row["capabilities"] = serde_json::json!({
            "network": { "type": "Block", "value": {} },
            "grants": []
        });
        let loaded: SessionSpec = serde_json::from_value(row).unwrap();
        assert_eq!(loaded, spec);
    }

    #[test]
    fn a_row_without_an_origin_loads_as_a_user_session() {
        // Every session journaled before routines existed carries no origin.
        // It must load as a user session — the alternative is a restart that
        // hides every pre-existing session from the session list.
        let row = r#"{"name":null,"agent":{"model":"m","allowed_tools":null,
            "use_plugins":null,"max_iterations":null,"max_retries":0},
            "workspaces":[],"vendor":"mock"}"#;
        let spec: SessionSpec = serde_json::from_str(row).unwrap();
        assert_eq!(spec.origin, SessionOrigin::User);
        assert_eq!(spec.routine(), None);
        assert!(!spec.is_unattended());
    }

    #[test]
    fn a_routine_origin_round_trips_and_reads_unattended() {
        let spec = SessionSpec {
            name: None,
            agent: AgentSettings {
                model: "m".into(),
                allowed_tools: None,
                use_plugins: None,
                max_iterations: None,
                max_retries: 0,
                mcp_servers: vec![],
                memory_spaces: vec![],
                thinking_effort: None,
                max_concurrent_subagents: None,
            },
            workspaces: vec![],
            provision: vec![],
            vendor: "mock".into(),
            plugins: vec![],
            origin: SessionOrigin::Routine {
                routine: "nightly".into(),
            },
            workflow: None,
            environment: None,
            env_vars: vec![],
        };
        let loaded: SessionSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(loaded, spec);
        assert_eq!(loaded.routine(), Some("nightly"));
        assert!(loaded.is_unattended());
    }

    #[test]
    fn agent_settings_default_max_subagents_and_read_old_rows() {
        // Pre-subagent journal rows carry no field; they must still load and
        // resolve to the built-in default.
        let old = r#"{"model":"m","allowed_tools":null,"use_plugins":null,"max_iterations":null,"max_retries":0}"#;
        let s: AgentSettings = serde_json::from_str(old).unwrap();
        assert_eq!(s.max_concurrent_subagents, None);
        assert_eq!(
            s.max_subagents(),
            crate::sessions::subagents::DEFAULT_MAX_CONCURRENT_SUBAGENTS
        );

        let s = AgentSettings {
            max_concurrent_subagents: Some(3),
            ..serde_json::from_str::<AgentSettings>(old).unwrap()
        };
        assert_eq!(s.max_subagents(), 3);
    }

    #[test]
    fn status_kind_and_reason_project_failures() {
        let s = SessionStatus::Unrecoverable {
            reason: "gone".into(),
        };
        assert_eq!(status_kind(&s), SessionStatusKind::Unrecoverable);
        assert_eq!(status_reason(&s).as_deref(), Some("gone"));
        assert_eq!(status_reason(&SessionStatus::Idle), None);
    }
}
