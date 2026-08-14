//! STORAGE types for sessions (journal-owned). Distinct from the fluorite wire
//! types in `horsie_models::session` — wire formats evolve at the speed of the
//! API contract, these evolve at the speed of data migrations.

use horsie_agentcore::LlmProvider;
use horsie_models::session::SessionStatusKind;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// LLM providers keyed by model alias, behind a shared lock so the settings API
/// can swap the whole set live. Read once per turn in
/// [`crate::sessions::session_actor::SessionActor::ensure_agent`]; the guard is
/// never held across an `.await`.
pub type SharedProviderRegistry = Arc<RwLock<HashMap<String, ModelEntry>>>;

/// One configured model: how to talk to it, and how much room it has.
///
/// The window rides here rather than in a second map because the two are
/// rebuilt from the same settings at the same instant, and a parallel map is a
/// thing that can disagree with this one. `None` means the card declares no
/// window, which is what disables automatic compaction for a session on it.
#[derive(Clone)]
pub struct ModelEntry {
    pub provider: Arc<dyn LlmProvider>,
    pub context_window: Option<u32>,
}

impl ModelEntry {
    /// A model with no declared window, so sessions on it never compact
    /// automatically. What a test wants unless it is testing compaction — a
    /// budget would otherwise change how many provider calls a run makes.
    #[must_use]
    pub fn provider_only(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            context_window: None,
        }
    }
}

/// Runtime vendors keyed by name, behind a shared lock so a settings edit can
/// activate, reconfigure or retire one without a restart. Read once per
/// provision call in [`crate::sessions::session_actor::SessionActor::vendor`].
///
/// Not "Shared": since per-account services there is one of these per account,
/// so a name saying "shared" would read as deployment-wide — the opposite of
/// what it is, on the type where being wrong means running tool calls on
/// someone else's machine.
pub type RuntimeVendorMap =
    Arc<RwLock<HashMap<String, Arc<dyn crate::runtime_vendor::RuntimeVendor>>>>;

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
    /// Standing instructions this session's agent runs under, resolved from its
    /// preset at creation and snapshotted here like everything else a preset
    /// contributes. `#[serde(default)]` so pre-instruction journal rows
    /// deserialize.
    #[serde(default)]
    pub instructions: Option<String>,
    /// Whether this session compacts automatically once its context fills.
    /// `#[serde(default)]` so pre-compaction journal rows deserialize; `None`
    /// means yes, so every existing session gains the behaviour.
    #[serde(default)]
    pub auto_compact: Option<bool>,
    /// Whether this session's main agent may manage the horsie server itself.
    /// `#[serde(default)]` so journal rows written before the control plane
    /// deserialize; `None` means no, because authority is granted and never
    /// acquired by age.
    #[serde(default)]
    pub control_plane: Option<bool>,
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
///
/// A workflow run is not an origin: it is what the session *is*, which
/// [`SessionKind::Workflow`] says structurally, so it carries `User` here and
/// stays in the ordinary session list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionOrigin {
    /// Created by a person, through the UI or the sessions API.
    #[default]
    User,
    /// Created by a routine trigger — timer, run endpoint, or the UI button.
    Routine { routine: String },
}

/// What a session is. A sum type rather than an `agent` field plus an
/// optional `workflow`: the old pairing let a run carry a session-wide
/// `AgentSettings` that nothing in it used — the steps own their own — and
/// every session-shaped reader then reported the start step's model as the
/// session's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SessionKind {
    /// A conversation: one main agent under these settings, and its forks.
    Agent { settings: AgentSettings },
    /// A run of a workflow. No main agent — the definition decides who runs —
    /// and each step carries its own settings in the snapshot.
    Workflow {
        run: Arc<crate::sessions::workflow::WorkflowRunSpec>,
    },
}

/// Persisted, self-contained description of one session (lives in the
/// supervisor journal, like the daemon's `JobSpec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub name: Option<String>,
    pub kind: SessionKind,
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
    /// A minimal spec naming one vendor, for tests that only care which vendor
    /// a call is routed to.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_vendor(vendor: &str) -> Self {
        Self {
            name: None,
            kind: SessionKind::Agent {
                settings: AgentSettings {
                    instructions: None,
                    model: "m".into(),
                    allowed_tools: None,
                    use_plugins: None,
                    max_iterations: None,
                    max_retries: 0,
                    mcp_servers: vec![],
                    memory_spaces: vec![],
                    thinking_effort: None,
                    max_concurrent_subagents: None,
                    auto_compact: None,
                    control_plane: None,
                },
            },
            workspaces: vec![],
            provision: vec![],
            vendor: vendor.to_string(),
            plugins: vec![],
            origin: SessionOrigin::User,
            environment: None,
            env_vars: vec![],
        }
    }

    /// The routine this session is a run of, if any.
    pub fn routine(&self) -> Option<&str> {
        match &self.origin {
            SessionOrigin::User => None,
            SessionOrigin::Routine { routine } => Some(routine),
        }
    }

    /// The main agent's settings, for the session kinds that have one.
    pub fn agent_settings(&self) -> Option<&AgentSettings> {
        match &self.kind {
            SessionKind::Agent { settings } => Some(settings),
            SessionKind::Workflow { .. } => None,
        }
    }

    /// The workflow this session is a run of, snapshotted at creation: the
    /// graph and each step's resolved preset. `None` for every conversation.
    pub fn workflow_run(&self) -> Option<&Arc<crate::sessions::workflow::WorkflowRunSpec>> {
        match &self.kind {
            SessionKind::Agent { .. } => None,
            SessionKind::Workflow { run } => Some(run),
        }
    }

    /// The workflow this session is a run of, if any.
    pub fn workflow_name(&self) -> Option<&str> {
        self.workflow_run().map(|run| run.workflow.as_str())
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
    /// A workflow run reached a terminal step with no error.
    ///
    /// Not terminal for the session: a retry, a fork or a new message moves it
    /// back to `Running`. `Unrecoverable` is the only status a session cannot
    /// leave. Unreachable for a conversation, which is never over.
    Finished,
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
        SessionStatus::Finished => SessionStatusKind::Finished,
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
        | SessionStatus::Finished
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
    pub vendors: RuntimeVendorMap,
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
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    pub(super) fn agent_settings() -> AgentSettings {
        AgentSettings {
            instructions: None,
            model: "m".into(),
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: vec![],
            memory_spaces: vec![],
            thinking_effort: None,
            max_concurrent_subagents: None,
            auto_compact: None,
            control_plane: None,
        }
    }

    pub(super) fn agent_spec(vendor: &str, origin: SessionOrigin) -> SessionSpec {
        SessionSpec {
            name: None,
            kind: SessionKind::Agent {
                settings: agent_settings(),
            },
            workspaces: vec![],
            provision: vec![],
            vendor: vendor.into(),
            plugins: vec![],
            origin,
            environment: None,
            env_vars: vec![],
        }
    }

    pub(super) fn workflow_spec(vendor: &str, workflow: &str) -> SessionSpec {
        SessionSpec {
            name: None,
            kind: SessionKind::Workflow {
                run: Arc::new(crate::sessions::workflow::WorkflowRunSpec {
                    workflow: workflow.into(),
                    start: "triage".into(),
                    steps: vec![],
                    input: "in".into(),
                    max_steps: 10,
                }),
            },
            workspaces: vec![],
            provision: vec![],
            vendor: vendor.into(),
            plugins: vec![],
            origin: SessionOrigin::User,
            environment: None,
            env_vars: vec![],
        }
    }

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
        let spec = agent_spec("mock", SessionOrigin::User);
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
        let row = r#"{"name":null,"kind":{"Agent":{"settings":{"model":"m",
            "allowed_tools":null,"use_plugins":null,"max_iterations":null,
            "max_retries":0,"mcp_servers":[],"memory_spaces":[],
            "thinking_effort":null,"max_concurrent_subagents":null,
            "instructions":null,"auto_compact":null,"control_plane":null}}},
            "workspaces":[],"vendor":"mock"}"#;
        let spec: SessionSpec = serde_json::from_str(row).unwrap();
        assert_eq!(spec.origin, SessionOrigin::User);
        assert_eq!(spec.routine(), None);
        assert!(!spec.is_unattended());
    }

    #[test]
    fn a_routine_origin_round_trips_and_reads_unattended() {
        let spec = agent_spec(
            "mock",
            SessionOrigin::Routine {
                routine: "nightly".into(),
            },
        );
        let loaded: SessionSpec =
            serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(loaded, spec);
        assert_eq!(loaded.routine(), Some("nightly"));
        assert!(loaded.is_unattended());
    }

    /// The kind is the whole of what differs between a conversation and a run:
    /// the shared fields are identical, and neither shape fabricates the
    /// other's. In particular there is no workflow constructor that also
    /// carries an `AgentSettings`.
    #[test]
    fn each_kind_projects_only_its_own_settings() {
        let agent = agent_spec("mock", SessionOrigin::User);
        assert_eq!(agent.agent_settings().map(|s| s.model.as_str()), Some("m"));
        assert!(agent.workflow_run().is_none());
        assert_eq!(agent.workflow_name(), None);

        let run = workflow_spec("mock", "fix-bug");
        assert!(run.agent_settings().is_none(), "a run has no session agent");
        assert_eq!(
            run.workflow_run().map(|r| r.workflow.as_str()),
            Some("fix-bug")
        );
        assert_eq!(run.workflow_name(), Some("fix-bug"));
        assert!(!run.is_unattended(), "a run may ask the person");
    }

    #[test]
    fn both_kinds_round_trip() {
        for spec in [
            agent_spec("mock", SessionOrigin::User),
            workflow_spec("mock", "fix-bug"),
        ] {
            let loaded: SessionSpec =
                serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
            assert_eq!(loaded, spec);
        }
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
