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
    pub workdir: PathBuf,
    pub input: String,
    /// Already resolved (`~`/`$HOME` expanded) at submit time.
    pub capabilities: CapabilitySpec,
}

/// Shared, process-wide dependencies the production [`crate::ProcessJobRuntime`]
/// injects into every job's executor assembly.
#[derive(Clone)]
pub struct SupervisorDeps {
    /// LLM providers keyed by the `model` field of a workflow agent.
    pub provider_registry: HashMap<String, Arc<dyn LlmProvider>>,
    /// Path to the sibling `october-runtime` binary.
    pub runtime_bin: PathBuf,
    /// State dir for ephemeral per-job runtime files; job capability files are
    /// written under `<state_dir>/jobs/<id>/`.
    pub state_dir: PathBuf,
    /// The shared journal; the same `Arc` backs the supervisor, jobs, workflows,
    /// and agents so every actor recovers from one event store.
    pub journal: Arc<dyn Journal>,
}
