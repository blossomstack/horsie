use actor::Journal;
use agentcore::LlmProvider;
use models::capabilities::CapabilitySpec;
use models::workflow::WorkflowDefinition;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A job's unique id (a UUID string). Equals the underlying workflow run id, so
/// `actors/job/<id>` and `actors/workflow/<id>` share the same `<id>`.
pub type JobId = String;

/// Persisted, self-contained description of one job. STORAGE type (lives in the
/// supervisor journal) — distinct from the daemon wire `SubmitRequest`. Carrying
/// the resolved capability spec inline makes the journal the single source of
/// truth, replacing the old `runs/<id>/manifest.json` + `capabilities.json`
/// sidecar files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub workflow: WorkflowDefinition,
    /// Display name for `job list` (usually the workflow file stem).
    pub workflow_name: String,
    /// The named workspace roots this job runs against (≥1). Names derived at submit.
    pub workspaces: Vec<models::Workspace>,
    pub input: String,
    /// Already resolved (`~`/`$HOME` expanded) at submit time.
    pub capabilities: CapabilitySpec,
    /// Shared plugin library root (`horsie_shared`), if any plugins are installed.
    #[serde(default)]
    pub plugins_dir: Option<PathBuf>,
    /// Directories prepended to PATH when running plugin hooks (resolved at submit).
    #[serde(default)]
    pub hook_path: Vec<PathBuf>,
    /// Per-run hackamore policy (the `--hackamore-policy` doc, parsed at submit). `Some`
    /// → the daemon mints a policy-bound proxy token for this job at spawn (fail
    /// closed); `None` → the job runs with no hackamore provisioning, exactly as a
    /// job runs today. The generated `models::daemon` wire type is reused as the
    /// stored shape (like [`CapabilitySpec`]); the inner hackamore `policy` stays an
    /// opaque value horsie forwards verbatim.
    #[serde(default)]
    pub hackamore_policy: Option<models::daemon::HackamoreRunPolicy>,
}

/// Shared, process-wide dependencies the production [`crate::ProcessJobRuntime`]
/// injects into every job's executor assembly.
#[derive(Clone)]
pub struct SupervisorDeps {
    /// LLM providers keyed by the `model` field of a workflow agent.
    pub provider_registry: HashMap<String, Arc<dyn LlmProvider>>,
    /// Path to the sibling `horsie-runtime` binary.
    pub runtime_bin: PathBuf,
    /// State dir for ephemeral per-job runtime files; job capability files are
    /// written under `<state_dir>/jobs/<id>/`.
    pub state_dir: PathBuf,
    /// The shared journal; the same `Arc` backs the supervisor, jobs, workflows,
    /// and agents so every actor recovers from one event store.
    pub journal: Arc<dyn Journal>,
    /// Hackamore server location (deployment-global): `Some` lets jobs that carry a
    /// per-run `models::daemon::HackamoreRunPolicy` mint a policy-bound proxy token
    /// at spawn (fail closed). `None`, or a job with no policy, spawns exactly as
    /// before. The minter holds only the admin/proxy URLs — the policy and TTL
    /// travel per-run on the [`JobSpec`].
    pub hackamore: Option<crate::hackamore::HackamoreMinter>,
}
