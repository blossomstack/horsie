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

/// Where an agent's settings came from.
///
/// The settings themselves are present either way — a preset is flattened at
/// creation, and that snapshotting is what stops an edit from reshaping a run
/// already under way. This records only *which* preset that flattening came
/// from, which is what lets a run be found again afterwards.
///
/// Deliberately **not** `#[serde(default)]` on the field that holds it.
/// Defaulting to `AdHoc` would relabel every agent that predates this as
/// having been configured inline — the one untruth this type exists to
/// prevent, and one that would outlive every session carrying it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentSource {
    /// Supplied inline at creation: a session started without naming a preset.
    AdHoc,
    /// Resolved from a saved preset.
    Preset { name: String },
}

impl AgentSource {
    /// The preset this ran under, if it ran under one.
    #[must_use]
    pub fn preset(&self) -> Option<&str> {
        match self {
            Self::AdHoc => None,
            Self::Preset { name } => Some(name),
        }
    }
}

/// Agent settings supplied at session creation. Storage copy of the wire
/// `horsie_models::session::AgentSettings`, with defaults applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Which preset these settings were flattened from, or that they were not.
    ///
    /// Every agent kind carries one, because `SessionActor::effective_settings`
    /// resolves this same struct for a main agent, a workflow step, a subagent
    /// and a sub session alike. Today a subagent inherits its parent's, which
    /// is honest — it really does run under that preset — and leaves room for a
    /// subagent to name its own later without anything else changing.
    pub source: AgentSource,
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
    /// Canonical thinking effort chosen at session creation.
    /// `#[serde(default)]` so pre-thinking journal rows deserialize.
    #[serde(default)]
    pub thinking_effort: Option<String>,
    /// Cap on concurrently-active subagents. `#[serde(default)]` so
    /// pre-subagent journal rows deserialize; `None` resolves to
    /// [`crate::sessions::run_forest::DEFAULT_MAX_CONCURRENT_SUBAGENTS`].
    #[serde(default)]
    pub max_concurrent_subagents: Option<u32>,
    /// Standing instructions this session's agent runs under, resolved from its
    /// preset at creation and snapshotted here like everything else a preset
    /// contributes. `#[serde(default)]` so pre-instruction journal rows
    /// deserialize.
    #[serde(default)]
    pub instructions: Option<String>,
    /// The plugin bundles *this agent* runs with, resolved from its preset.
    ///
    /// Per agent rather than per session, which is what lets a workflow step
    /// run with its own skills. It used to be session-wide: every step's
    /// bundles were unioned at run creation and installed once, so a step got
    /// its siblings' skills as well as its own and could not be given fewer.
    /// `#[serde(default)]` so pre-per-agent journal rows deserialize — they
    /// come back empty, and an empty set is now honestly "this agent has no
    /// bundles" rather than a stand-in for the session's.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// Whether this session compacts automatically once its context fills.
    /// `#[serde(default)]` so pre-compaction journal rows deserialize; `None`
    /// means yes, so every existing session gains the behaviour.
    #[serde(default)]
    pub auto_compact: Option<bool>,
}

impl AgentSettings {
    /// The session's effective concurrency cap.
    pub fn max_subagents(&self) -> u32 {
        self.max_concurrent_subagents
            .unwrap_or(crate::sessions::run_forest::DEFAULT_MAX_CONCURRENT_SUBAGENTS)
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
    /// A session: one main agent under these settings, and its sub sessions.
    ///
    /// Boxed because an enum is as big as its widest arm and the other arm
    /// holds an `Arc`: unboxed, every `SessionKind` anywhere — including the
    /// workflow runs that carry none of this — would be the size of a whole
    /// `AgentSettings`. Same reason `SessionDomainEvent::SpecRecorded` boxes
    /// its spec.
    Agent { settings: Box<AgentSettings> },
    /// A run of a workflow. No main agent — the definition decides who runs —
    /// and each step carries its own settings in the snapshot.
    Workflow {
        run: Arc<crate::sessions::workflow::WorkflowRunSpec>,
    },
}

/// A runtime's identity, minted when one is asked for.
///
/// Its own value rather than the session's id, which is what it used to be.
/// A session owns several runtimes now — its own, and one per sub session that
/// asked for a different environment — so a name that *was* the session's could
/// only ever address the first of them.
///
/// Opaque to everything below this layer: the vendor names its object after it,
/// the dial token claims it, and the bus builds the runtime's topics from it,
/// none of which ever needed it to be a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RuntimeId(pub uuid::Uuid);

impl RuntimeId {
    #[must_use]
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl std::fmt::Display for RuntimeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One runtime, as the session that owns it persists it: everything a vendor
/// needs to build the sandbox, resolved once at the moment it was asked for.
///
/// Its own type rather than fields on [`SessionSpec`] because a session owns
/// *several* of these — its own and one per sub session that asked for a
/// different environment — and a runtime's identity has to be able to differ
/// from the session's for that to be sayable at all.
///
/// A snapshot, never a reference: `environment` is provenance only. A session
/// revived next week gets what it was created with rather than what a
/// since-edited environment now says, which is the same rule the session spec
/// has always followed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeEnv {
    /// Runtime vendor name (key into [`RuntimeVendorMap`]).
    pub vendor: String,
    pub workspaces: Vec<WorkspaceDef>,
    /// Setup steps the runtime runs at every create/attach (idempotent).
    #[serde(default)]
    pub provision: Vec<ProvisionStepSpec>,
    /// Environment variables injected into the runtime child.
    #[serde(default)]
    pub env_vars: Vec<EnvVarSpec>,
    /// The predefined environment this was resolved from. Provenance only —
    /// everything it contributed is in the fields above, so nothing re-reads
    /// it. `None` for an ad-hoc environment.
    #[serde(default)]
    pub environment: Option<String>,
}

/// Persisted, self-contained description of one session (lives in the
/// supervisor journal, like the daemon's `JobSpec`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub kind: SessionKind,
    /// What this session's *own* runtime is built from — the one its main agent
    /// runs on.
    ///
    /// `None` means it runs without a sandbox at all: no runtime tools, no
    /// plugin skills, no hooks. An `Option` rather than an empty vendor,
    /// because a session with nowhere to run tools is a legitimate thing to ask
    /// for and a sentinel would leave every reader deciding for itself what
    /// counts as absent.
    ///
    /// Only the session's own. A sub session that asked for an environment of
    /// its own gets a runtime record instead; this is the seed for the first
    /// record, not a registry of them.
    #[serde(default)]
    pub runtime: Option<RuntimeEnv>,
    /// Selected plugin-bundle names to provision for this session. Resolved to
    /// current artifact hashes at each create/attach (latest-at-start); the
    /// runtime fetches them into its plugins dir before scanning.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// What asked for this session. `#[serde(default)]` so every pre-routines
    /// journal row loads as [`SessionOrigin::User`].
    #[serde(default)]
    pub origin: SessionOrigin,
}

impl SessionSpec {
    /// A minimal spec naming one vendor, for tests that only care which vendor
    /// a call is routed to.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn for_vendor(vendor: &str) -> Self {
        Self {
            kind: SessionKind::Agent {
                settings: Box::new(AgentSettings {
                    source: AgentSource::AdHoc,
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
                    plugins: Vec::new(),
                }),
            },
            runtime: Some(RuntimeEnv {
                vendor: vendor.to_string(),
                workspaces: vec![],
                provision: vec![],
                env_vars: vec![],
                environment: None,
            }),
            plugins: vec![],
            origin: SessionOrigin::User,
        }
    }

    /// A minimal spec with no runtime at all, for tests about the sessions that
    /// run without one.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn runtime_less() -> Self {
        let mut spec = Self::for_vendor("unused");
        spec.runtime = None;
        spec
    }

    /// What this session's own runtime is built from, if it has one.
    #[must_use]
    pub fn runtime_env(&self) -> Option<RuntimeEnv> {
        self.runtime.clone()
    }

    /// The vendor this session's own runtime is built by, if it has one.
    #[must_use]
    pub fn vendor(&self) -> Option<&str> {
        self.runtime.as_ref().map(|r| r.vendor.as_str())
    }

    /// The predefined environment this session was created from, if any.
    /// Provenance only — everything it contributed is already resolved.
    #[must_use]
    pub fn environment(&self) -> Option<&str> {
        self.runtime.as_ref().and_then(|r| r.environment.as_deref())
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
    /// graph and each step's resolved preset. `None` for every session.
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
    /// Not terminal for the session: a retry, a sub session or a new message
    /// moves it back to `Running`. `Unrecoverable` is the only status a
    /// session cannot leave. Unreachable for a session, which is never over.
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
    /// Safe to re-attempt for the same reason `Provisioning` is: a session
    /// whose create never succeeded has never run a turn, so there is no work
    /// in a workspace for a rebuild to destroy.
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

/// Process-wide dependencies injected into every
/// [`crate::sessions::session_actor::SessionActor`].
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
    /// The images and documents this session's messages carry. `None` in a
    /// test deployment with no artifact service, which shows the model
    /// nothing rather than failing a turn.
    pub artifacts: Option<Arc<crate::artifacts::ArtifactService>>,
    /// Which project this session belongs to, for scoping artifact reads.
    pub project: crate::projects::ProjectId,
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
            source: AgentSource::AdHoc,
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
            plugins: Vec::new(),
        }
    }

    pub(super) fn agent_spec(vendor: &str, origin: SessionOrigin) -> SessionSpec {
        SessionSpec {
            kind: SessionKind::Agent {
                settings: Box::new(agent_settings()),
            },
            runtime: Some(RuntimeEnv {
                vendor: vendor.into(),
                workspaces: vec![],
                provision: vec![],
                env_vars: vec![],
                environment: None,
            }),
            plugins: vec![],
            origin,
        }
    }

    pub(super) fn workflow_spec(vendor: &str, workflow: &str) -> SessionSpec {
        SessionSpec {
            kind: SessionKind::Workflow {
                run: Arc::new(crate::sessions::workflow::WorkflowRunSpec {
                    workflow: workflow.into(),
                    start: "triage".into(),
                    steps: vec![],
                    input: "in".into(),
                    max_steps: 10,
                }),
            },
            runtime: Some(RuntimeEnv {
                vendor: vendor.into(),
                workspaces: vec![],
                provision: vec![],
                env_vars: vec![],
                environment: None,
            }),
            plugins: vec![],
            origin: SessionOrigin::User,
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
        //
        // The row still carries `control_plane`, dropped in 0039. Left in on
        // purpose: an unknown key must stay ignorable, or every session
        // journaled before the tool selection would fail to load.
        //
        // `source` is present because it is required — the one field here that
        // is. See `settings_without_a_source_are_refused_rather_than_assumed_ad_hoc`.
        let row = r#"{"name":null,"kind":{"Agent":{"settings":{"model":"m",
            "source":"AdHoc",
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

    /// The kind is the whole of what differs between a session and a run:
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
        // Pre-subagent journal rows carry no `max_concurrent_subagents`; they
        // must still load and resolve to the built-in default. `source` is
        // present because it is required — see the test below.
        let old = r#"{"source":"AdHoc","model":"m","allowed_tools":null,"use_plugins":null,"max_iterations":null,"max_retries":0}"#;
        let s: AgentSettings = serde_json::from_str(old).unwrap();
        assert_eq!(s.max_concurrent_subagents, None);
        assert_eq!(
            s.max_subagents(),
            crate::sessions::run_forest::DEFAULT_MAX_CONCURRENT_SUBAGENTS
        );

        let s = AgentSettings {
            source: AgentSource::AdHoc,
            max_concurrent_subagents: Some(3),
            ..serde_json::from_str::<AgentSettings>(old).unwrap()
        };
        assert_eq!(s.max_subagents(), 3);
    }

    /// `source` is the one field here without `#[serde(default)]`, and that is
    /// the decision this pins.
    ///
    /// Every other field defaults so an old journal row loads. This one must
    /// not: `AdHoc` would be a claim about how those agents were configured,
    /// asserted on rows that never said, and it would stay wrong for as long as
    /// the session lives — including in the index a tuning agent reads. A row
    /// written before this field is instead refused, and the journal is reset
    /// as part of shipping it.
    ///
    /// If this test ever starts failing because someone added a default,
    /// that is the bug, not the test.
    #[test]
    fn settings_without_a_source_are_refused_rather_than_assumed_ad_hoc() {
        let no_source = r#"{"model":"m","allowed_tools":null,"use_plugins":null,"max_iterations":null,"max_retries":0}"#;
        assert!(
            serde_json::from_str::<AgentSettings>(no_source).is_err(),
            "a row that never named a source must not load as AdHoc"
        );
    }

    #[test]
    fn a_source_round_trips_and_names_its_preset() {
        let preset = AgentSource::Preset {
            name: "reviewer".into(),
        };
        assert_eq!(preset.preset(), Some("reviewer"));
        assert_eq!(AgentSource::AdHoc.preset(), None);

        let json = serde_json::to_string(&preset).unwrap();
        assert_eq!(
            serde_json::from_str::<AgentSource>(&json).unwrap(),
            preset,
            "the journal reads back what it wrote"
        );
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
